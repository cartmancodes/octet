//! The hub's network side: an authenticated WebSocket API on `127.0.0.1` for phones, and a
//! local control socket, `<run>/hub.sock`, for the CLI.
//!
//! # Connections
//!
//! A connection must finish its WebSocket upgrade within 5 seconds of being accepted, and at
//! most 32 upgrade at once (a new one lets the oldest go), so sockets that stall mid-request
//! cannot keep a phone out. The handshake turns away any request whose `Tailscale-User-Login`
//! header is not exactly the owner's login, or whose `Origin` names a host the request does not
//! (a web page of another site; the app sends none, or the hub's own), with 403, and one the hub
//! has no room for (64 open, or 8 not past `hello`) with 503. A connection then starts
//! unauthenticated and accepts only `hello` (a
//! device token) and `pair` (a pairing secret); anything else, or a failed `hello` or `pair`,
//! is refused and the socket closed, as is one silent for 30 seconds. After `hello` it is
//! authenticated as that device: every frame, text or not, first
//! checks that the device is still paired (a revoked one is told `revoked`, then closed) and takes a token
//! from the device's bucket (60 a minute; an empty bucket refuses, it does not close). Every
//! connection is pinged every 20 seconds, and one that misses two pongs is let go.
//! Bridge commands go to the session under a hub-wide req and the reply comes back under the
//! phone's own; one unanswered for 30 seconds is refused for it.
//!
//! # Subscriptions
//!
//! A subscription registers first and then replays the journals page by page in its own task,
//! which writes through the connection's replay queue and waits for room there. The replay
//! starts after the phone's cursor, or, with no cursor or one naming a journal the session does
//! not list, at the start of its first journal; its first frame, `subscribed`, says which, and
//! lists the journals. Live events for the session meanwhile wait in a capped buffer (a phone
//! that overflows it is dropped, and resumes from its cursor). When the connection's writer has
//! sent the last replayed frame it reports, for each journal, the last `seq` replayed (and the
//! cursor's, which the phone has), and the hub sends the buffered events past those, acks the
//! subscription and streams live. Buffered events the outbox has no room for, with the ack, go
//! the replay's way instead, waiting for room as a page does, and the hub looks again once they
//! are written. Frames a replay left queued for a subscription since dropped or replaced are not
//! sent, so none follows its acknowledgement. The replay reads files to their end while the tail polls, so
//! live events the replay already sent can still follow: a live event goes out only if its
//! `seq` is past the last one sent of its journal. A late record of an older journal (the old
//! session's last words after a reconnect) is past its journal's mark, so it is sent once. Each
//! replay page resumes at the byte where the last stopped. Live frames go through a queue of
//! 512: a phone that lets it fill is dropped.
use crate::{
    launch::{self, Launches, StartRequest, clip as clip_to},
    owner::published_url,
    push::{self, Pusher, Sender},
    sessions::{SessionUpdate, Sessions},
    store::{Device, SECRET_REFUSED, Store},
    tail::{self, file_name},
};
use futures_util::{SinkExt, StreamExt};
use octet_core::PROMPT_LIMIT;
use octet_core::remote::{
    protocol::{
        BridgeCommand, Code, Cursor, MAX_MESSAGE, PhoneCommand, ack, catalog_frame, event_frame,
        full_access_confirmed, paired, parse_phone, recent_frame, refused, req_of, sessions_frame,
        state_frame, subscribed_frame, welcome,
    },
    run::run_dir,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::{mpsc, oneshot, watch},
    task::AbortHandle,
    time::{Instant, Interval, MissedTickBehavior, interval_at, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{Callback, ErrorResponse, Request, Response},
        http::{HeaderMap, StatusCode},
        protocol::WebSocketConfig,
    },
};

/// Frames waiting for one `octet hub watch`; a watcher that lets it fill is dropped.
const WATCH_QUEUE: usize = 256;
/// How many `octet hub watch` clients may be attached at once.
const MAX_WATCHERS: usize = 8;
/// Live frames waiting to be written to one phone; a phone that lets it fill is dropped.
const OUTBOUND_QUEUE: usize = 512;
/// Replayed frames waiting to be written to one phone; a replay waits for room.
const REPLAY_QUEUE: usize = 64;
/// Journal records read per replay page.
const REPLAY_PAGE: usize = 500;
/// Live events held for one subscription while it replays; one more drops the phone.
const LIVE_BUFFER: usize = 2048;
/// How long one write to a phone, or a replay waiting for room, may take.
const WRITE_STALL: Duration = Duration::from_secs(30);
/// Commands one device may send per minute; also the bucket's size.
const COMMANDS_PER_MINUTE: u64 = 60;
/// Milliseconds in a minute: what one command costs in the bucket's units.
const MINUTE_MS: u64 = 60_000;
/// How long a session gets to answer a command.
const REPLY_WITHIN: Duration = Duration::from_secs(30);
/// How often `devices.json` is re-read for revoked devices.
const RELOAD_EVERY: Duration = Duration::from_secs(2);
/// How often the run directory is scanned for sessions.
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// How often unanswered commands are checked against their deadline.
const SWEEP_EVERY: Duration = Duration::from_secs(1);
/// How long a connection may take to say hello (or pair) before it is closed.
const HELLO_WITHIN: Duration = Duration::from_secs(30);
/// How long a connection may take from accept to a finished WebSocket upgrade.
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(5);
/// Upgrades under way at once; a connection accepted past it lets the oldest go.
const MAX_HANDSHAKES: usize = 32;
/// How often the hub asks Tailscale again whether it is published.
const PUBLISH_EVERY: Duration = Duration::from_secs(10);
/// How long the closing handshake may take.
const CLOSE_WITHIN: Duration = Duration::from_secs(2);
/// How long a revoked phone has to take its refusal before its connection is cut.
const REVOKED_GRACE: Duration = Duration::from_secs(1);
/// Connections open at once; more are turned away at accept.
const MAX_CONNECTIONS: usize = 64;
/// Connections that have not said hello yet; more are turned away at accept.
const MAX_WAITING: usize = 8;
/// How often a connection is pinged; one that misses two pongs is let go.
const PING_EVERY: Duration = Duration::from_secs(20);
/// Unanswered pings after which a connection is let go.
const MISSED_PONGS: u32 = 2;
/// How long accepting waits after a failure (out of descriptors, say) before trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Messages from connection tasks waiting for the hub loop.
const EVENT_QUEUE: usize = 256;
/// The longest control request, in bytes.
const CONTROL_LIMIT: u64 = 4096;
/// How long a control client may take to ask or to read the answer.
const CONTROL_WITHIN: Duration = Duration::from_secs(5);
/// The refusal for a device past its bucket.
const RATE_REFUSED: &str = "Too many commands; wait a minute";
/// The refusal sent to a phone whose device was unpaired, just before it is closed.
const REVOKED_REFUSED: &str = "This phone was unpaired on the Mac";
/// The refusal for a frame that is not text.
const NOT_TEXT: &str = "Send each message as a text frame";
/// The refusal for a command whose session ended before it answered.
const SESSION_ENDED: &str = "The session ended";
/// The refusal for a command the session did not answer in time.
const NO_ANSWER: &str = "The session didn't answer";
/// The refusal for a prompt or steer over [`PROMPT_LIMIT`].
const TOO_LONG: &str = "The text is over 64 KiB";
/// The refusal for `full-access` without the typed confirmation.
const FULL_ACCESS: &str = "Full access needs confirm: \"full access\"";
/// The refusal for a `recent` while another scan runs.
const SCAN_RUNNING: &str = "A recent-sessions scan is running; try again";
/// The refusal for a background job that took too long.
const JOB_TIMED_OUT: &str = "That took too long; try again";
/// The refusal for a push registration that could not be written.
const PUSH_NOT_SAVED: &str = "Could not save the notification setting; try again";
/// Reports of dead push tokens waiting for the hub loop.
const PUSH_QUEUE: usize = 16;
/// How long a scan or a start may take in all.
const JOB_WITHIN: Duration = Duration::from_secs(30);
/// The longest workspace path kept in an audit line, in characters.
const AUDIT_PATH: usize = 200;
/// The longest phone-chosen text (a mode or model name) kept in an audit line, in characters.
const AUDIT_TEXT: usize = 64;

/// What a connection task tells the hub loop.
enum ConnEvent {
    /// A frame arrived: its text, or `None` for one that is not text.
    Message { conn: u64, text: Option<String> },
    /// The writer sent a replay's last frame; `sent` is the last `seq` it held of each journal.
    Replayed {
        conn: u64,
        session: String,
        generation: u64,
        sent: Vec<(String, u64)>,
    },
    /// A background job (a scan or a launch) finished: `frame` is the answer for `conn`.
    Answer {
        conn: u64,
        frame: Value,
        /// Which job it was.
        job: Job,
        /// The audit line to write: device, session and what.
        audit: Option<(String, String, String)>,
    },
    /// A replay found no room for too long.
    Stalled { conn: u64 },
    /// The connection ended.
    Closed { conn: u64 },
}

/// The one running `recent` scan: holding it keeps the flag up, and dropping it, even in a scan
/// that panicked, puts it down.
#[derive(Debug)]
struct Scanning(Arc<AtomicBool>);

impl Scanning {
    /// Raises `flag` for a new scan; `None` while another runs.
    fn begin(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for Scanning {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A background job that answers a phone.
#[derive(Clone, Copy, Debug)]
enum Job {
    /// A scan for `recent`.
    Scan,
    /// A tmux launch, and whether tmux accepted it.
    Launch { accepted: bool },
}

/// What the hub loop has a connection's writer send, in order.
enum Out {
    /// A frame.
    Text(String),
    /// The closing handshake, then the end.
    Close,
}

// A frame can hold a device token, a pairing secret or a `paired` token, so the Debug output
// of what carries frames shows only their size.
impl std::fmt::Debug for ConnEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message { conn, text } => f
                .debug_struct("Message")
                .field("conn", conn)
                .field("text", &text.as_ref().map(|t| Redacted(t.len())))
                .finish(),
            Self::Replayed {
                conn,
                session,
                generation,
                sent,
            } => f
                .debug_struct("Replayed")
                .field("conn", conn)
                .field("session", session)
                .field("generation", generation)
                .field("sent", sent)
                .finish(),
            Self::Answer {
                conn,
                frame,
                job,
                audit,
            } => f
                .debug_struct("Answer")
                .field("conn", conn)
                .field("frame", &Redacted(frame.to_string().len()))
                .field("job", job)
                .field("audit", &audit.as_ref().map(|(_, session, _)| session))
                .finish(),
            Self::Stalled { conn } => f.debug_struct("Stalled").field("conn", conn).finish(),
            Self::Closed { conn } => f.debug_struct("Closed").field("conn", conn).finish(),
        }
    }
}

impl std::fmt::Debug for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(&Redacted(text.len())).finish(),
            Self::Close => f.write_str("Close"),
        }
    }
}

/// Stands in for a frame in Debug output: its length only.
struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

/// Whether a connection still answers pings.
#[derive(Debug, Default)]
struct Liveness {
    /// Pings sent since the last pong.
    unanswered: u32,
}

impl Liveness {
    /// Counts a ping about to be sent; false if too many went unanswered to send another.
    fn ping(&mut self) -> bool {
        if self.unanswered >= MISSED_PONGS {
            return false;
        }
        self.unanswered += 1;
        true
    }

    /// A pong arrived.
    fn pong(&mut self) {
        self.unanswered = 0;
    }
}

/// The connections the hub holds: every open one, and those not past `hello` yet. A handshake
/// takes a [`Seat`] before it upgrades, so a socket the hub has no room for is turned away
/// (503) rather than upgraded and then dropped, and one that never finishes its request holds
/// no place at all.
#[derive(Debug, Default)]
struct Room {
    open: AtomicUsize,
    waiting: AtomicUsize,
}

impl Room {
    /// A place for one more connection, not past `hello` yet; `None` when the hub has
    /// [`MAX_CONNECTIONS`] open or [`MAX_WAITING`] waiting.
    fn seat(self: &Arc<Self>) -> Option<Seat> {
        if self.open.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            self.open.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        if self.waiting.fetch_add(1, Ordering::AcqRel) >= MAX_WAITING {
            self.waiting.fetch_sub(1, Ordering::AcqRel);
            self.open.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Seat {
            room: Arc::clone(self),
            waiting: true,
        })
    }
}

/// One connection's place in the [`Room`], given back when it drops.
#[derive(Debug)]
struct Seat {
    room: Arc<Room>,
    /// Not past `hello` yet.
    waiting: bool,
}

impl Seat {
    /// The connection said hello: it no longer waits.
    fn said_hello(&mut self) {
        if std::mem::take(&mut self.waiting) {
            self.room.waiting.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.said_hello();
        self.room.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// An upgraded connection and its place.
type Upgraded = (WebSocketStream<TcpStream>, Seat);

/// What a replay task has a connection's writer send.
#[derive(Debug)]
enum ReplayItem {
    /// A replayed event frame, and whether its subscription is still the current one: the
    /// writer passes over a frame queued for one since dropped or replaced.
    Frame {
        frame: String,
        current: Arc<AtomicBool>,
    },
    /// The replay is over: the writer reports it once everything before it is sent.
    Done {
        session: String,
        generation: u64,
        sent: Vec<(String, u64)>,
    },
}

/// Where one subscription is.
#[derive(Debug)]
enum Subscription {
    /// Replaying, or handing over: live events wait in `buffer` as `(journal, seq, frame)`.
    /// `seen` is the last `seq` of each journal the phone has or has queued (its cursor, then
    /// what the replay sent). `current` is up while this subscription stands; its frames still
    /// queued when it goes are not sent.
    Replaying {
        generation: u64,
        req: u64,
        seen: HashMap<String, u64>,
        buffer: Vec<(String, u64, String)>,
        task: AbortHandle,
        current: Arc<AtomicBool>,
    },
    /// Streaming live. `seen` is the last `seq` the phone has of each journal; a live event at
    /// or below it is a repeat of the replay (the tail had not read it yet) and is dropped.
    Live { seen: HashMap<String, u64> },
}

impl Subscription {
    /// Whether a live event of `journal` and `seq` goes to the phone, which then has it.
    fn admit(&mut self, journal: &str, seq: u64) -> bool {
        match self {
            Self::Live { seen } => admit(seen, journal, seq),
            Self::Replaying { .. } => false,
        }
    }
}

/// Whether the phone, which has the records up to `seen` of each journal, lacks `seq` of
/// `journal`; if so it is counted as had.
fn admit(seen: &mut HashMap<String, u64>, journal: &str, seq: u64) -> bool {
    if seen.get(journal).is_some_and(|last| seq <= *last) {
        return false;
    }
    seen.insert(journal.to_owned(), seq);
    true
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Self::Replaying { task, current, .. } = self {
            current.store(false, Ordering::Release);
            task.abort();
        }
    }
}

/// One phone connection as the hub loop sees it.
#[derive(Debug)]
struct Conn {
    /// Live frames for the writer.
    out: mpsc::Sender<Out>,
    /// Given to each replay task, for the writer.
    replay: mpsc::Sender<ReplayItem>,
    /// The connection task.
    task: AbortHandle,
    /// When the connection was accepted.
    opened: Instant,
    /// The device, once `hello` succeeded.
    device: Option<Device>,
    /// The sessions it streams, by id.
    subscriptions: HashMap<String, Subscription>,
    /// Its place in the hub's room, given back as it drops.
    seat: Seat,
}

/// A command sent to a session and not yet answered.
#[derive(Debug)]
struct Pending {
    /// The connection that sent it.
    conn: u64,
    /// The phone's own req.
    req: u64,
    /// The session it went to; only that one may answer.
    session: String,
    /// When it is refused unanswered.
    deadline: Instant,
}

/// A device's command allowance: `units` (one command costs [`MINUTE_MS`]) as of `at`.
#[derive(Debug)]
struct Bucket {
    units: u64,
    at: Instant,
}

impl Bucket {
    /// A bucket holding a whole minute's commands.
    fn full(now: Instant) -> Self {
        Self {
            units: COMMANDS_PER_MINUTE * MINUTE_MS,
            at: now,
        }
    }

    /// Refills for the time since the last call, then takes one command if there is one.
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_millis();
        let elapsed = u64::try_from(elapsed).unwrap_or(u64::MAX);
        self.units = self
            .units
            .saturating_add(elapsed.saturating_mul(COMMANDS_PER_MINUTE))
            .min(COMMANDS_PER_MINUTE * MINUTE_MS);
        self.at = self.at.max(now);
        if self.units < MINUTE_MS {
            return false;
        }
        self.units -= MINUTE_MS;
        true
    }
}

/// A control-socket request the hub loop answers.
#[derive(Debug)]
enum Control {
    /// A new pairing secret.
    PairSecret(oneshot::Sender<String>),
    /// The port and the number of paired devices.
    Status(oneshot::Sender<(u16, usize)>),
    /// A watcher: the lines it is to be sent, first the rows as they stand.
    Watch(mpsc::Sender<String>),
}

/// The running hub: its files, its sessions and every phone connection.
#[derive(Debug)]
pub struct Hub {
    /// Devices, pairing secrets and the audit log.
    store: Store,
    /// The sessions the hub can reach.
    sessions: Sessions,
    /// The tailnet login every connection must come from.
    owner: String,
    /// The loopback port it listens on.
    port: u16,
    /// Phone connections by id.
    conns: HashMap<u64, Conn>,
    /// Commands sent to sessions and not yet answered, by hub req.
    pending: HashMap<u64, Pending>,
    /// Each device's command allowance, by token hash.
    buckets: HashMap<String, Bucket>,
    /// The session rows phones were last sent.
    listed: Vec<Value>,
    /// Control clients running `watch`.
    watchers: Vec<mpsc::Sender<String>>,
    /// The rows with NOW lines the watchers were last sent.
    watched: Vec<Value>,
    /// The data directory: journals, the run directory and the hub's files.
    directory: PathBuf,
    /// Launches that count toward the phone-started cap beyond the registrations.
    launches: Launches,
    /// Whether a `recent` scan is running; only one runs at a time (see [`Scanning`]).
    scanning: Arc<AtomicBool>,
    /// Given to each connection and replay task.
    events: mpsc::Sender<ConnEvent>,
    /// What to push, and when.
    pusher: Pusher,
    /// Posts pushes in the background.
    sender: Sender,
    /// The last id given to a connection, hub req or replay.
    ids: u64,
}

/// What tells the control socket whether, and where, the hub is published on the tailnet: given
/// the loopback port, the HTTPS URL `tailscale serve` shows it at, or `None`.
pub type Publisher = fn(u16) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>;

/// The real [`Publisher`]: asks `tailscale serve status`.
fn tailscale_publisher(port: u16) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> {
    Box::pin(published_url(port))
}

impl Hub {
    /// Binds `127.0.0.1:<port>` and the local control socket; runs until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// Fails if `owner` is empty, the hub's files cannot be opened, another hub answers on the
    /// control socket, or either socket cannot be bound.
    pub async fn serve(
        directory: PathBuf,
        port: u16,
        owner: String,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), String> {
        Self::serve_notify(directory, port, owner, shutdown, oneshot::channel().0).await
    }

    /// Like [`serve`](Self::serve), and sends the bound port on `ready` once the hub is
    /// answering (it is dropped unsent when the hub fails to start).
    ///
    /// # Errors
    ///
    /// The same as [`serve`](Self::serve).
    pub async fn serve_notify(
        directory: PathBuf,
        port: u16,
        owner: String,
        shutdown: impl Future<Output = ()>,
        ready: oneshot::Sender<u16>,
    ) -> Result<(), String> {
        let publishing = (tailscale_publisher as Publisher, PUBLISH_EVERY);
        Self::serve_with(directory, port, owner, shutdown, ready, publishing).await
    }

    /// Like [`serve_notify`](Self::serve_notify), asking `publisher` instead of Tailscale whether
    /// the hub is published, at start and then every `recheck` (for tests, which cannot depend
    /// on the machine's Tailscale).
    ///
    /// # Errors
    ///
    /// The same as [`serve`](Self::serve).
    pub async fn serve_with(
        directory: PathBuf,
        port: u16,
        owner: String,
        shutdown: impl Future<Output = ()>,
        ready: oneshot::Sender<u16>,
        (publisher, recheck): (Publisher, Duration),
    ) -> Result<(), String> {
        if owner.is_empty() {
            return Err("The tailnet owner's login is unknown".into());
        }
        let store = Store::open(&directory).await.map_err(|e| {
            format!(
                "Couldn't open the hub's files in {}: {e}",
                directory.display()
            )
        })?;
        let run = run_dir(&directory);
        private_dir(&run)
            .await
            .map_err(|e| format!("Couldn't prepare {}: {e}", run.display()))?;
        let control_path = run.join("hub.sock");
        let control = bind_control(&control_path).await?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| format!("Couldn't listen on 127.0.0.1:{port}: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Couldn't read the port: {e}"))?
            .port();
        let (events, mut inbox) = mpsc::channel(EVENT_QUEUE);
        let (dead_tokens, mut dead) = mpsc::channel(PUSH_QUEUE);
        let (streams, mut accepted) = mpsc::channel(16);
        let room = Arc::new(Room::default());
        let accept_task =
            tokio::spawn(accept_tcp(listener, owner.clone(), room, streams)).abort_handle();
        let (requests, mut controls) = mpsc::channel(16);
        let (published_in, published) = watch::channel(Publication::Unknown);
        let publish_task =
            tokio::spawn(watch_published(port, publisher, recheck, published_in)).abort_handle();
        let control_task = tokio::spawn(serve_control(control, requests, published)).abort_handle();
        let mut hub = Hub {
            store,
            sessions: Sessions::new(run),
            directory,
            launches: Launches::default(),
            scanning: Arc::new(AtomicBool::new(false)),
            owner,
            port,
            conns: HashMap::new(),
            pending: HashMap::new(),
            buckets: HashMap::new(),
            listed: Vec::new(),
            watchers: Vec::new(),
            watched: Vec::new(),
            events,
            pusher: Pusher::default(),
            sender: Sender::new(push::endpoint(), dead_tokens),
            ids: 0,
        };
        hub.sessions.refresh().await;
        hub.listed = hub.sessions.list();
        let (mut refresh, mut reload, mut sweep) = (
            every(REFRESH_EVERY),
            every(RELOAD_EVERY),
            every(SWEEP_EVERY),
        );
        let mut shutdown = std::pin::pin!(shutdown);
        let _ = ready.send(port);
        loop {
            // `refresh` is not cancel-safe, so it runs in a handler, never as a branch.
            tokio::select! {
                () = &mut shutdown => break,
                Some(upgraded) = accepted.recv() => hub.accept(upgraded),
                Some(event) = inbox.recv() => hub.on_event(event).await,
                update = hub.sessions.next() => hub.on_update(update),
                Some(request) = controls.recv() => hub.on_control(request).await,
                _ = refresh.tick() => hub.sessions.refresh().await,
                _ = reload.tick() => hub.reload().await,
                _ = sweep.tick() => {
                    hub.sweep();
                    hub.flush_push();
                }
                Some(tokens) = dead.recv() => hub.drop_push_tokens(&tokens).await,
            }
        }
        control_task.abort();
        publish_task.abort();
        accept_task.abort();
        for (_, conn) in hub.conns.drain() {
            conn.task.abort();
        }
        let _ = tokio::fs::remove_file(&control_path).await;
        Ok(())
    }

    /// A number never handed out before.
    fn next_id(&mut self) -> u64 {
        self.ids += 1;
        self.ids
    }

    /// Starts the task that serves one upgraded connection, which brings its seat.
    fn accept(&mut self, (ws, seat): Upgraded) {
        let conn = self.next_id();
        let (out, outbox) = mpsc::channel(OUTBOUND_QUEUE);
        let (replay, replays) = mpsc::channel(REPLAY_QUEUE);
        let task =
            tokio::spawn(connection(ws, conn, outbox, replays, self.events.clone())).abort_handle();
        self.conns.insert(
            conn,
            Conn {
                out,
                replay,
                task,
                opened: Instant::now(),
                device: None,
                subscriptions: HashMap::new(),
                seat,
            },
        );
    }

    /// What a connection task reported.
    async fn on_event(&mut self, event: ConnEvent) {
        match event {
            ConnEvent::Message { conn, text } => self.on_message(conn, text).await,
            ConnEvent::Replayed {
                conn,
                session,
                generation,
                sent,
            } => self.on_replayed(conn, &session, generation, sent),
            ConnEvent::Answer {
                conn,
                frame,
                job,
                audit,
            } => {
                match job {
                    // The scan's own guard put its flag down.
                    Job::Scan => {}
                    Job::Launch { accepted } => {
                        let now = Instant::now();
                        self.launches.running(self.sessions.phone_started(), now);
                        if accepted {
                            self.launches.accepted(now);
                        } else {
                            self.launches.failed();
                        }
                    }
                }
                if let Some((device, session, what)) = audit {
                    self.store.audit(&device, &session, &what).await;
                }
                self.send(conn, &frame);
            }
            ConnEvent::Stalled { conn } | ConnEvent::Closed { conn } => self.drop_conn(conn),
        }
    }

    /// One frame from a phone.
    async fn on_message(&mut self, conn: u64, text: Option<String>) {
        let Some(state) = self.conns.get(&conn) else {
            return;
        };
        match (state.device.clone(), text) {
            (None, Some(text)) => self.unauthenticated(conn, &text).await,
            (None, None) => {
                self.refuse(conn, 0, Code::Invalid, NOT_TEXT);
                self.close(conn);
            }
            (Some(device), text) => self.authenticated(conn, &device, text.as_deref()).await,
        }
    }

    /// A frame before `hello`: only `hello` and `pair` are taken; anything else, or a failure,
    /// is refused and closes the connection.
    async fn unauthenticated(&mut self, conn: u64, text: &str) {
        let (req, command) = match parse_phone(text) {
            Ok(parsed) => parsed,
            Err(reason) => {
                self.refuse(conn, req_of(text), Code::Invalid, &reason);
                self.close(conn);
                return;
            }
        };
        let now = SystemTime::now();
        match command {
            PhoneCommand::Hello { token } => {
                // A revoke by the CLI must be seen before the token is believed.
                self.store.reload_if_changed().await;
                let Some(device) = self
                    .store
                    .authenticate(token.expose(), &self.owner, now)
                    .await
                else {
                    self.refuse(conn, req, Code::Unpaired, "Unknown device; pair it again");
                    self.close(conn);
                    return;
                };
                let name = device.name.clone();
                // Out of the waiting room before the phone hears it is in.
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.device = Some(device);
                    state.seat.said_hello();
                }
                self.send(conn, &welcome(&name));
                let rows = self.sessions.list();
                self.send(conn, &sessions_frame(&rows));
            }
            PhoneCommand::Pair {
                secret,
                name,
                platform,
            } => match self
                .store
                .pair(secret.expose(), &name, &platform, &self.owner, now)
                .await
            {
                Ok((token, device)) => self.send(conn, &paired(&token, &device.name)),
                Err(reason) => {
                    let code = if reason == SECRET_REFUSED {
                        Code::Unpaired
                    } else {
                        Code::Invalid
                    };
                    self.refuse(conn, req, code, reason);
                    self.close(conn);
                }
            },
            _ => {
                self.refuse(conn, req, Code::Unpaired, "Say hello first");
                self.close(conn);
            }
        }
    }

    /// A frame from a device that said hello.
    /// `None` is a frame that was not text: it spends a command and is refused.
    async fn authenticated(&mut self, conn: u64, device: &Device, text: Option<&str>) {
        self.store.reload_if_changed().await;
        if !self.paired(device) {
            self.revoke(conn);
            return;
        }
        let req = text.map_or(0, req_of);
        if !self.take_token(&device.token_sha256) {
            self.refuse(conn, req, Code::RateLimited, RATE_REFUSED);
            return;
        }
        let Some(text) = text else {
            self.refuse(conn, 0, Code::Invalid, NOT_TEXT);
            return;
        };
        let (req, command) = match parse_phone(text) {
            Ok(parsed) => parsed,
            Err(reason) => {
                self.refuse(conn, req_of(text), Code::Invalid, &reason);
                return;
            }
        };
        let (session, command, what) = match command {
            PhoneCommand::Hello { .. } | PhoneCommand::Pair { .. } => {
                self.refuse(conn, req, Code::Invalid, "Already connected");
                return;
            }
            PhoneCommand::Ping => return self.send(conn, &ack(req, "pong")),
            PhoneCommand::RegisterPush { token } => {
                let token = token.map(|token| token.expose().to_owned());
                return self.register_push(conn, device, req, token).await;
            }
            PhoneCommand::Subscribe { session, after } => {
                self.subscribe(conn, req, session, after);
                return;
            }
            PhoneCommand::Unsubscribe { session } => {
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.subscriptions.remove(&session);
                }
                self.send(conn, &ack(req, "Unsubscribed"));
                return;
            }
            PhoneCommand::Start { mode, confirm, .. }
                if !full_access_confirmed(
                    mode.as_deref().unwrap_or_default(),
                    confirm.as_deref(),
                ) =>
            {
                self.refuse(conn, req, Code::NotAllowed, FULL_ACCESS);
                return;
            }
            PhoneCommand::Recent { workspace } => {
                self.recent(conn, req, workspace);
                return;
            }
            PhoneCommand::Start {
                engine,
                workspace,
                resume,
                model,
                mode,
                confirm,
            } => {
                let request = StartRequest {
                    engine,
                    workspace: PathBuf::from(workspace),
                    resume,
                    model,
                    mode,
                    confirm,
                };
                self.start(conn, &device.name, req, request);
                return;
            }
            PhoneCommand::SetMode { mode, confirm, .. }
                if !full_access_confirmed(&mode, confirm.as_deref()) =>
            {
                self.refuse(conn, req, Code::NotAllowed, FULL_ACCESS);
                return;
            }
            PhoneCommand::Prompt { text, .. } | PhoneCommand::Steer { text, .. }
                if text.len() > PROMPT_LIMIT =>
            {
                self.refuse(conn, req, Code::TooLarge, TOO_LONG);
                return;
            }
            command => {
                let Some(parts) = bridge_command(command) else {
                    return;
                };
                parts
            }
        };
        self.route(conn, device, req, &session, &command, &what)
            .await;
    }

    /// Sets, or with `None` clears, `device`'s push token, and answers `req`. Neither the token nor
    /// the device's hash reaches the audit line.
    async fn register_push(&mut self, conn: u64, device: &Device, req: u64, token: Option<String>) {
        let on = token.is_some();
        match self
            .store
            .set_push_token(&device.token_sha256, token.clone())
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                self.refuse(conn, req, Code::Unavailable, PUSH_NOT_SAVED);
                return;
            }
            Err(error) => {
                // The error names a path and a kind, never the token or the device's hash.
                eprintln!(
                    "Couldn't save a phone's notification setting: {}",
                    error.kind()
                );
                self.refuse(conn, req, Code::Unavailable, PUSH_NOT_SAVED);
                return;
            }
        }
        // The connections of this device carry a copy of it.
        for state in self.conns.values_mut() {
            if let Some(held) = state
                .device
                .as_mut()
                .filter(|held| held.token_sha256 == device.token_sha256)
            {
                held.push_token.clone_from(&token);
            }
        }
        let (message, what) = if on {
            ("Notifications on", "push on")
        } else {
            ("Notifications off", "push off")
        };
        self.store.audit(&device.name, "", what).await;
        self.send(conn, &ack(req, message));
    }

    /// Lists resumable sessions in the background; the scan reads up to 400 journals, and only
    /// one runs at a time.
    fn recent(&mut self, conn: u64, req: u64, workspace: Option<String>) {
        let Some(scanning) = Scanning::begin(&self.scanning) else {
            self.refuse(conn, req, Code::Busy, SCAN_RUNNING);
            return;
        };
        let events = self.events.clone();
        let directory = self.directory.clone();
        let configured = self.store.config().workspaces.clone();
        tokio::spawn(async move {
            let scan = async {
                let allowed = launch::allowed_workspaces(&directory, &configured).await;
                let wanted = workspace.as_deref().map(Path::new);
                launch::recent(&directory, &allowed, wanted).await
            };
            let frame = match timeout(JOB_WITHIN, scan).await {
                Ok(Ok(items)) => recent_frame(req, &items),
                Ok(Err((code, reason))) => refused(req, code, &reason),
                Err(_) => refused(req, Code::Timeout, JOB_TIMED_OUT),
            };
            // Before the answer, so the phone may scan again as soon as it has it.
            drop(scanning);
            let done = ConnEvent::Answer {
                conn,
                frame,
                job: Job::Scan,
                audit: None,
            };
            let _ = events.send(done).await;
        });
    }

    /// Starts a terminal UI in tmux in the background and answers when tmux has finished.
    fn start(&mut self, conn: u64, device: &str, req: u64, request: StartRequest) {
        let registered = self.sessions.phone_started();
        let running = self.launches.running(registered, Instant::now());
        self.launches.begin();
        let events = self.events.clone();
        let directory = self.directory.clone();
        let configured = self.store.config().workspaces.clone();
        let device = device.to_owned();
        tokio::spawn(async move {
            let started = launch::start(&directory, &configured, &request, (running, &device));
            let result = timeout(JOB_WITHIN, started)
                .await
                .unwrap_or_else(|_| Err((Code::Timeout, JOB_TIMED_OUT.to_owned())));
            let (frame, audit, accepted) = match result {
                Ok(started) => {
                    let place = started.workspace.to_string_lossy();
                    let resume = if request.resume.is_some() {
                        "yes"
                    } else {
                        "no"
                    };
                    let what = format!(
                        "start {} {} resume={resume} mode={}",
                        request.engine,
                        clip_to(&place, AUDIT_PATH),
                        request.mode.as_deref().unwrap_or("default"),
                    );
                    let text = format!("Starting {} in {place}", started.name);
                    (ack(req, &text), Some((device, started.name, what)), true)
                }
                Err((code, reason)) => (refused(req, code, &reason), None, false),
            };
            let done = ConnEvent::Answer {
                conn,
                frame,
                job: Job::Launch { accepted },
                audit,
            };
            let _ = events.send(done).await;
        });
    }

    /// Sends `command` to `session` under a fresh hub req, and audits it once it is on its way.
    async fn route(
        &mut self,
        conn: u64,
        device: &Device,
        req: u64,
        session: &str,
        command: &BridgeCommand,
        what: &str,
    ) {
        let hub_req = self.next_id();
        match self
            .sessions
            .send(session, hub_req, &device.name, command)
            .await
        {
            Ok(()) => {
                let pending = Pending {
                    conn,
                    req,
                    session: session.to_owned(),
                    deadline: Instant::now() + REPLY_WITHIN,
                };
                self.pending.insert(hub_req, pending);
                self.store.audit(&device.name, session, what).await;
            }
            Err((code, reason)) => self.refuse(conn, req, code, reason),
        }
    }

    /// Starts a subscription: the last state and catalog now, then the replay in its own task.
    fn subscribe(&mut self, conn: u64, req: u64, session: String, after: Option<Cursor>) {
        let Some(journals) = self.sessions.journals(&session) else {
            self.refuse(conn, req, Code::Unavailable, "No such session");
            return;
        };
        let generation = self.next_id();
        let mut frames = Vec::new();
        if let Some(state) = self.sessions.state(&session) {
            frames.push(state_frame(&session, state).to_string());
        }
        if let Some(catalog) = self.sessions.catalog(&session) {
            frames.push(catalog_frame(&session, catalog).to_string());
        }
        let Some(state) = self.conns.get_mut(&conn) else {
            return;
        };
        let names: Vec<String> = journals.iter().map(|j| file_name(j)).collect();
        // A cursor in a journal the session does not list is stale: the replay starts over.
        let after = after.filter(|cursor| names.contains(&cursor.journal));
        let current = Arc::new(AtomicBool::new(true));
        // The phone has what its cursor names.
        let seen = after
            .as_ref()
            .map(|c| (c.journal.clone(), c.seq))
            .into_iter()
            .collect();
        let job = ReplayJob {
            conn,
            journals,
            first: subscribed_frame(&session, after.is_some(), &names).to_string(),
            after,
            session: session.clone(),
            generation,
            out: state.replay.clone(),
            current: Arc::clone(&current),
            events: self.events.clone(),
        };
        let task = tokio::spawn(replay(job)).abort_handle();
        let subscription = Subscription::Replaying {
            generation,
            req,
            seen,
            buffer: Vec::new(),
            task,
            current,
        };
        // A subscription it replaces stops its replay as it drops.
        state.subscriptions.insert(session, subscription);
        for frame in frames {
            self.push(conn, frame);
        }
    }

    /// A replay's frames are all sent: the buffered events it did not send follow, then the ack,
    /// and the subscription is live. When the outbox has no room for them all and the ack, they
    /// go the replay's way instead, waiting for room as a page does, and the hub looks again once
    /// they are written: the subscription stays replaying, and buffers, until then.
    fn on_replayed(&mut self, conn: u64, session: &str, generation: u64, sent: Vec<(String, u64)>) {
        let Some(state) = self.conns.get_mut(&conn) else {
            return;
        };
        let Some(subscription) = state.subscriptions.get_mut(session) else {
            return;
        };
        let Subscription::Replaying {
            generation: standing,
            req,
            seen,
            buffer,
            task,
            current,
        } = subscription
        else {
            return;
        };
        if *standing != generation {
            return;
        }
        for (journal, seq) in sent {
            admit(seen, &journal, seq);
        }
        let fresh = fresh(std::mem::take(buffer), seen);
        if fresh.len() >= state.out.capacity() {
            let job = HandOff {
                conn,
                frames: fresh,
                session: session.to_owned(),
                generation,
                out: state.replay.clone(),
                current: Arc::clone(current),
                events: self.events.clone(),
            };
            *task = tokio::spawn(hand_off(job)).abort_handle();
            return;
        }
        let req = *req;
        let seen = std::mem::take(seen);
        *subscription = Subscription::Live { seen };
        for frame in fresh {
            self.push(conn, frame);
        }
        self.send(conn, &ack(req, "Subscribed"));
    }

    /// What a session did: events, state, catalog, replies and changed rows.
    fn on_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::Event {
                id,
                journal,
                seq,
                record,
            } => self.on_journal_event(&id, &journal, seq, &record),
            SessionUpdate::State { id } => {
                if let Some(state) = self.sessions.state(&id) {
                    let frame = state_frame(&id, state).to_string();
                    self.tell_subscribers(&id, &frame);
                }
                // A row shows the state's phase, mode and approvals.
                self.relist();
                self.observe_push();
            }
            SessionUpdate::Catalog { id } => {
                if let Some(catalog) = self.sessions.catalog(&id) {
                    let frame = catalog_frame(&id, catalog).to_string();
                    self.tell_subscribers(&id, &frame);
                }
            }
            SessionUpdate::Reply {
                id,
                req,
                ok,
                message,
                code,
            } => {
                // Only the session the command went to may answer it.
                if self.pending.get(&req).is_none_or(|p| p.session != id) {
                    return;
                }
                let Some(pending) = self.pending.remove(&req) else {
                    return;
                };
                let frame = if ok {
                    ack(pending.req, &message)
                } else {
                    refused(pending.req, code.unwrap_or(Code::Invalid), &message)
                };
                self.send(pending.conn, &frame);
            }
            SessionUpdate::Changed => {
                self.refuse_ended();
                self.relist();
                self.observe_push();
            }
        }
    }

    /// Refuses at once the commands waiting on a session whose link closed or that went away:
    /// no answer can come.
    fn refuse_ended(&mut self) {
        let ended: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| !self.sessions.linked(&pending.session))
            .map(|(&req, _)| req)
            .collect();
        for req in ended {
            if let Some(pending) = self.pending.remove(&req) {
                self.refuse(pending.conn, pending.req, Code::Unavailable, SESSION_ENDED);
            }
        }
    }

    /// A live journal record: streamed to live subscribers, buffered for replaying ones.
    fn on_journal_event(&mut self, id: &str, journal: &str, seq: u64, record: &Value) {
        let frame = event_text(id, journal, seq, record);
        broadcast(&mut self.watchers, &frame);
        let (mut live, mut overflowed) = (Vec::new(), Vec::new());
        for (&conn, state) in &mut self.conns {
            match state.subscriptions.get_mut(id) {
                Some(live_one @ Subscription::Live { .. }) => {
                    if live_one.admit(journal, seq) {
                        live.push(conn);
                    }
                }
                Some(Subscription::Replaying { buffer, .. }) if buffer.len() >= LIVE_BUFFER => {
                    overflowed.push(conn);
                }
                Some(Subscription::Replaying { buffer, .. }) => {
                    buffer.push((journal.to_owned(), seq, frame.clone()));
                }
                None => {}
            }
        }
        for conn in overflowed {
            self.drop_conn(conn);
        }
        for conn in live {
            self.push(conn, frame.clone());
        }
    }

    /// Sends `frame` to every connection subscribed to `session`.
    fn tell_subscribers(&mut self, session: &str, frame: &str) {
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| state.subscriptions.contains_key(session))
            .map(|(&conn, _)| conn)
            .collect();
        for conn in targets {
            self.push(conn, frame.to_owned());
        }
    }

    /// Sends the `sessions` frame to every authenticated phone if the rows changed.
    fn relist(&mut self) {
        self.relist_watchers();
        let list = self.sessions.list();
        if list == self.listed {
            return;
        }
        let frame = sessions_frame(&list).to_string();
        self.listed = list;
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| state.device.is_some())
            .map(|(&conn, _)| conn)
            .collect();
        for conn in targets {
            self.push(conn, frame.clone());
        }
    }

    /// The `sessions` frame for watchers if the rows changed.
    fn relist_watchers(&mut self) {
        if self.watchers.is_empty() {
            return;
        }
        let rows = self.sessions.list();
        if rows == self.watched {
            return;
        }
        broadcast(&mut self.watchers, &sessions_frame(&rows).to_string());
        self.watched = rows;
    }

    /// Attaches a watcher, or turns it away when too many are attached.
    fn watch(&mut self, watcher: mpsc::Sender<String>) {
        self.watchers.retain(|w| !w.is_closed());
        if self.watchers.len() >= MAX_WATCHERS {
            let _ = watcher.try_send(json!({"error": "Too many watchers"}).to_string());
            return;
        }
        if self.watchers.is_empty() {
            self.watched = self.sessions.list();
        }
        let rows = sessions_frame(&self.watched).to_string();
        if watcher.try_send(rows).is_ok() {
            self.watchers.push(watcher);
        }
    }

    /// Answers a control-socket request.
    async fn on_control(&mut self, request: Control) {
        match request {
            Control::PairSecret(reply) => {
                let _ = reply.send(self.store.pairing_secret(SystemTime::now()));
            }
            Control::Status(reply) => {
                self.store.reload_if_changed().await;
                let _ = reply.send((self.port, self.store.devices().len()));
            }
            Control::Watch(watcher) => self.watch(watcher),
        }
    }

    /// Re-reads `devices.json` and closes the connections of devices no longer in it.
    async fn reload(&mut self) {
        self.store.reload_if_changed().await;
        let revoked: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| state.device.as_ref().is_some_and(|d| !self.paired(d)))
            .map(|(&conn, _)| conn)
            .collect();
        for conn in revoked {
            self.revoke(conn);
        }
    }

    /// Refuses, for their phones, the commands no session answered in time, and closes
    /// connections that never said hello.
    fn sweep(&mut self) {
        let now = Instant::now();
        self.launches.running(self.sessions.phone_started(), now);
        let silent: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| state.device.is_none() && now >= state.opened + HELLO_WITHIN)
            .map(|(&conn, _)| conn)
            .collect();
        for conn in silent {
            self.close(conn);
        }
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(&req, _)| req)
            .collect();
        for req in expired {
            if let Some(pending) = self.pending.remove(&req) {
                self.refuse(pending.conn, pending.req, Code::Timeout, NO_ANSWER);
            }
        }
    }

    /// Whether `device` is still paired.
    fn paired(&self, device: &Device) -> bool {
        self.store
            .devices()
            .iter()
            .any(|d| d.token_sha256 == device.token_sha256)
    }

    /// Takes one command from the bucket of the device whose token hash is `key`.
    fn take_token(&mut self, key: &str) -> bool {
        let now = Instant::now();
        self.buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::full(now))
            .take(now)
    }

    /// Queues a refusal of `req` for `conn`, with its code.
    fn refuse(&mut self, conn: u64, req: u64, code: Code, reason: &str) {
        self.send(conn, &refused(req, code, reason));
    }

    /// Queues `frame` for `conn`.
    fn send(&mut self, conn: u64, frame: &Value) {
        self.push(conn, frame.to_string());
    }

    /// Queues `frame` for `conn`; a connection whose queue is full is dropped.
    fn push(&mut self, conn: u64, frame: String) {
        let Some(state) = self.conns.get(&conn) else {
            return;
        };
        if state.out.try_send(Out::Text(frame)).is_err() {
            self.drop_conn(conn);
        }
    }

    /// Ends `conn` after what is already queued, with a closing handshake.
    fn close(&mut self, conn: u64) {
        let Some(state) = self.forget(conn) else {
            return;
        };
        if state.out.try_send(Out::Close).is_err() {
            state.task.abort();
        }
    }

    /// Tells `conn` its device was unpaired, then ends it once that is sent, or after
    /// [`REVOKED_GRACE`] if the phone does not take it.
    fn revoke(&mut self, conn: u64) {
        let Some(state) = self.forget(conn) else {
            return;
        };
        let told = state
            .out
            .try_send(Out::Text(
                refused(0, Code::Revoked, REVOKED_REFUSED).to_string(),
            ))
            .is_ok();
        if !told || state.out.try_send(Out::Close).is_err() {
            state.task.abort();
            return;
        }
        let task = state.task;
        tokio::spawn(async move {
            tokio::time::sleep(REVOKED_GRACE).await;
            task.abort();
        });
    }

    /// Hands the pusher what the sessions look like; what it finds new waits in a window for
    /// each device with a push token (whether it is connected is judged when the window closes).
    fn observe_push(&mut self) {
        let notices = self.pusher.observe(&self.sessions.snapshots());
        let now = Instant::now();
        for device in self.store.devices() {
            if device.push_token.is_some() {
                self.pusher.offer(&device.token_sha256, &notices, now);
            }
        }
    }

    /// Posts the pushes whose windows have closed, in the background. A device with a phone
    /// connected right now is looked at, so it gets none.
    fn flush_push(&mut self) {
        let now = Instant::now();
        self.pusher.prune(now);
        let mut batch = Vec::new();
        for key in self.pusher.waiting() {
            let token = self
                .store
                .devices()
                .iter()
                .find(|device| device.token_sha256 == key)
                .and_then(|device| device.push_token.clone());
            let connected = token.is_none()
                || self.conns.values().any(|conn| {
                    conn.device
                        .as_ref()
                        .is_some_and(|device| device.token_sha256 == key)
                });
            if let (Some(due), Some(token)) = (self.pusher.due_for(&key, connected, now), token) {
                batch.push((token, due));
            }
        }
        if batch.is_empty() {
            return;
        }
        self.sender.dispatch(batch);
    }

    /// Clears push tokens Expo says no device has, from `devices.json` and the connections'
    /// copies of their devices.
    async fn drop_push_tokens(&mut self, tokens: &[String]) {
        for token in tokens {
            match self.store.drop_push_token(token).await {
                Ok(Some(name)) => self.store.audit(&name, "", "push dropped").await,
                Ok(None) => {}
                // The only push log lines are the ruled ones: no kind, path or token.
                Err(_) => push::report(&[push::Outcome::Failed]),
            }
            for state in self.conns.values_mut() {
                if let Some(held) = state
                    .device
                    .as_mut()
                    .filter(|held| held.push_token.as_ref() == Some(token))
                {
                    held.push_token = None;
                }
            }
        }
    }

    /// Ends `conn` now.
    fn drop_conn(&mut self, conn: u64) {
        if let Some(state) = self.forget(conn) {
            state.task.abort();
        }
    }

    /// Removes `conn` and its unanswered commands; its replays stop as it drops. When it was its
    /// device's last connection, the approvals that device was spared while looking are offered
    /// to it again.
    fn forget(&mut self, conn: u64) -> Option<Conn> {
        let state = self.conns.remove(&conn)?;
        self.pending.retain(|_, pending| pending.conn != conn);
        if let Some(device) = &state.device {
            let key = &device.token_sha256;
            let still_here = self.conns.values().any(|other| {
                other
                    .device
                    .as_ref()
                    .is_some_and(|held| &held.token_sha256 == key)
            });
            // A device unpaired, or without a push token, has nowhere to be told.
            let reachable = self
                .store
                .devices()
                .iter()
                .any(|known| &known.token_sha256 == key && known.push_token.is_some());
            if !still_here && reachable {
                self.pusher.disconnected(key, Instant::now());
            }
        }
        Some(state)
    }
}

/// The session, the command and the audit text of a phone command that a terminal UI runs.
fn bridge_command(command: PhoneCommand) -> Option<(String, BridgeCommand, String)> {
    match command {
        PhoneCommand::End { session } => Some((session, BridgeCommand::End, "end".into())),
        PhoneCommand::Prompt { session, text } => {
            Some((session, BridgeCommand::Prompt { text }, "prompt".to_owned()))
        }
        PhoneCommand::Answer {
            session,
            approval,
            allow,
        } => {
            let verdict = if allow { "allow" } else { "deny" };
            let what = format!("answer {approval} {verdict}");
            Some((session, BridgeCommand::Answer { approval, allow }, what))
        }
        PhoneCommand::Cancel { session } => Some((session, BridgeCommand::Cancel, "cancel".into())),
        PhoneCommand::Steer { session, text } => {
            Some((session, BridgeCommand::Steer { text }, "steer".into()))
        }
        PhoneCommand::SetMode {
            session,
            mode,
            confirm,
        } => {
            let what = format!("set_mode {}", clip(&mode));
            Some((session, BridgeCommand::SetMode { mode, confirm }, what))
        }
        PhoneCommand::SetModel { session, selection } => {
            let what = format!("set_model {}", clip(&selection));
            Some((session, BridgeCommand::SetModel { selection }, what))
        }
        _ => None,
    }
}

/// The frames of `buffer`, `(journal, seq, frame)`, that the phone lacks given `seen` (see
/// [`admit`]), which then counts them as had.
fn fresh(buffer: Vec<(String, u64, String)>, seen: &mut HashMap<String, u64>) -> Vec<String> {
    buffer
        .into_iter()
        .filter(|(journal, seq, _)| admit(seen, journal, *seq))
        .map(|(_, _, frame)| frame)
        .collect()
}

/// An interval whose first tick is one `period` from now.
fn every(period: Duration) -> Interval {
    let mut tick = interval_at(Instant::now() + period, period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick
}

/// The text of the `event` frame for a record `{type, data}`.
fn event_text(session: &str, journal: &str, seq: u64, record: &Value) -> String {
    let kind = record
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = record.get("data").unwrap_or(&Value::Null);
    event_frame(session, journal, seq, kind, data).to_string()
}

/// At most [`AUDIT_TEXT`] characters of `text`.
fn clip(text: &str) -> String {
    text.chars().take(AUDIT_TEXT).collect()
}

/// Queues `frame` for every watcher; one whose queue is full or gone is dropped, never waited for.
fn broadcast(watchers: &mut Vec<mpsc::Sender<String>>, frame: &str) {
    watchers.retain(|watcher| watcher.try_send(frame.to_owned()).is_ok());
}

/// Makes `run` ready for the hub's socket: creates it owner-only if missing, refuses one that is
/// a symlink (it would put the socket elsewhere), not a directory, or not this user's, and
/// tightens one looser than 0700, as the terminal UI does.
async fn private_dir(run: &Path) -> std::io::Result<()> {
    octet_store::create_private_dir(run).await?;
    let meta = tokio::fs::symlink_metadata(run).await?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other("it is a symlink"));
    }
    if !meta.is_dir() {
        return Err(std::io::Error::other("it is not a directory"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::other("it belongs to another user"));
    }
    if meta.mode() & 0o077 != 0 {
        tokio::fs::set_permissions(run, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

/// Binds the control socket at `path`, owner-only, removing a stale one first.
async fn bind_control(path: &Path) -> Result<UnixListener, String> {
    if UnixStream::connect(path).await.is_ok() {
        return Err("Another hub is already running".into());
    }
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("Couldn't remove {}: {e}", path.display()));
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).map_err(|e| format!("Couldn't bind {}: {e}", path.display()))?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|e| format!("Couldn't restrict {}: {e}", path.display()))?;
    Ok(listener)
}

/// Accepts TCP connections and upgrades each in its own task, handing the upgraded ones, with
/// their seats, to the hub loop. At most [`MAX_HANDSHAKES`] upgrade at once: one accepted past
/// that lets the oldest go, so sockets that stall mid-request cannot keep a phone out, and each
/// has [`HANDSHAKE_WITHIN`] in all.
async fn accept_tcp(
    listener: TcpListener,
    owner: String,
    room: Arc<Room>,
    hub: mpsc::Sender<Upgraded>,
) {
    let mut upgrading: VecDeque<AbortHandle> = VecDeque::new();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                if hub.is_closed() {
                    return;
                }
                upgrading.retain(|task| !task.is_finished());
                if upgrading.len() >= MAX_HANDSHAKES
                    && let Some(oldest) = upgrading.pop_front()
                {
                    oldest.abort();
                }
                let (owner, room, hub) = (owner.clone(), Arc::clone(&room), hub.clone());
                let task = tokio::spawn(async move {
                    if let Some(upgraded) = handshake(stream, &owner, &room).await {
                        let _ = hub.send(upgraded).await;
                    }
                });
                upgrading.push_back(task.abort_handle());
            }
            // Out of descriptors, say: wait rather than spin.
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// Where the hub is published, as Tailscale last said.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Publication {
    /// Not asked yet.
    Unknown,
    /// Not published.
    Nowhere,
    /// Published at this HTTPS URL.
    At(String),
}

/// The hub's [`Publication`], as last asked.
type Published = watch::Receiver<Publication>;

/// Asks `publisher` where `port` is published, now and then every `recheck`, so that control
/// requests answer from what it last said and never wait on Tailscale.
async fn watch_published(
    port: u16,
    publisher: Publisher,
    recheck: Duration,
    published: watch::Sender<Publication>,
) {
    loop {
        let url = publisher(port).await;
        published.send_replace(url.map_or(Publication::Nowhere, Publication::At));
        tokio::time::sleep(recheck).await;
    }
}

/// Where the hub is published, as last asked; waits for the first answer, for at most
/// [`CONTROL_WITHIN`].
async fn last_published(published: &mut Published) -> Option<String> {
    let known = published.wait_for(|state| *state != Publication::Unknown);
    match timeout(CONTROL_WITHIN, known).await {
        Ok(Ok(state)) => match &*state {
            Publication::At(url) => Some(url.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Accepts control clients, each answered in its own task.
async fn serve_control(listener: UnixListener, hub: mpsc::Sender<Control>, published: Published) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(control_client(stream, hub.clone(), published.clone()));
            }
            // Out of descriptors, say: wait rather than spin.
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// Reads one request line from a control client and writes one answer line.
async fn control_client(stream: UnixStream, hub: mpsc::Sender<Control>, mut published: Published) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(CONTROL_LIMIT));
    if !timeout(CONTROL_WITHIN, reader.read_line(&mut line))
        .await
        .is_ok_and(|read| read.is_ok_and(|n| n > 0))
    {
        return;
    }
    if request_kind(&line) == "watch" {
        watch_stream(&mut reader, &mut write, &hub).await;
        return;
    }
    let answer = control_answer(&line, &hub, &mut published).await;
    let _ = timeout(
        CONTROL_WITHIN,
        write.write_all(format!("{answer}\n").as_bytes()),
    )
    .await;
}

/// The `type` of a control request line; empty when it has none.
fn request_kind(line: &str) -> String {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|request| request.get("type")?.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Writes the hub's watch lines to a control client until it leaves, falls behind or the hub
/// stops.
async fn watch_stream(
    reader: &mut (impl AsyncReadExt + Unpin),
    write: &mut OwnedWriteHalf,
    hub: &mpsc::Sender<Control>,
) {
    let (watcher, mut lines) = mpsc::channel(WATCH_QUEUE);
    if hub.send(Control::Watch(watcher)).await.is_err() {
        return;
    }
    let mut ignored = [0u8; 64];
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { return };
                let sent = timeout(WRITE_STALL, write.write_all(format!("{line}\n").as_bytes())).await;
                if !sent.is_ok_and(|sent| sent.is_ok()) {
                    return;
                }
            }
            // The client sends nothing more; a read that ends means it left.
            read = reader.read(&mut ignored) => {
                if read.is_ok_and(|n| n > 0) {
                    continue;
                }
                return;
            }
        }
    }
}

/// The hub's loopback port and how many phones are paired; `None` once the hub is stopping.
async fn hub_status(hub: &mpsc::Sender<Control>) -> Option<(u16, usize)> {
    let (reply, status) = oneshot::channel();
    hub.send(Control::Status(reply)).await.ok()?;
    status.await.ok()
}

/// The answer to one control request.
async fn control_answer(
    line: &str,
    hub: &mpsc::Sender<Control>,
    published: &mut Published,
) -> Value {
    let kind = request_kind(line);
    let gone = || json!({"error": "The hub is stopping"});
    match kind.as_str() {
        "pair_secret" => {
            // A secret is minted only for a hub a phone can reach: the port must be published.
            let Some((port, _)) = hub_status(hub).await else {
                return gone();
            };
            let Some(url) = last_published(published).await else {
                return json!({"secret": null, "url": null,
                    "reason": format!("Publish it first: tailscale serve --bg {port}")});
            };
            let (reply, secret) = oneshot::channel();
            if hub.send(Control::PairSecret(reply)).await.is_err() {
                return gone();
            }
            let Ok(secret) = secret.await else {
                return gone();
            };
            json!({"secret": secret, "url": url})
        }
        "status" => {
            let Some((port, devices)) = hub_status(hub).await else {
                return gone();
            };
            let url = last_published(published).await;
            json!({"port": port, "published": url.is_some(), "url": url, "devices": devices})
        }
        _ => json!({"error": "Unknown request"}),
    }
}

/// What one replay task needs.
#[derive(Debug)]
struct ReplayJob {
    conn: u64,
    journals: Vec<PathBuf>,
    /// The `subscribed` frame, sent before the replay.
    first: String,
    after: Option<Cursor>,
    session: String,
    generation: u64,
    out: mpsc::Sender<ReplayItem>,
    /// Whether the subscription still stands; given to every frame.
    current: Arc<AtomicBool>,
    events: mpsc::Sender<ConnEvent>,
}

/// Queues `item` for the writer, waiting for room for at most [`WRITE_STALL`]; false if there
/// was none in time or the connection is gone.
async fn queue(out: &mpsc::Sender<ReplayItem>, item: ReplayItem) -> bool {
    timeout(WRITE_STALL, out.send(item))
        .await
        .is_ok_and(|queued| queued.is_ok())
}

/// Replays a session's journals after the cursor, a page at a time, into the connection's
/// replay queue, then marks the end. A phone with no room for too long is reported stalled.
async fn replay(job: ReplayJob) {
    let ReplayJob {
        conn,
        journals,
        first,
        mut after,
        session,
        generation,
        out,
        current,
        events,
    } = job;
    let mut sent: Vec<(String, u64)> = Vec::new();
    let mut offset = 0;
    let stalled = || async {
        let _ = events.send(ConnEvent::Stalled { conn }).await;
    };
    let frame = |frame: String| ReplayItem::Frame {
        frame,
        current: Arc::clone(&current),
    };
    if !queue(&out, frame(first)).await {
        stalled().await;
        return;
    }
    loop {
        let (page, more, end) =
            tail::replay_from(&journals, after.as_ref(), offset, REPLAY_PAGE).await;
        offset = end;
        let Some((journal, seq, _)) = page.last() else {
            break;
        };
        let next = Cursor {
            journal: journal.clone(),
            seq: *seq,
        };
        for (journal, seq, record) in &page {
            if !queue(&out, frame(event_text(&session, journal, *seq, record))).await {
                stalled().await;
                return;
            }
            match sent.last_mut() {
                Some((last, at)) if last == journal => *at = *seq,
                _ => sent.push((journal.clone(), *seq)),
            }
        }
        after = Some(next);
        if !more {
            break;
        }
    }
    let done = ReplayItem::Done {
        session,
        generation,
        sent,
    };
    if !queue(&out, done).await {
        stalled().await;
    }
}

/// What one hand-off task needs: the live events a replay's end found buffered.
#[derive(Debug)]
struct HandOff {
    conn: u64,
    frames: Vec<String>,
    session: String,
    generation: u64,
    out: mpsc::Sender<ReplayItem>,
    current: Arc<AtomicBool>,
    events: mpsc::Sender<ConnEvent>,
}

/// Queues buffered live events the way a replay page goes, waiting for room, then marks the
/// end again so the hub hands over what buffered meanwhile. A phone with no room for too long
/// is reported stalled.
async fn hand_off(job: HandOff) {
    let HandOff {
        conn,
        frames,
        session,
        generation,
        out,
        current,
        events,
    } = job;
    for frame in frames {
        let current = Arc::clone(&current);
        if !queue(&out, ReplayItem::Frame { frame, current }).await {
            let _ = events.send(ConnEvent::Stalled { conn }).await;
            return;
        }
    }
    // The hub counted these as the phone's when it handed them over.
    let done = ReplayItem::Done {
        session,
        generation,
        sent: Vec::new(),
    };
    if !queue(&out, done).await {
        let _ = events.send(ConnEvent::Stalled { conn }).await;
    }
}

/// Serves one upgraded connection: frames both ways until either side ends.
async fn connection(
    ws: WebSocketStream<TcpStream>,
    conn: u64,
    mut outbox: mpsc::Receiver<Out>,
    mut replays: mpsc::Receiver<ReplayItem>,
    events: mpsc::Sender<ConnEvent>,
) {
    pump(ws, conn, &mut outbox, &mut replays, &events).await;
    let _ = events.send(ConnEvent::Closed { conn }).await;
}

/// The WebSocket upgrade, refused as [`Gate`] says, within [`HANDSHAKE_WITHIN`]: the connection
/// and the seat it took.
async fn handshake(stream: TcpStream, owner: &str, room: &Arc<Room>) -> Option<Upgraded> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let mut seat = None;
    let gate = Gate {
        owner,
        room,
        seat: &mut seat,
    };
    let ws = timeout(
        HANDSHAKE_WITHIN,
        accept_hdr_async_with_config(stream, gate, Some(config)),
    )
    .await
    .ok()?
    .ok()?;
    Some((ws, seat?))
}

/// The handshake check: the request must carry `Tailscale-User-Login` equal to the owner's and
/// come from no other site's web page (403), and the hub must have room for it (503). `seat`
/// gets the place it took.
#[derive(Debug)]
struct Gate<'a> {
    owner: &'a str,
    room: &'a Arc<Room>,
    seat: &'a mut Option<Seat>,
}

impl Callback for Gate<'_> {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let headers = request.headers();
        let login = headers
            .get("Tailscale-User-Login")
            .and_then(|value| value.to_str().ok());
        if login != Some(self.owner) || foreign_origin(headers) {
            return Err(refusal(StatusCode::FORBIDDEN, "Forbidden"));
        }
        let Some(seat) = self.room.seat() else {
            return Err(refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many connections",
            ));
        };
        *self.seat = Some(seat);
        Ok(response)
    }
}

/// A handshake refusal with `status`.
fn refusal(status: StatusCode, text: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(text.to_owned()));
    *response.status_mut() = status;
    response
}

/// Whether a web page of another site may have opened this socket: an `Origin` is there and its
/// host is neither the `Host` the request names nor the `X-Forwarded-Host` that `tailscale
/// serve` adds. A browser sends `Origin` with every WebSocket and cannot set the others; the
/// app's socket sends none, or the hub's own address.
fn foreign_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("Origin") else {
        return false;
    };
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .map(|(_, authority)| host_of(authority))
    else {
        return true;
    };
    let named = |name: &str| {
        let value = headers.get(name)?.to_str().ok()?;
        Some(host_of(value.split(',').next().unwrap_or_default()))
    };
    ![named("Host"), named("X-Forwarded-Host")]
        .into_iter()
        .flatten()
        .any(|host| !host.is_empty() && host.eq_ignore_ascii_case(origin))
}

/// The host of `authority` (`host`, `host:port` or `[v6]:port`), without its port.
fn host_of(authority: &str) -> &str {
    let authority = authority.trim();
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
}

/// Moves frames between the phone and the hub loop: what the phone sends goes to the hub,
/// and replayed and live frames go to the phone. A frame over [`MAX_MESSAGE`], a write that
/// stalls, or the hub letting go ends it.
async fn pump(
    mut ws: WebSocketStream<TcpStream>,
    conn: u64,
    outbox: &mut mpsc::Receiver<Out>,
    replays: &mut mpsc::Receiver<ReplayItem>,
    events: &mpsc::Sender<ConnEvent>,
) {
    let mut replaying = true;
    let mut alive = Liveness::default();
    let mut pings = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = pings.tick() => {
                // A connection that stopped answering (a phone gone behind tailscaled) ends.
                let sent = timeout(WRITE_STALL, ws.send(Message::Ping(Vec::new().into()))).await;
                if !alive.ping() || !sent.is_ok_and(|sent| sent.is_ok()) {
                    return;
                }
            }
            incoming = ws.next() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => Some(text.as_str().to_owned()),
                    Some(Ok(Message::Binary(_))) => None,
                    Some(Ok(Message::Pong(_))) => {
                        alive.pong();
                        continue;
                    }
                    Some(Ok(Message::Ping(_) | Message::Frame(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                };
                if events.send(ConnEvent::Message { conn, text }).await.is_err() {
                    return;
                }
            }
            item = replays.recv(), if replaying => match item {
                Some(ReplayItem::Frame { frame, current }) => {
                    // A subscription since dropped or replaced has its acknowledgement out
                    // already: what it left queued would follow it.
                    if current.load(Ordering::Acquire) && !write(&mut ws, frame).await {
                        return;
                    }
                }
                Some(ReplayItem::Done { session, generation, sent }) => {
                    let done = ConnEvent::Replayed { conn, session, generation, sent };
                    if events.send(done).await.is_err() {
                        return;
                    }
                }
                // The hub let go of the connection; what it queued last is still to send.
                None => replaying = false,
            },
            item = outbox.recv() => match item {
                Some(Out::Text(frame)) => {
                    if !write(&mut ws, frame).await {
                        return;
                    }
                }
                Some(Out::Close) | None => {
                    let _ = timeout(CLOSE_WITHIN, ws.close(None)).await;
                    return;
                }
            },
        }
    }
}

/// Writes one text frame; false if it failed or stalled.
async fn write(ws: &mut WebSocketStream<TcpStream>, frame: String) -> bool {
    timeout(WRITE_STALL, ws.send(Message::text(frame)))
        .await
        .is_ok_and(|sent| sent.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Buffered events whose frame text is the journal and seq.
    fn buffered(events: &[(&str, u64)]) -> Vec<(String, u64, String)> {
        events
            .iter()
            .map(|&(journal, seq)| (journal.to_owned(), seq, format!("{journal}{seq}")))
            .collect()
    }

    #[test]
    fn buffered_events_the_replay_did_not_send_are_kept() {
        let buffer = buffered(&[("a", 4), ("a", 5), ("a", 6), ("b", 0), ("b", 1), ("c", 0)]);
        // Replayed up to a/5 and b/0: the rest of a, b/1 and the unknown c are new.
        let mut seen = HashMap::from([("a".to_owned(), 5), ("b".to_owned(), 0)]);
        assert_eq!(fresh(buffer, &mut seen), ["a6", "b1", "c0"]);
        assert_eq!(seen["a"], 6);
        // Nothing replayed: all are new.
        let buffer = buffered(&[("a", 4), ("b", 0)]);
        assert_eq!(fresh(buffer, &mut HashMap::new()), ["a4", "b0"]);
    }

    #[test]
    fn a_live_subscription_drops_repeats_but_takes_late_records_of_an_older_journal() {
        // The replay ended at b/3, having read a to its end (a/9); the tail had not yet emitted
        // a/9, b/2 or b/3.
        let mut live = Subscription::Live {
            seen: HashMap::from([("a".to_owned(), 9), ("b".to_owned(), 3)]),
        };
        assert!(!live.admit("a", 9));
        assert!(!live.admit("b", 2));
        assert!(!live.admit("b", 3));
        assert!(live.admit("b", 4));
        // The old session writes its last records after the new one began: they are new.
        assert!(live.admit("a", 10));
        assert!(!live.admit("a", 10), "once");
        // A journal the replay did not know is new.
        assert!(live.admit("c", 0));
    }

    #[test]
    fn a_watcher_that_falls_behind_or_leaves_is_dropped_without_waiting() {
        let (slow, mut slow_lines) = mpsc::channel(2);
        let (quick, mut quick_lines) = mpsc::channel(8);
        let (gone, gone_lines) = mpsc::channel(8);
        drop(gone_lines);
        let mut watchers = vec![slow, quick, gone];
        broadcast(&mut watchers, "one");
        assert_eq!(watchers.len(), 2, "the departed watcher is dropped");
        broadcast(&mut watchers, "two");
        broadcast(&mut watchers, "three");
        assert_eq!(
            watchers.len(),
            1,
            "the watcher with a full queue is dropped"
        );
        let got: Vec<String> = std::iter::from_fn(|| quick_lines.try_recv().ok()).collect();
        assert_eq!(got, ["one", "two", "three"]);
        let kept: Vec<String> = std::iter::from_fn(|| slow_lines.try_recv().ok()).collect();
        assert_eq!(kept, ["one", "two"], "what it had is still there to read");
    }

    #[test]
    fn an_origin_counts_as_foreign_unless_it_names_the_requests_host() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut map = HeaderMap::new();
            for &(name, value) in pairs {
                map.insert(name, value.parse().unwrap());
            }
            map
        };
        let host = ("Host", "mac.tail1234.ts.net");
        assert!(!foreign_origin(&headers(&[host])), "no Origin");
        for same in [
            "https://mac.tail1234.ts.net",
            "https://MAC.tail1234.ts.net:443",
        ] {
            assert!(
                !foreign_origin(&headers(&[host, ("Origin", same)])),
                "{same}"
            );
        }
        for other in ["https://evil.example", "null", "mac.tail1234.ts.net", ""] {
            assert!(
                foreign_origin(&headers(&[host, ("Origin", other)])),
                "{other}"
            );
        }
        let v6 = headers(&[("Host", "[::1]:47823"), ("Origin", "http://[::1]:47823")]);
        assert!(!foreign_origin(&v6));
        let no_host = headers(&[("Origin", "https://mac.tail1234.ts.net")]);
        assert!(foreign_origin(&no_host));
    }

    #[test]
    fn a_seat_is_given_back_and_the_waiting_room_is_capped() {
        let room = Arc::new(Room::default());
        let mut seats: Vec<Seat> = (0..MAX_WAITING).filter_map(|_| room.seat()).collect();
        assert_eq!(seats.len(), MAX_WAITING);
        assert!(room.seat().is_none(), "the waiting room is full");
        seats[0].said_hello();
        let another = room.seat().expect("one said hello");
        assert_eq!(room.open.load(Ordering::Acquire), MAX_WAITING + 1);
        drop(another);
        drop(seats);
        assert_eq!(room.open.load(Ordering::Acquire), 0);
        assert_eq!(room.waiting.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_bucket_takes_sixty_a_minute_and_refills() {
        let start = Instant::now();
        let mut bucket = Bucket::full(start);
        let taken = (0..100).filter(|_| bucket.take(start)).count();
        assert_eq!(taken, 60);
        // A second refills one command; a minute refills them all, and no more.
        let second = start + Duration::from_secs(1);
        assert!(bucket.take(second));
        assert!(!bucket.take(second));
        let later = second + Duration::from_secs(600);
        assert_eq!((0..100).filter(|_| bucket.take(later)).count(), 60);
    }

    #[tokio::test]
    async fn a_scan_that_panics_still_lets_the_next_one_run() {
        let scanning = Arc::new(AtomicBool::new(false));
        let guard = Scanning::begin(&scanning).expect("no scan runs");
        assert!(Scanning::begin(&scanning).is_none(), "one at a time");
        let task = tokio::spawn(async move {
            let _scanning = guard;
            panic!("the scan failed");
        });
        assert!(task.await.is_err());
        assert!(
            Scanning::begin(&scanning).is_some(),
            "the flag was put down"
        );
    }

    #[test]
    fn a_connection_that_misses_two_pongs_is_let_go() {
        let mut alive = Liveness::default();
        assert!(alive.ping(), "first ping");
        assert!(alive.ping(), "one missed pong is forgiven");
        assert!(!alive.ping(), "two missed pongs end it");
        let mut alive = Liveness::default();
        for _ in 0..10 {
            assert!(alive.ping());
            alive.pong();
        }
    }

    #[test]
    fn debug_output_never_shows_what_a_phone_sent_or_was_sent() {
        let secret = "f00dfeed".repeat(8);
        let message = ConnEvent::Message {
            conn: 3,
            text: Some(format!("{{\"type\":\"hello\",\"token\":\"{secret}\"}}")),
        };
        let paired = Out::Text(format!("{{\"type\":\"paired\",\"token\":\"{secret}\"}}"));
        for shown in [format!("{message:?}"), format!("{paired:?}")] {
            assert!(!shown.contains(&secret), "{shown}");
            assert!(shown.contains("bytes"), "{shown}");
        }
    }
}
