//! The hub's network side: an authenticated WebSocket API on `127.0.0.1` for phones, and a
//! local control socket, `<run>/hub.sock`, for the CLI.
//!
//! # Connections
//!
//! The handshake turns away any request whose `Tailscale-User-Login` header is not exactly the
//! owner's login (403). A connection then starts unauthenticated and accepts only `hello` (a
//! device token) and `pair` (a pairing secret); anything else, or a failed `hello` or `pair`,
//! is refused and the socket closed, as is one silent for 30 seconds; at most 8 may wait at
//! once. After `hello` it is authenticated as that device: every frame, text or not, first
//! checks that the device is still paired (a revoked one is dropped at once) and takes a token
//! from the device's bucket (60 a minute; an empty bucket refuses, it does not close). Every
//! connection is pinged every 20 seconds, and one that misses two pongs is let go.
//! Bridge commands go to the session under a hub-wide req and the reply comes back under the
//! phone's own; one unanswered for 30 seconds is refused for it.
//!
//! # Subscriptions
//!
//! A subscription registers first and then replays the journals page by page in its own task,
//! which writes through the connection's replay queue and waits for room there. Live events
//! for the session meanwhile wait in a capped buffer (a phone that overflows it is dropped, and
//! resumes from its cursor). When the connection's writer has sent the last replayed frame it
//! reports the high-water mark, `(index of the journal, seq)` of the last record replayed (or
//! the cursor, when nothing was), and the hub sends the buffered events past that mark, acks
//! the subscription and streams live. The replay reads files to their end while the tail polls,
//! so live events at or below the mark can still follow: they are dropped until the first one
//! past it. Each replay page resumes at the byte where the last stopped. Live frames go through
//! a queue of 512: a phone that lets it fill is dropped.
use crate::{
    launch::{self, StartRequest, clip as clip_to},
    owner::tailnet_url,
    sessions::{SessionUpdate, Sessions},
    store::{Device, Store},
    tail::{self, file_name},
};
use futures_util::{SinkExt, StreamExt};
use octet_core::remote::{
    protocol::{
        BridgeCommand, Cursor, MAX_MESSAGE, PhoneCommand, ack, catalog_frame, event_frame,
        full_access_confirmed, paired, parse_phone, recent_frame, refused, req_of, sessions_frame,
        state_frame, welcome,
    },
    run::run_dir,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    sync::{mpsc, oneshot},
    task::AbortHandle,
    time::{Instant, Interval, MissedTickBehavior, interval_at, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{Callback, ErrorResponse, Request, Response},
        http::StatusCode,
        protocol::WebSocketConfig,
    },
};

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
/// How long the WebSocket handshake may take.
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(10);
/// How long the closing handshake may take.
const CLOSE_WITHIN: Duration = Duration::from_secs(2);
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
/// The refusal for a frame that is not text.
const NOT_TEXT: &str = "Send each message as a text frame";
/// The refusal for what a later task adds.
const NOT_YET: &str = "Not yet supported";
/// The refusal for a command the session did not answer in time.
const NO_ANSWER: &str = "The session didn't answer";
/// The refusal for `full-access` without the typed confirmation.
const FULL_ACCESS: &str = "Full access needs confirm: \"full access\"";
/// The longest workspace path kept in an audit line, in characters.
const AUDIT_PATH: usize = 200;
/// The longest phone-chosen text (a mode or model name) kept in an audit line, in characters.
const AUDIT_TEXT: usize = 64;

/// What a connection task tells the hub loop.
enum ConnEvent {
    /// A frame arrived: its text, or `None` for one that is not text.
    Message { conn: u64, text: Option<String> },
    /// The writer sent a replay's last frame; `last` is the last record it held.
    Replayed {
        conn: u64,
        session: String,
        generation: u64,
        last: Option<(String, u64)>,
    },
    /// A background job (a scan or a launch) finished: `frame` is the answer for `conn`.
    Answer {
        conn: u64,
        frame: Value,
        /// Whether it was a launch, which stops counting as one in flight.
        launch: bool,
        /// The audit line to write: device, session and what.
        audit: Option<(String, String, String)>,
    },
    /// A replay found no room for too long.
    Stalled { conn: u64 },
    /// The connection ended.
    Closed { conn: u64 },
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
                last,
            } => f
                .debug_struct("Replayed")
                .field("conn", conn)
                .field("session", session)
                .field("generation", generation)
                .field("last", last)
                .finish(),
            Self::Answer {
                conn,
                frame,
                launch,
                audit,
            } => f
                .debug_struct("Answer")
                .field("conn", conn)
                .field("frame", &Redacted(frame.to_string().len()))
                .field("launch", launch)
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

/// What a replay task has a connection's writer send.
#[derive(Debug)]
enum ReplayItem {
    /// A replayed event frame.
    Frame(String),
    /// The replay is over: the writer reports it once everything before it is sent.
    Done {
        session: String,
        generation: u64,
        last: Option<(String, u64)>,
    },
}

/// Where one subscription is.
#[derive(Debug)]
enum Subscription {
    /// Replaying: live events wait in `buffer` as `(journal, seq, frame)`.
    Replaying {
        generation: u64,
        req: u64,
        journals: Vec<String>,
        after: Option<Cursor>,
        buffer: Vec<(String, u64, String)>,
        task: AbortHandle,
    },
    /// Streaming live. Until a live event past `mark`, the high-water mark of the replay as
    /// `(index in journals, seq)`, arrives, events at or below it are repeats of the replay
    /// (the tail had not read them yet) and are dropped.
    Live {
        mark: Option<(usize, u64)>,
        journals: Vec<String>,
    },
}

impl Subscription {
    /// Whether a live event of `journal` and `seq` goes to the phone; the first past the mark
    /// clears it.
    fn admit(&mut self, journal: &str, seq: u64) -> bool {
        match self {
            Self::Live { mark, journals } => {
                let Some(at) = *mark else {
                    return true;
                };
                if !is_past(journals, at, journal, seq) {
                    return false;
                }
                *mark = None;
                true
            }
            Self::Replaying { .. } => false,
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Self::Replaying { task, .. } = self {
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
    /// The data directory: journals, the run directory and the hub's files.
    directory: PathBuf,
    /// Launches started and not yet finished; they count toward the phone-started cap.
    launching: usize,
    /// Given to each connection and replay task.
    events: mpsc::Sender<ConnEvent>,
    /// The last id given to a connection, hub req or replay.
    ids: u64,
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
        let (streams, mut accepted) = mpsc::channel(16);
        let accept_task = tokio::spawn(accept_tcp(listener, streams)).abort_handle();
        let (requests, mut controls) = mpsc::channel(16);
        let control_task = tokio::spawn(serve_control(control, requests)).abort_handle();
        let mut hub = Hub {
            store,
            sessions: Sessions::new(run),
            directory,
            launching: 0,
            owner,
            port,
            conns: HashMap::new(),
            pending: HashMap::new(),
            buckets: HashMap::new(),
            listed: Vec::new(),
            events,
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
        loop {
            // `refresh` is not cancel-safe, so it runs in a handler, never as a branch.
            tokio::select! {
                () = &mut shutdown => break,
                Some(stream) = accepted.recv() => hub.accept(stream),
                Some(event) = inbox.recv() => hub.on_event(event).await,
                update = hub.sessions.next() => hub.on_update(update),
                Some(request) = controls.recv() => hub.on_control(request).await,
                _ = refresh.tick() => hub.sessions.refresh().await,
                _ = reload.tick() => hub.reload().await,
                _ = sweep.tick() => hub.sweep(),
            }
        }
        control_task.abort();
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

    /// Starts the task that serves one TCP connection.
    fn accept(&mut self, stream: TcpStream) {
        let waiting = self.conns.values().filter(|c| c.device.is_none()).count();
        if self.conns.len() >= MAX_CONNECTIONS || waiting >= MAX_WAITING {
            return;
        }
        let conn = self.next_id();
        let (out, outbox) = mpsc::channel(OUTBOUND_QUEUE);
        let (replay, replays) = mpsc::channel(REPLAY_QUEUE);
        let task = tokio::spawn(connection(
            stream,
            self.owner.clone(),
            conn,
            outbox,
            replays,
            self.events.clone(),
        ))
        .abort_handle();
        self.conns.insert(
            conn,
            Conn {
                out,
                replay,
                task,
                opened: Instant::now(),
                device: None,
                subscriptions: HashMap::new(),
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
                last,
            } => self.on_replayed(conn, &session, generation, last),
            ConnEvent::Answer {
                conn,
                frame,
                launch,
                audit,
            } => {
                if launch {
                    self.launching = self.launching.saturating_sub(1);
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
                self.refuse(conn, 0, NOT_TEXT);
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
                self.refuse(conn, req_of(text), &reason);
                self.close(conn);
                return;
            }
        };
        let now = SystemTime::now();
        match command {
            PhoneCommand::Hello { token } => {
                // A revoke by the CLI must be seen before the token is believed.
                self.store.reload_if_changed().await;
                let Some(device) = self.store.authenticate(&token, &self.owner, now).await else {
                    self.refuse(conn, req, "Unknown device; pair it again");
                    self.close(conn);
                    return;
                };
                self.send(conn, &welcome(&device.name));
                let rows = self.sessions.list();
                self.send(conn, &sessions_frame(&rows));
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.device = Some(device);
                }
            }
            PhoneCommand::Pair {
                secret,
                name,
                platform,
            } => match self
                .store
                .pair(&secret, &name, &platform, &self.owner, now)
                .await
            {
                Ok((token, device)) => self.send(conn, &paired(&token, &device.name)),
                Err(reason) => {
                    self.refuse(conn, req, reason);
                    self.close(conn);
                }
            },
            _ => {
                self.refuse(conn, req, "Say hello first");
                self.close(conn);
            }
        }
    }

    /// A frame from a device that said hello.
    /// `None` is a frame that was not text: it spends a command and is refused.
    async fn authenticated(&mut self, conn: u64, device: &Device, text: Option<&str>) {
        self.store.reload_if_changed().await;
        if !self.paired(device) {
            // Revoked: nothing more is written to it.
            self.drop_conn(conn);
            return;
        }
        let req = text.map_or(0, req_of);
        if !self.take_token(&device.token_sha256) {
            self.refuse(conn, req, RATE_REFUSED);
            return;
        }
        let Some(text) = text else {
            self.refuse(conn, 0, NOT_TEXT);
            return;
        };
        let (req, command) = match parse_phone(text) {
            Ok(parsed) => parsed,
            Err(reason) => {
                self.refuse(conn, req_of(text), &reason);
                return;
            }
        };
        let (session, command, what) = match command {
            PhoneCommand::Hello { .. } | PhoneCommand::Pair { .. } => {
                self.refuse(conn, req, "Already connected");
                return;
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
                self.refuse(conn, req, FULL_ACCESS);
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
                self.refuse(conn, req, FULL_ACCESS);
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

    /// Lists resumable sessions in the background; the scan reads up to 400 journals.
    fn recent(&self, conn: u64, req: u64, workspace: Option<String>) {
        let events = self.events.clone();
        let directory = self.directory.clone();
        let configured = self.store.config().workspaces.clone();
        tokio::spawn(async move {
            let allowed = launch::allowed_workspaces(&directory, &configured).await;
            let wanted = workspace.as_deref().map(Path::new);
            let frame = match launch::recent(&directory, &allowed, wanted).await {
                Ok(items) => recent_frame(&items),
                Err(reason) => refused(req, &reason),
            };
            let done = ConnEvent::Answer {
                conn,
                frame,
                launch: false,
                audit: None,
            };
            let _ = events.send(done).await;
        });
    }

    /// Starts a terminal UI in tmux in the background and answers when tmux has finished.
    fn start(&mut self, conn: u64, device: &str, req: u64, request: StartRequest) {
        let running = self.sessions.phone_started() + self.launching;
        self.launching += 1;
        let events = self.events.clone();
        let directory = self.directory.clone();
        let configured = self.store.config().workspaces.clone();
        let device = device.to_owned();
        tokio::spawn(async move {
            let result = launch::start(&directory, &configured, &request, running).await;
            let (frame, audit) = match result {
                Ok(started) => {
                    let place = started.workspace.to_string_lossy();
                    let what = format!(
                        "start {} {} resume={} mode={}",
                        request.engine,
                        clip_to(&place, AUDIT_PATH),
                        if request.resume.is_some() {
                            "yes"
                        } else {
                            "no"
                        },
                        request.mode.as_deref().unwrap_or("default"),
                    );
                    let text = format!("Starting {} in {place}", started.name);
                    (ack(req, &text), Some((device, started.name, what)))
                }
                Err(reason) => (refused(req, &reason), None),
            };
            let done = ConnEvent::Answer {
                conn,
                frame,
                launch: true,
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
            Err(reason) => self.refuse(conn, req, &reason),
        }
    }

    /// Starts a subscription: the last state and catalog now, then the replay in its own task.
    fn subscribe(&mut self, conn: u64, req: u64, session: String, after: Option<Cursor>) {
        let Some(journals) = self.sessions.journals(&session) else {
            self.refuse(conn, req, "No such session");
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
        let names = journals.iter().map(|j| file_name(j)).collect();
        let job = ReplayJob {
            conn,
            journals,
            after: after.clone(),
            session: session.clone(),
            generation,
            out: state.replay.clone(),
            events: self.events.clone(),
        };
        let task = tokio::spawn(replay(job)).abort_handle();
        let subscription = Subscription::Replaying {
            generation,
            req,
            journals: names,
            after,
            buffer: Vec::new(),
            task,
        };
        // A subscription it replaces stops its replay as it drops.
        state.subscriptions.insert(session, subscription);
        for frame in frames {
            self.push(conn, frame);
        }
    }

    /// A replay's frames are all sent: the buffered events past its high-water mark follow,
    /// then the ack, and the subscription is live.
    fn on_replayed(
        &mut self,
        conn: u64,
        session: &str,
        generation: u64,
        last: Option<(String, u64)>,
    ) {
        let Some(state) = self.conns.get_mut(&conn) else {
            return;
        };
        let Some(subscription) = state.subscriptions.get_mut(session) else {
            return;
        };
        let Subscription::Replaying {
            generation: current,
            req,
            journals,
            after,
            buffer,
            ..
        } = subscription
        else {
            return;
        };
        if *current != generation {
            return;
        }
        let mark = last.or_else(|| after.take().map(|c| (c.journal, c.seq)));
        let mark = mark_index(journals, mark);
        let fresh = past_mark(std::mem::take(buffer), journals, mark);
        let req = *req;
        *subscription = Subscription::Live {
            mark,
            journals: std::mem::take(journals),
        };
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
                    refused(pending.req, &message)
                };
                self.send(pending.conn, &frame);
            }
            SessionUpdate::Changed => self.relist(),
        }
    }

    /// A live journal record: streamed to live subscribers, buffered for replaying ones.
    fn on_journal_event(&mut self, id: &str, journal: &str, seq: u64, record: &Value) {
        let frame = event_text(id, journal, seq, record);
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
            self.drop_conn(conn);
        }
    }

    /// Refuses, for their phones, the commands no session answered in time, and closes
    /// connections that never said hello.
    fn sweep(&mut self) {
        let now = Instant::now();
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
                self.refuse(pending.conn, pending.req, NO_ANSWER);
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

    /// Queues a refusal of `req` for `conn`.
    fn refuse(&mut self, conn: u64, req: u64, reason: &str) {
        self.send(conn, &refused(req, reason));
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

    /// Ends `conn` now.
    fn drop_conn(&mut self, conn: u64) {
        if let Some(state) = self.forget(conn) {
            state.task.abort();
        }
    }

    /// Removes `conn` and its unanswered commands; its replays stop as it drops.
    fn forget(&mut self, conn: u64) -> Option<Conn> {
        let state = self.conns.remove(&conn)?;
        self.pending.retain(|_, pending| pending.conn != conn);
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

/// A high-water mark `(journal, seq)` as `(index in journals, seq)`; none for a journal the
/// replay did not read (or no mark), so that everything counts as past it.
fn mark_index(journals: &[String], mark: Option<(String, u64)>) -> Option<(usize, u64)> {
    let (journal, seq) = mark?;
    Some((journals.iter().position(|j| *j == journal)?, seq))
}

/// Whether the record `seq` of `journal` comes after `mark` in a replay of `journals`: later
/// in the same journal, or in a later one. A journal the replay did not know is newer than all
/// it read.
fn is_past(journals: &[String], mark: (usize, u64), journal: &str, seq: u64) -> bool {
    journals
        .iter()
        .position(|j| j == journal)
        .is_none_or(|index| (index, seq) > mark)
}

/// The frames of `buffer`, `(journal, seq, frame)`, past `mark` (see [`is_past`]); with no
/// mark, all of them.
fn past_mark(
    buffer: Vec<(String, u64, String)>,
    journals: &[String],
    mark: Option<(usize, u64)>,
) -> Vec<String> {
    buffer
        .into_iter()
        .filter(|(journal, seq, _)| mark.is_none_or(|mark| is_past(journals, mark, journal, *seq)))
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

/// Creates `run` owner-only, and tightens it to 0700 if it exists with more.
async fn private_dir(run: &Path) -> std::io::Result<()> {
    octet_store::create_private_dir(run).await?;
    if tokio::fs::metadata(run).await?.permissions().mode() & 0o077 != 0 {
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

/// Accepts TCP connections and hands them to the hub loop.
async fn accept_tcp(listener: TcpListener, hub: mpsc::Sender<TcpStream>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                if hub.send(stream).await.is_err() {
                    return;
                }
            }
            // Out of descriptors, say: wait rather than spin.
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// Accepts control clients, each answered in its own task.
async fn serve_control(listener: UnixListener, hub: mpsc::Sender<Control>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(control_client(stream, hub.clone()));
            }
            // Out of descriptors, say: wait rather than spin.
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// Reads one request line from a control client and writes one answer line.
async fn control_client(stream: UnixStream, hub: mpsc::Sender<Control>) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(CONTROL_LIMIT));
    if !timeout(CONTROL_WITHIN, reader.read_line(&mut line))
        .await
        .is_ok_and(|read| read.is_ok_and(|n| n > 0))
    {
        return;
    }
    let answer = control_answer(&line, &hub).await;
    let _ = timeout(
        CONTROL_WITHIN,
        write.write_all(format!("{answer}\n").as_bytes()),
    )
    .await;
}

/// The answer to one control request.
async fn control_answer(line: &str, hub: &mpsc::Sender<Control>) -> Value {
    let request = serde_json::from_str::<Value>(line).ok();
    let kind = request
        .as_ref()
        .and_then(|r| r.get("type")?.as_str())
        .unwrap_or_default();
    let gone = || json!({"error": "The hub is stopping"});
    match kind {
        "pair_secret" => {
            let (reply, secret) = oneshot::channel();
            if hub.send(Control::PairSecret(reply)).await.is_err() {
                return gone();
            }
            let Ok(secret) = secret.await else {
                return gone();
            };
            json!({"secret": secret, "url": tailnet_url().await})
        }
        "status" => {
            let (reply, status) = oneshot::channel();
            if hub.send(Control::Status(reply)).await.is_err() {
                return gone();
            }
            let Ok((port, devices)) = status.await else {
                return gone();
            };
            let url = tailnet_url().await;
            json!({"port": port, "published": url.is_some(), "url": url, "devices": devices})
        }
        // Task 9: `watch` streams sessions and events to `octet hub watch`.
        "watch" => json!({"error": NOT_YET}),
        _ => json!({"error": "Unknown request"}),
    }
}

/// What one replay task needs.
#[derive(Debug)]
struct ReplayJob {
    conn: u64,
    journals: Vec<PathBuf>,
    after: Option<Cursor>,
    session: String,
    generation: u64,
    out: mpsc::Sender<ReplayItem>,
    events: mpsc::Sender<ConnEvent>,
}

/// Replays a session's journals after the cursor, a page at a time, into the connection's
/// replay queue, then marks the end. A phone with no room for too long is reported stalled.
async fn replay(job: ReplayJob) {
    let ReplayJob {
        conn,
        journals,
        mut after,
        session,
        generation,
        out,
        events,
    } = job;
    let mut last = None;
    let mut offset = 0;
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
            let frame = ReplayItem::Frame(event_text(&session, journal, *seq, record));
            if !timeout(WRITE_STALL, out.send(frame))
                .await
                .is_ok_and(|sent| sent.is_ok())
            {
                let _ = events.send(ConnEvent::Stalled { conn }).await;
                return;
            }
        }
        last = Some((next.journal.clone(), next.seq));
        after = Some(next);
        if !more {
            break;
        }
    }
    let done = ReplayItem::Done {
        session,
        generation,
        last,
    };
    if !timeout(WRITE_STALL, out.send(done))
        .await
        .is_ok_and(|sent| sent.is_ok())
    {
        let _ = events.send(ConnEvent::Stalled { conn }).await;
    }
}

/// Serves one TCP connection: the handshake, then frames both ways until either side ends.
async fn connection(
    stream: TcpStream,
    owner: String,
    conn: u64,
    mut outbox: mpsc::Receiver<Out>,
    mut replays: mpsc::Receiver<ReplayItem>,
    events: mpsc::Sender<ConnEvent>,
) {
    if let Some(ws) = handshake(stream, &owner).await {
        pump(ws, conn, &mut outbox, &mut replays, &events).await;
    }
    let _ = events.send(ConnEvent::Closed { conn }).await;
}

/// The WebSocket handshake, refused with 403 unless `Tailscale-User-Login` is `owner`.
async fn handshake(stream: TcpStream, owner: &str) -> Option<WebSocketStream<TcpStream>> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    timeout(
        HANDSHAKE_WITHIN,
        accept_hdr_async_with_config(stream, OwnerOnly(owner), Some(config)),
    )
    .await
    .ok()?
    .ok()
}

/// The handshake check: the request must carry `Tailscale-User-Login` equal to the owner's.
#[derive(Debug)]
struct OwnerOnly<'a>(&'a str);

impl Callback for OwnerOnly<'_> {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let login = request
            .headers()
            .get("Tailscale-User-Login")
            .and_then(|value| value.to_str().ok());
        if login == Some(self.0) {
            return Ok(response);
        }
        let mut forbidden = ErrorResponse::new(Some("Forbidden".into()));
        *forbidden.status_mut() = StatusCode::FORBIDDEN;
        Err(forbidden)
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
                Some(ReplayItem::Frame(frame)) => {
                    if !write(&mut ws, frame).await {
                        return;
                    }
                }
                Some(ReplayItem::Done { session, generation, last }) => {
                    let done = ConnEvent::Replayed { conn, session, generation, last };
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
    fn live_events_past_the_high_water_mark_are_kept() {
        let journals = ["a".to_owned(), "b".to_owned()];
        let buffer = || buffered(&[("a", 4), ("a", 5), ("b", 0), ("b", 1), ("c", 0)]);
        // Replayed up to a/5: the rest of a is a repeat; b and the unknown c are new.
        let kept = past_mark(buffer(), &journals, Some((0, 5)));
        assert_eq!(kept, ["b0", "b1", "c0"]);
        // Replayed into b: everything of a is older.
        let kept = past_mark(buffer(), &journals, Some((1, 0)));
        assert_eq!(kept, ["b1", "c0"]);
        // Nothing replayed and no cursor, or a mark in no journal of the replay: all are new.
        assert_eq!(past_mark(buffer(), &journals, None).len(), 5);
        assert_eq!(
            past_mark(
                buffer(),
                &journals,
                mark_index(&journals, Some(("gone".into(), 9)))
            )
            .len(),
            5
        );
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

    #[test]
    fn a_live_subscription_drops_repeats_until_past_its_mark() {
        // The replay ended at a/5, and the tail had not yet emitted a/4 and a/5.
        let journals = vec!["a".to_owned(), "b".to_owned()];
        let mut live = Subscription::Live {
            mark: Some((0, 5)),
            journals,
        };
        assert!(!live.admit("a", 4));
        assert!(!live.admit("a", 5));
        assert!(live.admit("a", 6));
        // Past the mark once, everything after is new.
        assert!(live.admit("b", 0));
        assert!(live.admit("a", 1));
        // A journal the replay did not know is past the mark.
        let mut live = Subscription::Live {
            mark: Some((1, 9)),
            journals: vec!["a".to_owned(), "b".to_owned()],
        };
        assert!(!live.admit("a", 99));
        assert!(live.admit("c", 0));
        let mut unmarked = Subscription::Live {
            mark: None,
            journals: Vec::new(),
        };
        assert!(unmarked.admit("a", 0));
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
