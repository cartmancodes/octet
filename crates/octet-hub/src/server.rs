//! The hub's network side: an authenticated WebSocket API on `127.0.0.1` for phones, and a
//! local control socket, `<run>/hub.sock`, for the CLI.
//!
//! # Connections
//!
//! A connection must finish its WebSocket upgrade within 5 seconds of being accepted, and at
//! most 32 upgrade at once (a new one lets the oldest go), so sockets that stall mid-request
//! cannot keep a phone out. The handshake turns away any request whose `Tailscale-User-Login`
//! header is not exactly the owner's login, or whose `Origin` names a host that is neither the
//! request's (`Host`, `X-Forwarded-Host`) nor that of the hub's published URL as last asked (a
//! web page of another site; the app sends none, or the hub's own), with 403, and one the hub
//! has no room for (64 open) with 503. A connection then starts unauthenticated and accepts
//! only `hello` (a device token) and `pair` (a pairing secret); anything else, or a failed
//! `hello` or `pair`, is refused and the socket closed, as is one silent for 10 seconds. At most
//! 32 wait for their `hello` at once: a newer one lets the oldest go if that one has waited 2
//! seconds, and is turned away with 503 (to try again) if every one is younger, so neither
//! sockets that never speak nor a storm of reconnects can push out a phone about to say hello.
//! After `hello` it is authenticated as that device: every frame, text or not, first checks
//! that the device is still paired (a revoked one is told `revoked`, then closed) and takes a
//! token from the device's bucket (60 a minute; an empty bucket refuses, it does not close).
//! Every connection is pinged every 20 seconds, and one that misses two pongs is let go.
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
//! replay page resumes at the byte where the last stopped, and the first page after a cursor
//! starts at the last mark at or below it of the hub's journal index (`tail::Index`),
//! which replays build as they read, so a phone back with its cursor does not have the whole
//! journal read again. Live frames go through a queue of 512: a phone that lets it fill is
//! dropped.
//!
//! # The sessions list
//!
//! A `sessions` frame goes to every authenticated phone when what a row shows changes, at most
//! four a second (a change after a quiet spell goes at once, the rest within a quarter second,
//! as one frame), and to a phone at its `hello`. It is made once and shared by every connection,
//! and it takes one place at most in a connection's queue: a newer one replaces one still
//! waiting there. Its size is bounded by [`Sessions::frame`].
//!
//! # Writes
//!
//! `devices.json` is written by one writer task, under its lock, in the order the writes were
//! asked for, so the hub loop never waits on a lock another process holds or on a sync to disk;
//! what a write was for (an ack, a pairing) is done once it is saved.
use crate::{
    activity::{Activity, Report},
    apns::{Outcome, Signer},
    git::{self, Git},
    launch::{self, Launches, StartRequest, clip as clip_to},
    owner::published_url,
    push::{self, Limiter, Pusher, Sender},
    sessions::{SessionUpdate, Sessions},
    store::{
        ActivityTokens, Change, Device, Disk, PAIR_NOT_SAVED, Pairing, SECRET_REFUSED, Store,
        activity_change, activity_clear, activity_token_drop, pair_change, push_change,
        push_token_drop, seen_change,
    },
    tail::{self, file_name},
};
use futures_util::{SinkExt, StreamExt};
use octet_core::PROMPT_LIMIT;
use octet_core::remote::{
    protocol::{
        BridgeCommand, Code, Cursor, MAX_MESSAGE, PhoneCommand, PushKinds, Secret, ack,
        catalog_frame, event_frame, full_access_confirmed, paired, parse_phone, recent_frame,
        refused, req_of, state_frame, subscribed_frame, welcome,
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
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::{mpsc, oneshot, watch},
    task::AbortHandle,
    time::{Instant, Interval, MissedTickBehavior, interval_at, sleep_until, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message, Utf8Bytes,
        handshake::server::{Callback, ErrorResponse, Request, Response},
        http::{HeaderMap, StatusCode},
        protocol::WebSocketConfig,
    },
};

mod git_commands;

use git_commands::Listings;

/// Frames waiting for one `octet hub watch`; a watcher that lets it fill is dropped.
const WATCH_QUEUE: usize = 256;
/// How many `octet hub watch` clients may be attached at once.
const MAX_WATCHERS: usize = 8;
/// Live frames waiting to be written to one phone; a phone that lets it fill is dropped. A
/// `sessions` frame takes one place at most: a newer one replaces it while it waits.
const OUTBOUND_QUEUE: usize = 512;
/// The least time between two `sessions` frames to the phones: at most four a second, whatever
/// the sessions do. A change after a quiet spell goes out at once; one within this of the last
/// frame waits for the rest of it, and goes with whatever else changed meanwhile.
const RELIST_GAP: Duration = Duration::from_millis(250);
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
const HELLO_WITHIN: Duration = Duration::from_secs(10);
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
/// Connections that may wait for their hello at once; a new one past it lets the oldest go, if
/// that one has waited [`WAIT_KEPT`], and is turned away (503) otherwise.
const MAX_WAITING: usize = 32;
/// How long a connection waiting for its hello is safe from being let go to make room: a phone
/// on a slow link takes a moment to say hello, and a storm of reconnects must not push it out.
const WAIT_KEPT: Duration = Duration::from_secs(2);
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
/// The refusal for a command whose session is still there but whose link closed before it
/// answered: the command may have run.
const LINK_LOST: &str = "Lost the link to that session; check it before sending again";
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
/// Refuses a Live Activity registration the store could not save.
const ACTIVITY_NOT_SAVED: &str = "Could not save the Live Activity; try again";
/// Refuses a `clear_activity` the store could not write.
const ACTIVITY_NOT_CLEARED: &str = "Could not update the Live Activity; try again";
/// Reports of dead push tokens waiting for the hub loop.
const PUSH_QUEUE: usize = 16;
/// Reports of Live Activity sends waiting for the hub loop.
const ACTIVITY_QUEUE: usize = 16;
/// `devices.json` writes done and waiting for the hub loop.
const SAVE_QUEUE: usize = 64;
/// Logged when a dead Live Activity token could not be dropped; it names no token or path.
const ACTIVITY_NOT_DROPPED: &str = "Couldn't drop a Live Activity token";
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
    /// A `changes` finished: the listing to keep for `session` and the frame, or the refusal;
    /// `ticket` orders it among the connection's listings.
    Listed {
        conn: u64,
        req: u64,
        ticket: u64,
        session: String,
        result: Result<git::Changes, git::Refusal>,
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
    /// A git call (a diff or a commit); its slot went back as it ended.
    Git,
}

/// A `devices.json` write made off the hub loop, by the writer task, and what the hub does once
/// it is made. The writes go one at a time, under the file's lock, in the order they were asked
/// for, and come back in that order.
#[derive(Debug)]
struct Saved {
    /// The file as the write left it, for the store in memory; none if the write failed.
    disk: Option<Disk>,
    then: AfterSave,
}

/// What a write was for, and how it went.
enum AfterSave {
    /// The time a hello was seen.
    Seen,
    /// A pairing on `conn`, answering `req`.
    Paired {
        conn: u64,
        req: u64,
        pairing: Pairing,
        result: std::io::Result<Result<(), &'static str>>,
    },
    /// `register_push` from `device` on `conn`, answering `req`.
    Push {
        conn: u64,
        req: u64,
        device: Device,
        token: Option<String>,
        kinds: Option<PushKinds>,
        result: std::io::Result<bool>,
    },
    /// `register_activity`, or `clear_activity` when `cleared`, from `device` on `conn`.
    Activity {
        conn: u64,
        req: u64,
        device: Device,
        cleared: bool,
        result: std::io::Result<bool>,
    },
    /// A Live Activity token let go after a send; `gone` when APNs said so (410).
    ActivityDropped {
        gone: bool,
        result: std::io::Result<Option<String>>,
    },
    /// A push token Expo says no device has.
    PushDropped {
        token: String,
        result: std::io::Result<Option<String>>,
    },
}

// Tokens and devices stay out: which write it was, only.
impl std::fmt::Debug for AfterSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Seen => "Seen",
            Self::Paired { .. } => "Paired",
            Self::Push { .. } => "Push",
            Self::Activity { .. } => "Activity",
            Self::ActivityDropped { .. } => "ActivityDropped",
            Self::PushDropped { .. } => "PushDropped",
        })
    }
}

/// What the hub loop has a connection's writer send, in order.
enum Out {
    /// A frame.
    Text(String),
    /// The `sessions` frame its [`Listing`] holds when the writer comes to it: the newest.
    Sessions,
    /// The closing handshake, then the end.
    Close,
}

/// The `sessions` frame waiting to be written to one phone, latest wins: while one waits, a newer
/// one takes its place rather than a place of its own in the queue. Every phone's frame is the
/// same text, shared.
type Listing = Arc<Mutex<Option<Utf8Bytes>>>;

/// Queues `frame` as the `sessions` frame `out`'s writer sends next: in `listing`, and, unless one
/// already waits there, as an [`Out::Sessions`] in `out`. False if `out` has no room or is gone.
fn offer_listing(out: &mpsc::Sender<Out>, listing: &Listing, frame: &Utf8Bytes) -> bool {
    let waiting = listing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(frame.clone())
        .is_some();
    waiting || out.try_send(Out::Sessions).is_ok()
}

/// The `sessions` frame waiting in `listing`, taking it.
fn take_listing(listing: &Listing) -> Option<Utf8Bytes> {
    listing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
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
            Self::Listed {
                conn,
                req,
                ticket,
                session,
                result,
            } => f
                .debug_struct("Listed")
                .field("conn", conn)
                .field("req", req)
                .field("ticket", ticket)
                .field("session", session)
                .field(
                    "result",
                    &result.as_ref().map(|c| c.info.files.len()).map_err(|e| e.0),
                )
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
            Self::Sessions => f.write_str("Sessions"),
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

/// The connections the hub holds. A handshake takes a [`Seat`] before it upgrades, so a socket
/// the hub has no room for (64 open) is turned away (503) rather than upgraded and then dropped,
/// and one that never finishes its request holds no place at all. The room also keeps the
/// connections not past `hello` yet, [`MAX_WAITING`] at most: a new one past that lets the
/// oldest of them go if it has waited [`WAIT_KEPT`] (the hub then drops it, see
/// [`Hub::accept`]), and is itself turned away (503) when every one is younger, so that neither
/// silent sockets nor a storm of reconnects can push out a phone about to say hello.
#[derive(Debug, Default)]
struct Room {
    open: AtomicUsize,
    /// The connections waiting for their hello.
    waiting: Mutex<Vec<Waiter>>,
    /// The last ticket given to a seat.
    tickets: AtomicU64,
}

/// A connection waiting for its hello, as the [`Room`] keeps it.
#[derive(Debug)]
struct Waiter {
    ticket: u64,
    since: Instant,
    /// Raised when the room let it go to make room for a newer one.
    evicted: Arc<AtomicBool>,
}

impl Room {
    /// A place for one more connection, waiting for its hello; `None` when the hub has
    /// [`MAX_CONNECTIONS`] open, or [`MAX_WAITING`] waiting none of which has waited
    /// [`WAIT_KEPT`].
    fn seat(self: &Arc<Self>) -> Option<Seat> {
        self.seat_at(Instant::now())
    }

    /// [`seat`](Self::seat), at `now`.
    fn seat_at(self: &Arc<Self>, now: Instant) -> Option<Seat> {
        if self.open.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            self.open.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        while waiting.len() >= MAX_WAITING {
            let oldest = waiting
                .iter()
                .enumerate()
                .min_by_key(|(_, waiter)| waiter.since)
                .filter(|(_, waiter)| now.saturating_duration_since(waiter.since) >= WAIT_KEPT)
                .map(|(at, _)| at);
            let Some(oldest) = oldest else {
                drop(waiting);
                self.open.fetch_sub(1, Ordering::AcqRel);
                return None;
            };
            waiting
                .swap_remove(oldest)
                .evicted
                .store(true, Ordering::Release);
        }
        let ticket = self.tickets.fetch_add(1, Ordering::AcqRel) + 1;
        let evicted = Arc::new(AtomicBool::new(false));
        waiting.push(Waiter {
            ticket,
            since: now,
            evicted: Arc::clone(&evicted),
        });
        Some(Seat {
            room: Arc::clone(self),
            ticket,
            evicted,
        })
    }

    /// The seat with `ticket` waits no more.
    fn leave_waiting(&self, ticket: u64) {
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|waiter| waiter.ticket != ticket);
    }
}

/// One connection's place in the [`Room`], given back when it drops.
#[derive(Debug)]
struct Seat {
    room: Arc<Room>,
    ticket: u64,
    /// Whether the room let it go, while it waited, to make room for a newer connection.
    evicted: Arc<AtomicBool>,
}

impl Seat {
    /// It said hello: it no longer waits, and is never let go to make room.
    fn admitted(&self) {
        self.room.leave_waiting(self.ticket);
    }

    /// Whether the room let it go to make room; the hub then drops its connection.
    fn evicted(&self) -> bool {
        self.evicted.load(Ordering::Acquire)
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.room.leave_waiting(self.ticket);
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
    /// The `sessions` frame for the writer, latest wins.
    listing: Listing,
    /// The last `sessions` frame it was given, by a list or at its hello.
    offered: Option<Utf8Bytes>,
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
    /// Whether its hello said it is the app in the background (handing a Live Activity token
    /// over, or sending a Deny): such a connection is never the app in front.
    background: bool,
    /// What its last `changes` of each session listed, which is all a `diff` may name.
    listings: Listings,
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
    /// The last `sessions` frame made, one text shared by every connection given it.
    listed: Utf8Bytes,
    /// When a `sessions` frame asked for within [`RELIST_GAP`] of the last goes out.
    relist_at: Option<Instant>,
    /// When the last `sessions` frame was made.
    relisted: Option<Instant>,
    /// Control clients running `watch`.
    watchers: Vec<mpsc::Sender<String>>,
    /// The `sessions` frame the watchers were last sent.
    watched: String,
    /// The data directory: journals, the run directory and the hub's files.
    directory: PathBuf,
    /// Launches that count toward the phone-started cap beyond the registrations.
    launches: Launches,
    /// Whether a `recent` scan is running; only one runs at a time (see [`Scanning`]).
    scanning: Arc<AtomicBool>,
    /// Runs git in sessions' workspaces, two calls at a time.
    git: Git,
    /// Whether each session with a commit under way may still commit, kept current as its
    /// state arrives; the commit looks again just before it stages.
    turns: HashMap<String, watch::Sender<bool>>,
    /// Where seqs are in the journals, for replays after a cursor.
    index: tail::SharedIndex,
    /// Given to each connection and replay task.
    events: mpsc::Sender<ConnEvent>,
    /// Makes the `devices.json` writes, one at a time, in the order they are asked for.
    writer: crate::store::Writer,
    /// Given to each `devices.json` write, which says here when it is done.
    saves: mpsc::Sender<Saved>,
    /// What to push, and when.
    pusher: Pusher,
    /// Posts pushes in the background.
    sender: Sender,
    /// Each device's sends in the last minute, pushes and Live Activity alike.
    cap: Limiter,
    /// The Live Activity while phones are away; none unless `hub.json` names APNs and its key
    /// loads.
    activity: Option<Activity>,
    /// Where the hub is published, as Tailscale last said: each push names its host as `mac`.
    published: Published,
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
        let Bound {
            store,
            run,
            control_path,
            control,
            listener,
            port,
        } = bind(&directory, port).await?;
        let (events, mut inbox) = mpsc::channel(EVENT_QUEUE);
        let (dead_tokens, mut dead) = mpsc::channel(PUSH_QUEUE);
        let (activity_reports, mut reports) = mpsc::channel(ACTIVITY_QUEUE);
        let (saves, mut saved) = mpsc::channel(SAVE_QUEUE);
        let activity = load_activity(&store, activity_reports).await;
        let (streams, mut accepted) = mpsc::channel(16);
        let room = Arc::new(Room::default());
        let (requests, mut controls) = mpsc::channel(16);
        let (published_in, published) = watch::channel(Publication::Unknown);
        let gate = Admission {
            owner: owner.clone(),
            room,
            published: published.clone(),
        };
        let accept_task = tokio::spawn(accept_tcp(listener, gate, streams)).abort_handle();
        let publish_task =
            tokio::spawn(watch_published(port, publisher, recheck, published_in)).abort_handle();
        let control_task =
            tokio::spawn(serve_control(control, requests, published.clone())).abort_handle();
        let writer = crate::store::Writer::start(directory.clone());
        let mut hub = Hub {
            store,
            sessions: Sessions::new(run),
            directory,
            launches: Launches::default(),
            scanning: Arc::new(AtomicBool::new(false)),
            git: Git::new(),
            turns: HashMap::new(),
            index: tail::SharedIndex::default(),
            owner,
            port,
            conns: HashMap::new(),
            pending: HashMap::new(),
            buckets: HashMap::new(),
            listed: Utf8Bytes::default(),
            relist_at: None,
            relisted: None,
            watchers: Vec::new(),
            watched: String::new(),
            events,
            writer,
            saves,
            pusher: Pusher::default(),
            sender: Sender::new(push::endpoint(), dead_tokens),
            cap: Limiter::default(),
            activity,
            published,
            ids: 0,
        };
        hub.sessions.refresh().await;
        hub.listed = hub.sessions.frame().into();
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
                _ = sweep.tick() => hub.tick(),
                Some(tokens) = dead.recv() => hub.drop_push_tokens(&tokens),
                Some(report) = reports.recv(), if hub.activity.is_some() => {
                    hub.on_activity_report(&report);
                }
                Some(done) = saved.recv() => hub.on_saved(done).await,
                () = sleep_until(hub.relist_at.unwrap_or_else(Instant::now)),
                    if hub.relist_at.is_some() => hub.flush_relist(Instant::now()),
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

    /// Starts the task that serves one upgraded connection, which brings its seat, and drops the
    /// waiting connections the room let go to make room for it.
    fn accept(&mut self, (ws, seat): Upgraded) {
        let conn = self.next_id();
        let (out, outbox) = mpsc::channel(OUTBOUND_QUEUE);
        let (replay, replays) = mpsc::channel(REPLAY_QUEUE);
        let listing = Listing::default();
        let writer = Writer {
            outbox,
            listing: Arc::clone(&listing),
            replays,
        };
        let task = tokio::spawn(connection(ws, conn, writer, self.events.clone())).abort_handle();
        self.conns.insert(
            conn,
            Conn {
                out,
                listing,
                offered: None,
                replay,
                task,
                opened: Instant::now(),
                device: None,
                subscriptions: HashMap::new(),
                background: false,
                listings: Listings::default(),
                seat,
            },
        );
        self.let_go_evicted();
    }

    /// Drops the connections the room let go, while they waited for their hello, to make room
    /// for newer ones (see [`Room`]): only a socket that has waited [`WAIT_KEPT`] goes.
    fn let_go_evicted(&mut self) {
        let evicted: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| state.device.is_none() && state.seat.evicted())
            .map(|(&conn, _)| conn)
            .collect();
        for conn in evicted {
            self.drop_conn(conn);
        }
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
                    // The scan's own guard put its flag down, as git's slot went back.
                    Job::Scan | Job::Git => {}
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
            ConnEvent::Listed {
                conn,
                req,
                ticket,
                session,
                result,
            } => self.on_listed(conn, req, ticket, session, result),
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
            PhoneCommand::Hello { token, background } => {
                // A revoke by the CLI must be seen before the token is believed.
                self.store.reload_if_changed().await;
                let Some((device, seen)) = self.store.recognise(token.expose(), &self.owner, now)
                else {
                    self.refuse(conn, req, Code::Unpaired, "Unknown device; pair it again");
                    self.close(conn);
                    return;
                };
                if let Some(seen) = seen {
                    // Off the loop; a device revoked meanwhile is let go at the next reload.
                    self.save(seen_change(&device.token_sha256, seen), |_| AfterSave::Seen);
                }
                let name = device.name.clone();
                // Judged on news from now. Any hello but a background one is the app in front,
                // which drives the Live Activity itself while it is; nothing ends.
                if let Some(activity) = self.activity.as_mut() {
                    activity.hello(&device.token_sha256);
                    if !background {
                        let tokens = device.activity.as_ref();
                        activity.connected(&device.token_sha256, tokens, now);
                    }
                }
                // Out of the waiting room before the phone hears it is in.
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.seat.admitted();
                    state.device = Some(device);
                    state.background = background;
                }
                self.send(conn, &welcome(&name));
                // A fresh hello is answered at once, whenever the last list went.
                let frame = self.sessions.frame().into();
                self.offer_sessions(conn, &frame);
            }
            PhoneCommand::Pair {
                secret,
                name,
                platform,
            } => match self
                .store
                .begin_pair(secret.expose(), &name, &platform, &self.owner, now)
            {
                // Answered once the device is saved (see `on_saved`).
                Ok(pairing) => {
                    let change = pair_change(pairing.device.clone());
                    self.save(change, move |result| AfterSave::Paired {
                        conn,
                        req,
                        pairing,
                        result,
                    });
                }
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
            PhoneCommand::RegisterPush { token, kinds } => {
                let token = token.map(|token| token.expose().to_owned());
                return self.register_push(conn, device, req, token, kinds);
            }
            command @ (PhoneCommand::RegisterActivity { .. } | PhoneCommand::ClearActivity) => {
                return self.activity(conn, device, req, command);
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
            command => match bridge_command(command) {
                Ok(parts) => parts,
                Err(command) => return self.git_command(conn, &device.name, req, *command),
            },
        };
        self.route(conn, device, req, &session, &command, &what)
            .await;
    }

    /// Sets, or with `None` clears, `device`'s push token, and its kinds when `kinds` says them
    /// (`None` keeps what is stored), off the loop, and answers `req` once it is saved
    /// ([`pushed`](Self::pushed)). Neither the token nor the device's hash reaches the audit line.
    fn register_push(
        &self,
        conn: u64,
        device: &Device,
        req: u64,
        token: Option<String>,
        kinds: Option<PushKinds>,
    ) {
        let change = push_change(&device.token_sha256, token.clone(), kinds);
        let device = device.clone();
        self.save(change, move |result| AfterSave::Push {
            conn,
            req,
            device,
            token,
            kinds,
            result,
        });
    }

    /// A push registration was saved, or not: the connections of the device take the token and
    /// kinds, and the phone is answered.
    async fn pushed(
        &mut self,
        (conn, req): (u64, u64),
        device: &Device,
        (token, kinds): (Option<String>, Option<PushKinds>),
        result: std::io::Result<bool>,
    ) {
        match result {
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
        let on = token.is_some();
        // The connections of this device carry a copy of it.
        for state in self.conns.values_mut() {
            if let Some(held) = state
                .device
                .as_mut()
                .filter(|held| held.token_sha256 == device.token_sha256)
            {
                held.push_token.clone_from(&token);
                if let Some(kinds) = kinds {
                    held.push_kinds = kinds;
                }
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

    /// Stores `device`'s `ActivityKit` tokens for `register_activity`, replacing both it had and
    /// taking them from any other device that held them, or removes them for `clear_activity`,
    /// off the loop; `req` is answered once that is saved ([`activity_saved`](Self::activity_saved)).
    /// Neither a token nor the device's hash reaches the audit line or a log.
    fn activity(&self, conn: u64, device: &Device, req: u64, command: PhoneCommand) {
        let hash = &device.token_sha256;
        let exposed = |token: Option<Secret>| token.map(|token| token.expose().to_owned());
        let (change, cleared) =
            if let PhoneCommand::RegisterActivity { token, start_token } = command {
                let activity = ActivityTokens {
                    token: exposed(token),
                    start_token: exposed(start_token),
                    // After 12 hours the token is spent: ActivityKit has ended that activity. The
                    // same token registered again keeps its first time.
                    registered: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .ok()
                        .map(|since| since.as_secs()),
                };
                (activity_change(hash, activity), false)
            } else {
                (activity_clear(hash), true)
            };
        let device = device.clone();
        self.save(change, move |result| AfterSave::Activity {
            conn,
            req,
            device,
            cleared,
            result,
        });
    }

    /// A Live Activity registration or clear was saved, or not: the driver forgets a cleared
    /// device's activity, the connections take the tokens, and the phone is answered.
    async fn activity_saved(
        &mut self,
        (conn, req): (u64, u64),
        device: &Device,
        cleared: bool,
        saved: std::io::Result<bool>,
    ) {
        let (message, what, failed) = if cleared {
            ("Activity cleared", "activity cleared", ACTIVITY_NOT_CLEARED)
        } else {
            (
                "Activity registered",
                "activity registered",
                ACTIVITY_NOT_SAVED,
            )
        };
        match saved {
            Ok(true) => {}
            Ok(false) => {
                self.refuse(conn, req, Code::Unavailable, failed);
                return;
            }
            Err(error) => {
                // The error names a kind, never a token or the device's hash.
                eprintln!("Couldn't save a phone's Live Activity: {}", error.kind());
                self.refuse(conn, req, Code::Unavailable, failed);
                return;
            }
        }
        // Forgotten now, not at the next look: a register just after it starts afresh.
        if cleared && let Some(activity) = self.activity.as_mut() {
            activity.cleared(&device.token_sha256);
        }
        self.copy_activity_tokens();
        self.store.audit(&device.name, "", what).await;
        self.send(conn, &ack(req, message));
    }

    /// Connections carry a copy of their device: each takes its activity tokens as stored, which
    /// may have left it for another device.
    fn copy_activity_tokens(&mut self) {
        let devices = self.store.devices();
        for held in self
            .conns
            .values_mut()
            .filter_map(|state| state.device.as_mut())
        {
            if let Some(stored) = devices.iter().find(|d| d.token_sha256 == held.token_sha256) {
                held.activity.clone_from(&stored.activity);
            }
        }
    }

    /// Makes `change` to `devices.json` off the loop, after every write asked for before it;
    /// once it is made, `then` says what the hub does about it ([`on_saved`](Self::on_saved)).
    fn save<R: Send + 'static>(
        &self,
        change: Change<R>,
        then: impl FnOnce(std::io::Result<R>) -> AfterSave + Send + 'static,
    ) {
        queue_save(&self.writer, &self.saves, change, then);
    }

    /// A write made off the loop is done: the store takes the file as it left it, and the hub
    /// does what the write was for.
    async fn on_saved(&mut self, Saved { disk, then }: Saved) {
        if let Some(disk) = disk {
            self.store.adopt(disk);
        }
        match then {
            AfterSave::Seen => {}
            AfterSave::Paired {
                conn,
                req,
                pairing,
                result,
            } => {
                let reason = match result {
                    Ok(Ok(())) => {
                        // Who paired, and when; never the token or the secret.
                        self.store.audit(&pairing.device.name, "", "paired").await;
                        self.send(conn, &paired(&pairing.token, &pairing.device.name));
                        return;
                    }
                    Ok(Err(reason)) => reason,
                    Err(_) => PAIR_NOT_SAVED,
                };
                // The phone may try again with the same code.
                self.store.unspend(pairing);
                self.refuse(conn, req, Code::Invalid, reason);
                self.close(conn);
            }
            AfterSave::Push {
                conn,
                req,
                device,
                token,
                kinds,
                result,
            } => {
                self.pushed((conn, req), &device, (token, kinds), result)
                    .await;
            }
            AfterSave::Activity {
                conn,
                req,
                device,
                cleared,
                result,
            } => {
                self.activity_saved((conn, req), &device, cleared, result)
                    .await;
            }
            AfterSave::ActivityDropped { gone, result } => {
                // A token APNs said is gone is audited; one the hub ended is just let go.
                match result {
                    Ok(Some(name)) if gone => {
                        self.store.audit(&name, "", "activity dropped").await;
                    }
                    Ok(_) => {}
                    Err(_) => eprintln!("{ACTIVITY_NOT_DROPPED}"),
                }
                self.copy_activity_tokens();
            }
            AfterSave::PushDropped { token, result } => {
                match result {
                    Ok(Some(name)) => self.store.audit(&name, "", "push dropped").await,
                    Ok(None) => {}
                    // The only push log lines are the ruled ones: no kind, path or token.
                    Err(_) => push::report(&[push::Outcome::Failed]),
                }
                for state in self.conns.values_mut() {
                    if let Some(held) = state
                        .device
                        .as_mut()
                        .filter(|held| held.push_token.as_ref() == Some(&token))
                    {
                        held.push_token = None;
                    }
                }
            }
        }
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
            index: Arc::clone(&self.index),
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
                self.update_turns();
                let subscribed = self
                    .conns
                    .values()
                    .any(|state| state.subscriptions.contains_key(&id));
                if subscribed && let Some(state) = self.sessions.state(&id) {
                    let frame = state_frame(&id, state).to_string();
                    self.tell_subscribers(&id, &frame);
                }
                // A row shows the state's phase, mode and approvals.
                self.relist();
                self.observe_sessions();
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
                self.update_turns();
                self.refuse_ended();
                self.relist();
                self.observe_sessions();
            }
        }
    }

    /// Refuses at once the commands waiting on a session whose link closed or that went away:
    /// no answer can come. A session still there may have run its command before the link
    /// closed, so the phone is told to look before sending it again.
    fn refuse_ended(&mut self) {
        let ended: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| !self.sessions.linked(&pending.session))
            .map(|(&req, _)| req)
            .collect();
        for req in ended {
            if let Some(pending) = self.pending.remove(&req) {
                let reason = if self.sessions.journals(&pending.session).is_some() {
                    LINK_LOST
                } else {
                    SESSION_ENDED
                };
                self.refuse(pending.conn, pending.req, Code::Unavailable, reason);
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

    /// Asks for the `sessions` frame to go to the phones and watchers: at once when none was
    /// made in the last [`RELIST_GAP`], otherwise once that has passed (see
    /// [`flush_relist`](Self::flush_relist)), so a storm of state changes costs at most four
    /// frames a second.
    fn relist(&mut self) {
        if self.relist_at.is_some() {
            return;
        }
        let now = Instant::now();
        match self.relisted.map(|last| last + RELIST_GAP) {
            Some(at) if at > now => self.relist_at = Some(at),
            _ => self.flush_relist(now),
        }
    }

    /// Makes the `sessions` frame once, and sends it to every authenticated phone (latest wins:
    /// see [`Listing`]) and to the watchers, each if it differs from what they were last given.
    /// Each phone is judged on its own last frame: one that said hello since the last list got a
    /// newer frame then, and must hear even of a list that went back to the last one.
    fn flush_relist(&mut self, now: Instant) {
        self.relist_at = None;
        self.relisted = Some(now);
        let frame = self.sessions.frame();
        if !self.watchers.is_empty() && frame != self.watched {
            broadcast(&mut self.watchers, &frame);
            self.watched.clone_from(&frame);
        }
        if frame != self.listed.as_str() {
            self.listed = frame.into();
        }
        let frame = self.listed.clone();
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| {
                state.device.is_some()
                    && state
                        .offered
                        .as_ref()
                        .is_none_or(|offered| offered.as_str() != frame.as_str())
            })
            .map(|(&conn, _)| conn)
            .collect();
        for conn in targets {
            self.offer_sessions(conn, &frame);
        }
    }

    /// Gives `conn` `frame` as the `sessions` frame its writer sends next; a connection whose
    /// queue is full is dropped.
    fn offer_sessions(&mut self, conn: u64, frame: &Utf8Bytes) {
        let Some(state) = self.conns.get_mut(&conn) else {
            return;
        };
        state.offered = Some(frame.clone());
        if !offer_listing(&state.out, &state.listing, frame) {
            self.drop_conn(conn);
        }
    }

    /// Attaches a watcher, or turns it away when too many are attached.
    fn watch(&mut self, watcher: mpsc::Sender<String>) {
        self.watchers.retain(|w| !w.is_closed());
        if self.watchers.len() >= MAX_WATCHERS {
            let _ = watcher.try_send(json!({"error": "Too many watchers"}).to_string());
            return;
        }
        if self.watchers.is_empty() {
            self.watched = self.sessions.frame();
        }
        if watcher.try_send(self.watched.clone()).is_ok() {
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
        self.let_go_evicted();
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

    /// Every second: refuses what timed out, and sends the pushes and Live Activity sends due.
    fn tick(&mut self) {
        self.sweep();
        self.flush_push();
        self.flush_activity();
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

    /// Hands the pusher and the Live Activity driver what the sessions look like; what the
    /// pusher finds new waits in a window for each device with a push token (whether it is
    /// connected is judged when the window closes).
    fn observe_sessions(&mut self) {
        let snapshots = self.sessions.snapshots();
        let notices = self.pusher.observe(&snapshots);
        let now = Instant::now();
        if let Some(activity) = self.activity.as_mut() {
            activity.observe(&snapshots, (now, SystemTime::now()));
        }
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
        self.pusher.prune();
        self.cap.prune(now);
        let mut batch = Vec::new();
        for key in self.pusher.waiting() {
            let known = self
                .store
                .devices()
                .iter()
                .find(|device| device.token_sha256 == key);
            let token = known.and_then(|device| device.push_token.clone());
            let kinds = known.map(|device| device.push_kinds).unwrap_or_default();
            let connected = token.is_none() || in_front(&self.conns, &key);
            let due = self
                .pusher
                .due_for(&key, connected, kinds, &mut self.cap, now);
            if let (Some(due), Some(token)) = (due, token) {
                batch.push((token, due));
            }
        }
        if batch.is_empty() {
            return;
        }
        let mac = push_mac(&self.published.borrow());
        self.sender.dispatch(batch, mac);
    }

    /// Sends what each device's Live Activity is due, in the background, while no connection of
    /// the device streams a session: an update to its activity, a start when it has none, an end
    /// once its sessions have all been idle long enough. Each send takes from the allowance
    /// pushes use.
    fn flush_activity(&mut self) {
        let Some(activity) = self.activity.as_mut() else {
            return;
        };
        let mac = push_mac(&self.published.borrow());
        let conns = &self.conns;
        activity.flush(
            self.store.devices(),
            |key| in_front(conns, key),
            &mut self.cap,
            mac.as_deref(),
            (Instant::now(), SystemTime::now()),
        );
    }

    /// Takes what a Live Activity send came to. A token APNs says is gone (410), or one whose
    /// activity the hub ended, is cleared, by value, from `devices.json` (off the loop) and then
    /// from the connections' copies of their devices.
    fn on_activity_report(&mut self, report: &Report) {
        let Some(activity) = self.activity.as_mut() else {
            return;
        };
        let stored = self
            .store
            .devices()
            .iter()
            .find(|device| device.token_sha256 == report.device)
            .and_then(|device| device.activity.as_ref()?.token.clone());
        if !activity.reported(report, stored.as_deref()) {
            return;
        }
        let gone = report.outcome == Outcome::Gone;
        self.save(activity_token_drop(&report.token), move |result| {
            AfterSave::ActivityDropped { gone, result }
        });
    }

    /// Clears push tokens Expo says no device has, from `devices.json` (off the loop) and then
    /// from the connections' copies of their devices.
    fn drop_push_tokens(&self, tokens: &[String]) {
        for token in tokens {
            let token = token.clone();
            self.save(push_token_drop(&token), move |result| {
                AfterSave::PushDropped { token, result }
            });
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
            // A background connection going was never the app in front.
            let still_here = state.background || in_front(&self.conns, key);
            // The app leaves the front: what its activity shows is current as of now.
            if !still_here && let Some(activity) = self.activity.as_mut() {
                let tokens = self
                    .store
                    .devices()
                    .iter()
                    .find(|known| &known.token_sha256 == key)
                    .and_then(|known| known.activity.as_ref());
                activity.left(key, tokens, (Instant::now(), SystemTime::now()));
            }
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

/// Queues `change` with `writer`, behind every write queued before it, and tells `saves` how it
/// went, as `then` puts it: the hub hears of the writes in the order they were asked for.
fn queue_save<R: Send + 'static>(
    writer: &crate::store::Writer,
    saves: &mpsc::Sender<Saved>,
    change: Change<R>,
    then: impl FnOnce(std::io::Result<R>) -> AfterSave + Send + 'static,
) {
    let saves = saves.clone();
    writer.queue(change, move |written| async move {
        let (result, disk) = match written {
            Ok((result, disk)) => (Ok(result), Some(disk)),
            Err(error) => (Err(error), None),
        };
        // The next write waits until the hub has this one's result.
        let _ = saves
            .send(Saved {
                disk,
                then: then(result),
            })
            .await;
    });
}

/// The session, the command and the audit text of a phone command that a terminal UI runs; any
/// other command comes back.
fn bridge_command(
    command: PhoneCommand,
) -> Result<(String, BridgeCommand, String), Box<PhoneCommand>> {
    match command {
        PhoneCommand::End { session } => Ok((session, BridgeCommand::End, "end".into())),
        PhoneCommand::Prompt { session, text } => {
            Ok((session, BridgeCommand::Prompt { text }, "prompt".to_owned()))
        }
        PhoneCommand::Answer {
            session,
            approval,
            allow,
        } => {
            let verdict = if allow { "allow" } else { "deny" };
            let what = format!("answer {approval} {verdict}");
            Ok((session, BridgeCommand::Answer { approval, allow }, what))
        }
        PhoneCommand::Cancel { session } => Ok((session, BridgeCommand::Cancel, "cancel".into())),
        PhoneCommand::Steer { session, text } => {
            Ok((session, BridgeCommand::Steer { text }, "steer".into()))
        }
        PhoneCommand::SetMode {
            session,
            mode,
            confirm,
        } => {
            let what = format!("set_mode {}", clip(&mode));
            Ok((session, BridgeCommand::SetMode { mode, confirm }, what))
        }
        PhoneCommand::SetModel { session, selection } => {
            let what = format!("set_model {}", clip(&selection));
            Ok((session, BridgeCommand::SetModel { selection }, what))
        }
        PhoneCommand::Command { session, name } => {
            let what = format!("command {}", name.as_str());
            Ok((session, BridgeCommand::Command { name }, what))
        }
        other => Err(Box::new(other)),
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

/// What the hub serves from: its files, the run directory, and both sockets, bound.
struct Bound {
    store: Store,
    run: PathBuf,
    control_path: PathBuf,
    control: UnixListener,
    listener: TcpListener,
    /// The loopback port it listens on.
    port: u16,
}

/// Opens the hub's files in `directory`, readies its run directory and binds the control
/// socket there and `127.0.0.1:<port>` (0 for any).
async fn bind(directory: &Path, port: u16) -> Result<Bound, String> {
    let store = Store::open(directory).await.map_err(|e| {
        format!(
            "Couldn't open the hub's files in {}: {e}",
            directory.display()
        )
    })?;
    let run = run_dir(directory);
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
    Ok(Bound {
        store,
        run,
        control_path,
        control,
        listener,
        port,
    })
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
async fn accept_tcp(listener: TcpListener, gate: Admission, hub: mpsc::Sender<Upgraded>) {
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
                let (gate, hub) = (gate.clone(), hub.clone());
                let task = tokio::spawn(async move {
                    if let Some(upgraded) = handshake(stream, &gate).await {
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

/// Whether the device whose token hash is `key` has the app in front: a connection of it whose
/// hello was not a background one, on any screen. The pusher and the Live Activity share it.
fn in_front(conns: &HashMap<u64, Conn>, key: &str) -> bool {
    conns.values().any(|conn| {
        !conn.background
            && conn
                .device
                .as_ref()
                .is_some_and(|device| device.token_sha256 == key)
    })
}

/// The Live Activity, when `hub.json` names APNs and its key loads (read once, off the hub's
/// thread); `None` otherwise, and then nothing of it runs. The key is never read again: a hub
/// whose key changes is restarted.
async fn load_activity(store: &Store, reports: mpsc::Sender<Report>) -> Option<Activity> {
    let config = store.config().apns.clone()?;
    let (config, signer) = tokio::task::spawn_blocking(move || {
        let signer = Signer::load(&config)?;
        Some((config, signer))
    })
    .await
    .ok()??;
    Some(Activity::new(config, signer, reports))
}

/// The `mac` a push names: the host of the hub's published URL, when it is published and that
/// host is a DNS name ([`push::mac_of`]); `None` otherwise.
fn push_mac(publication: &Publication) -> Option<String> {
    match publication {
        Publication::At(url) => push::mac_of(url),
        Publication::Unknown | Publication::Nowhere => None,
    }
}

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
    /// The hub's index of the journals, which the replay starts from and adds to.
    index: tail::SharedIndex,
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
        index,
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
        let tail::Page {
            records: page,
            more,
            end,
            ..
        } = tail::replay_from(&journals, after.as_ref(), offset, REPLAY_PAGE, Some(&index)).await;
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

/// What one connection's writer sends: the hub's live frames, its `sessions` frame, and the
/// replays' frames.
struct Writer {
    outbox: mpsc::Receiver<Out>,
    listing: Listing,
    replays: mpsc::Receiver<ReplayItem>,
}

/// Serves one upgraded connection: frames both ways until either side ends.
async fn connection(
    ws: WebSocketStream<TcpStream>,
    conn: u64,
    mut writer: Writer,
    events: mpsc::Sender<ConnEvent>,
) {
    pump(ws, conn, &mut writer, &events).await;
    let _ = events.send(ConnEvent::Closed { conn }).await;
}

/// What every handshake is checked against: the owner's login, the hub's room, and where the
/// hub is published, as `tailscale serve status` last said (the cache `status` and
/// `pair_secret` answer from; nothing is asked on a request's way in).
#[derive(Debug, Clone)]
struct Admission {
    owner: String,
    room: Arc<Room>,
    published: Published,
}

/// The WebSocket upgrade, refused as [`Gate`] says, within [`HANDSHAKE_WITHIN`]: the connection
/// and the seat it took.
async fn handshake(stream: TcpStream, admission: &Admission) -> Option<Upgraded> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let published = match &*admission.published.borrow() {
        Publication::At(url) => url.split_once("://").map(|(_, rest)| {
            let authority = rest.split('/').next().unwrap_or_default();
            host_of(authority).to_owned()
        }),
        _ => None,
    };
    let mut seat = None;
    let gate = Gate {
        owner: &admission.owner,
        room: &admission.room,
        published: published.as_deref(),
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
    /// The host of the hub's published URL, when it is published.
    published: Option<&'a str>,
    seat: &'a mut Option<Seat>,
}

impl Callback for Gate<'_> {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let headers = request.headers();
        let login = headers
            .get("Tailscale-User-Login")
            .and_then(|value| value.to_str().ok());
        if login != Some(self.owner) || foreign_origin(headers, self.published) {
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
/// host (any case, any port) is none of the `Host` the request names, the `X-Forwarded-Host`
/// that `tailscale serve` may add, and `published`, the host of the hub's published URL (the
/// app's socket names it whatever `tailscale serve` forwards as `Host`). A browser sends
/// `Origin` with every WebSocket and cannot set the others; the app's socket sends none, or
/// the hub's own address.
fn foreign_origin(headers: &HeaderMap, published: Option<&str>) -> bool {
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
    ![named("Host"), named("X-Forwarded-Host"), published]
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
    writer: &mut Writer,
    events: &mpsc::Sender<ConnEvent>,
) {
    let Writer {
        outbox,
        listing,
        replays,
    } = writer;
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
                    if current.load(Ordering::Acquire) && !write(&mut ws, frame.into()).await {
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
                    if !write(&mut ws, frame.into()).await {
                        return;
                    }
                }
                Some(Out::Sessions) => {
                    if let Some(frame) = take_listing(listing)
                        && !write(&mut ws, frame).await
                    {
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
async fn write(ws: &mut WebSocketStream<TcpStream>, frame: Utf8Bytes) -> bool {
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
    fn pushes_name_the_mac_only_while_the_hub_is_published() {
        let at = |url: &str| Publication::At(url.to_owned());
        assert_eq!(
            push_mac(&at("https://Studio.tail1234.ts.net")).as_deref(),
            Some("studio.tail1234.ts.net")
        );
        assert_eq!(push_mac(&Publication::Unknown), None);
        assert_eq!(push_mac(&Publication::Nowhere), None);
        // A published URL whose host is no DNS name is no `mac`.
        assert_eq!(push_mac(&at("https://user@studio.ts.net")), None);
        assert_eq!(push_mac(&at("https://-studio.ts.net")), None);
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
        assert!(!foreign_origin(&headers(&[host]), None), "no Origin");
        for same in [
            "https://mac.tail1234.ts.net",
            "https://MAC.tail1234.ts.net:443",
        ] {
            assert!(
                !foreign_origin(&headers(&[host, ("Origin", same)]), None),
                "{same}"
            );
        }
        for other in ["https://evil.example", "null", "mac.tail1234.ts.net", ""] {
            assert!(
                foreign_origin(&headers(&[host, ("Origin", other)]), None),
                "{other}"
            );
        }
        let v6 = headers(&[("Host", "[::1]:47823"), ("Origin", "http://[::1]:47823")]);
        assert!(!foreign_origin(&v6, None));
        let no_host = headers(&[("Origin", "https://mac.tail1234.ts.net")]);
        assert!(foreign_origin(&no_host, None));
    }

    #[test]
    fn an_origin_naming_the_published_host_is_never_foreign() {
        // What `tailscale serve` may forward: the loopback Host, no X-Forwarded-Host.
        let request = |origin: &'static str| {
            let mut map = HeaderMap::new();
            map.insert("Host", "127.0.0.1:47823".parse().unwrap());
            map.insert("Origin", origin.parse().unwrap());
            map
        };
        let published = Some("mac.qa.ts.net");
        for ours in ["https://mac.qa.ts.net", "https://MAC.qa.ts.net:443"] {
            assert!(!foreign_origin(&request(ours), published), "{ours}");
        }
        for other in ["https://evil.example", "https://other.qa.ts.net"] {
            assert!(foreign_origin(&request(other), published), "{other}");
        }
        // Not published: the request's own host alone names the hub.
        assert!(foreign_origin(&request("https://mac.qa.ts.net"), None));
        assert!(!foreign_origin(&request("http://127.0.0.1:47823"), None));
    }

    #[test]
    fn a_seat_is_given_back_and_only_open_connections_are_capped() {
        let room = Arc::new(Room::default());
        // Connections past hello take seats past the waiting room's size, up to the cap.
        let seats: Vec<Seat> = (0..MAX_CONNECTIONS)
            .filter_map(|_| room.seat())
            .inspect(Seat::admitted)
            .collect();
        assert_eq!(seats.len(), MAX_CONNECTIONS);
        assert!(room.seat().is_none(), "every connection is open");
        drop(seats);
        assert_eq!(room.open.load(Ordering::Acquire), 0);
        assert!(room.seat().is_some());
        // Ten seconds to say hello.
        assert_eq!(HELLO_WITHIN, Duration::from_secs(10));
    }

    #[test]
    fn a_waiting_socket_younger_than_two_seconds_is_never_let_go() {
        let room = Arc::new(Room::default());
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // Thirty-two wait, one every 10 ms.
        let waiting: Vec<Seat> = (0..MAX_WAITING as u64)
            .map(|n| room.seat_at(at(n * 10)).expect("room to wait"))
            .collect();
        // A newcomer while all are younger than 2 s: turned away, and nobody is let go.
        assert!(room.seat_at(at(1000)).is_none());
        assert!(waiting.iter().all(|seat| !seat.evicted()));
        assert_eq!(room.open.load(Ordering::Acquire), MAX_WAITING);
        // Once the oldest has waited 2 s, a newcomer lets it go, and only it.
        let newcomer = room.seat_at(at(2000)).expect("room once the oldest is old");
        assert!(waiting[0].evicted());
        assert!(waiting[1..].iter().all(|seat| !seat.evicted()));
        // The next has waited only 1.99 s: the room is full again.
        assert!(room.seat_at(at(2000)).is_none());
        // A connection that says hello leaves the room, and is never let go after.
        waiting[5].admitted();
        let another = room.seat_at(at(2001)).expect("the hello made room");
        assert!(
            room.seat_at(at(9000)).is_some(),
            "the 2nd oldest makes room"
        );
        assert!(waiting[1].evicted() && !waiting[5].evicted());
        // One that goes away frees its place.
        drop((newcomer, another));
        assert!(room.seat_at(at(2002)).is_some());
    }

    #[test]
    fn a_waiting_sessions_frame_is_replaced_not_queued() {
        let (out, mut outbox) = mpsc::channel(4);
        let listing = Listing::default();
        // A thousand lists while the writer is busy: one place in the queue, the newest frame.
        for n in 0..1000 {
            let frame = Utf8Bytes::from(format!("list {n}"));
            assert!(offer_listing(&out, &listing, &frame));
        }
        assert!(matches!(outbox.try_recv(), Ok(Out::Sessions)));
        assert!(outbox.try_recv().is_err(), "one place, however many lists");
        assert_eq!(take_listing(&listing).as_deref(), Some("list 999"));
        assert_eq!(take_listing(&listing), None);
        // Once it went, the next list takes a place again, after what was queued before it.
        out.try_send(Out::Text("ack".into())).unwrap();
        assert!(offer_listing(&out, &listing, &Utf8Bytes::from("list 1000")));
        assert!(matches!(outbox.try_recv(), Ok(Out::Text(text)) if text == "ack"));
        assert!(matches!(outbox.try_recv(), Ok(Out::Sessions)));
        // Every phone shares the one text.
        let shared = Utf8Bytes::from("shared".to_owned());
        let (other, _keep) = mpsc::channel(1);
        let theirs = Listing::default();
        assert!(offer_listing(&other, &theirs, &shared));
        let held = take_listing(&theirs).unwrap();
        assert_eq!(held.as_str().as_ptr(), shared.as_str().as_ptr());
        // A queue with no room left drops the phone, as any frame does.
        let (full, _queue) = mpsc::channel(1);
        full.try_send(Out::Close).unwrap();
        assert!(!offer_listing(&full, &Listing::default(), &shared));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_asked_for_back_to_back_land_in_that_order() {
        let dir = octet_testkit::TempDir::new("hub-writeorder");
        let mut store = Store::open(dir.path()).await.expect("the store");
        let secret = store.pairing_secret(SystemTime::now());
        let (_, device) = store
            .pair(
                &secret,
                "Order phone",
                "ios",
                "owner@example",
                SystemTime::now(),
            )
            .await
            .expect("paired");
        let tokens = |n: u64| ActivityTokens {
            token: Some(format!("{n:032x}")),
            start_token: None,
            registered: Some(n),
        };
        let (saves, mut saved) = mpsc::channel(SAVE_QUEUE);
        let writer = crate::store::Writer::start(dir.path().to_owned());
        for round in 0..20u64 {
            // Two registrations, with no await between them, from a task on a worker thread as
            // the hub loop is: the second is the one that stands.
            let (writer, saves, device) = (writer.clone(), saves.clone(), device.clone());
            tokio::spawn(async move {
                for n in [2 * round + 1, 2 * round + 2] {
                    let change = activity_change(&device.token_sha256, tokens(n));
                    let device = device.clone();
                    queue_save(&writer, &saves, change, move |result| AfterSave::Activity {
                        conn: n,
                        req: n,
                        device,
                        cleared: false,
                        result,
                    });
                }
            })
            .await
            .expect("queued");
            for want in [2 * round + 1, 2 * round + 2] {
                let done = saved.recv().await.expect("a write is done");
                let AfterSave::Activity { req, .. } = done.then else {
                    panic!("not the activity write")
                };
                assert_eq!(req, want, "round {round}: told out of order");
                store.adopt(done.disk.expect("saved"));
            }
            let reopened = Store::open(dir.path()).await.expect("the store");
            let held = reopened.devices()[0].activity.clone().expect("tokens");
            assert_eq!(
                held,
                tokens(2 * round + 2),
                "round {round}: written out of order"
            );
        }
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
