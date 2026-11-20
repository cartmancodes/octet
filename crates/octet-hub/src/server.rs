//! The hub's network side: phones through the relay ([`crate::relay`]), each over its own
//! end-to-end encrypted channel ([`crate::secure`]), and a local control socket,
//! `<run>/hub.sock`, for the CLI. The hub listens on no network port.
//!
//! # Connections
//!
//! A phone's connection arrives from the relay link past its Noise handshake, with the key the
//! handshake proved and the seat it took in the hub's room at its `connect`. It starts
//! unauthenticated and accepts only `hello` (from a known key) and `pair` (a pairing secret); an
//! unknown key that says hello is refused `unpaired` and closed, and anything else, or a failed
//! `pair`, is refused and closed, as is one with no accepted frame 10 seconds after its
//! `connect`. A good `pair` is held for the owner's answer at the Mac ([`pairing`]): the phone,
//! sent its code, waits up to two minutes past those 10 seconds, still counted as not past its
//! first frame, and is then told `paired` and closed (it says hello on a connection of its own),
//! or refused and closed. After `hello` it is authenticated as that device: every frame first
//! checks that the device is still paired (a revoked one is told `revoked`, then closed) and
//! takes a token from the device's bucket (60 a minute; an empty bucket refuses, it does not
//! close). Every connection is sent an encrypted ping every 20 seconds, and one whose last ping
//! is still unanswered when the next falls due is let go. Bridge commands go to the session
//! under a hub-wide req and the reply comes back under the phone's own; one unanswered for 30
//! seconds is refused for it.
//!
//! The hub dials the relay only while it is wanted: a device is paired, a pairing secret is out
//! and unexpired, or a phone is held for its answer; and for 3 seconds after a phone was revoked
//! or refused its pairing, so it hears that before its socket goes with the link.
//!
//! # The control socket
//!
//! `<run>/hub.sock` answers one request line with one answer line, the request read within 5
//! seconds: `status`; `watch` (a line for each change, until the client leaves); and pairing's
//! three. `pair_secret` opens a pairing (in place of one no phone has used) and gives its link
//! once the relay has the hub, waiting up to 10 seconds; `pair_wait` answers when a phone is held,
//! or the secret runs out, and ends if the client leaves (closes its side); `pair_answer` says yes
//! or no for the phone held under its `pending` id.
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
    keys::HubKeys,
    launch::{self, Launches, StartRequest, clip as clip_to},
    push::{self, Limiter, Pusher, Sender, Snapshot},
    relay::{self, Arrival, FIRST_FRAME_WITHIN, Log, RelayState, Room, Seat, Stream},
    secure::{Incoming, Secure},
    sessions::{SessionUpdate, Sessions},
    store::{
        ActivityTokens, Change, Device, Disk, Store, activity_change, activity_clear,
        activity_token_drop, hex, push_change, push_token_drop, seen_change,
    },
    tail::{self, file_name},
};
use octet_core::PROMPT_LIMIT;
use octet_core::remote::{
    protocol::{
        BridgeCommand, Code, Cursor, PhoneCommand, PushKinds, Secret, ack, catalog_frame,
        event_frame, full_access_confirmed, parse_phone, recent_frame, refused, req_of,
        state_frame, subscribed_frame, welcome,
    },
    run::run_dir,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    sync::{mpsc, oneshot, watch},
    task::AbortHandle,
    time::{Instant, Interval, MissedTickBehavior, interval_at, sleep_until, timeout},
};
use tokio_tungstenite::tungstenite::Utf8Bytes;

mod git_commands;
mod pairing;

use git_commands::Listings;
use pairing::{Opened, PHONE_WAITS, Pairings};

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
/// How long the closing handshake may take.
const CLOSE_WITHIN: Duration = Duration::from_secs(2);
/// How long a revoked phone has to take its refusal before its connection is cut.
const REVOKED_GRACE: Duration = Duration::from_secs(1);
/// How often a connection is sent an encrypted ping; one whose last ping is still unanswered
/// when the next falls due is let go.
const PING_EVERY: Duration = Duration::from_secs(20);
/// How long accepting control clients waits after a failure (out of descriptors, say) before
/// trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Messages from connection tasks waiting for the hub loop.
const EVENT_QUEUE: usize = 256;
/// Phones past their handshake waiting for the hub loop.
const ARRIVAL_QUEUE: usize = 16;
/// The longest Mac name `welcome` and `paired` carry, in characters.
const NAME_LIMIT: usize = 64;
/// The longest control request, in bytes.
const CONTROL_LIMIT: u64 = 4096;
/// How long a control client may take to ask or to read the answer.
const CONTROL_WITHIN: Duration = Duration::from_secs(5);
/// How long `pair_secret` waits for the relay to have the hub before giving up on it.
const REGISTERED_WITHIN: Duration = Duration::from_secs(10);
/// The refusal for a device past its bucket.
const RATE_REFUSED: &str = "Too many commands; wait a minute";
/// The refusal sent to a phone whose device was unpaired, just before it is closed.
const REVOKED_REFUSED: &str = "This phone was unpaired on the Mac";
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
    /// A frame arrived.
    Message { conn: u64, text: String },
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
    /// The device of the held pairing `pending`, and the key of the device it replaced.
    Paired {
        pending: u64,
        result: std::io::Result<Result<Option<String>, &'static str>>,
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

// A frame can hold a pairing secret or a push token, so the Debug output of what carries frames
// shows only their size.
impl std::fmt::Debug for ConnEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message { conn, text } => f
                .debug_struct("Message")
                .field("conn", conn)
                .field("text", &Redacted(text.len()))
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
    /// When the relay said `connect` for it: its first frame is due 10 seconds on.
    opened: Instant,
    /// The phone's key as its handshake proved it, 64 lowercase hex digits.
    key: String,
    /// The device, once `hello` succeeded.
    device: Option<Device>,
    /// Whether the phone waits for the answer to its `pair`: from the `pair` the hub held until
    /// `paired` or the refusal goes to it. The relay link stays up meanwhile, and its 10 seconds
    /// do not run.
    awaiting: bool,
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
    /// `pair_secret`: a new pairing secret, as a pairing link, unless a phone is held.
    PairSecret(oneshot::Sender<Opened>),
    /// `pair_wait`: the question for the Mac once a phone is held, or `expired`.
    PairWait(oneshot::Sender<Value>),
    /// `pair_answer`: yes or no for the held pairing `pending`.
    PairAnswer {
        pending: u64,
        accept: bool,
        reply: oneshot::Sender<Value>,
    },
    /// The number of paired devices.
    Status(oneshot::Sender<usize>),
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
    /// The hub's identity and relay keys.
    keys: Arc<HubKeys>,
    /// The relay it dials, which a pairing records.
    relay: String,
    /// The Mac's own host name, which `welcome` and `paired` carry.
    name: String,
    /// Whether the relay link should be up: a device is paired or a pairing is open.
    wanted: watch::Sender<bool>,
    /// Phone connections by id.
    conns: HashMap<u64, Conn>,
    /// Commands sent to sessions and not yet answered, by hub req.
    pending: HashMap<u64, Pending>,
    /// Each device's command allowance, by key.
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
    /// The snapshots the pushes and the Live Activity last looked at.
    observed: Vec<Snapshot>,
    /// Until when the relay link stays up for revoked or refused phones still to be told so,
    /// though nothing else wants it.
    lingering: Option<Instant>,
    /// The pairing held for the owner's answer, and the Mac waiting to hear of one.
    pairings: Pairings,
    /// The last id given to a connection, hub req or replay.
    ids: u64,
}

impl Hub {
    /// Serves the hub's control socket and, while a device is paired or a pairing is open, its
    /// link to the relay `hub.json` names; runs until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// Fails if the hub's files or its key file cannot be opened, another hub answers on the
    /// control socket, or the control socket cannot be bound.
    pub async fn serve(
        directory: PathBuf,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), String> {
        Self::serve_notify(directory, shutdown, oneshot::channel().0).await
    }

    /// Like [`serve`](Self::serve), and says on `ready` once the hub is answering (it is dropped
    /// unsent when the hub fails to start).
    ///
    /// # Errors
    ///
    /// The same as [`serve`](Self::serve).
    pub async fn serve_notify(
        directory: PathBuf,
        shutdown: impl Future<Output = ()>,
        ready: oneshot::Sender<()>,
    ) -> Result<(), String> {
        Self::run(directory, None, relay::stderr(), shutdown, ready).await
    }

    /// Like [`serve_notify`](Self::serve_notify), dialling the relay at `relay` (a `ws://`
    /// address on loopback, say) whatever `hub.json` says, and writing the relay link's log lines
    /// to `log`: for the hub's own tests, which run a relay in process. Debug builds only.
    ///
    /// # Errors
    ///
    /// The same as [`serve`](Self::serve).
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub async fn serve_with(
        directory: PathBuf,
        relay: String,
        log: Arc<dyn Fn(&str) + Send + Sync>,
        shutdown: impl Future<Output = ()>,
        ready: oneshot::Sender<()>,
    ) -> Result<(), String> {
        Self::run(directory, Some(relay), log, shutdown, ready).await
    }

    /// Serves, dialling `relay` or else the one `hub.json` names.
    async fn run(
        directory: PathBuf,
        relay: Option<String>,
        log: Log,
        shutdown: impl Future<Output = ()>,
        ready: oneshot::Sender<()>,
    ) -> Result<(), String> {
        let (
            Bound {
                store,
                run,
                control_path,
                control,
            },
            keys,
        ) = open(&directory).await?;
        let relay = relay.unwrap_or_else(|| store.config().relay.clone());
        let (events, mut inbox) = mpsc::channel(EVENT_QUEUE);
        let (dead_tokens, mut dead) = mpsc::channel(PUSH_QUEUE);
        let (activity_reports, mut reports) = mpsc::channel(ACTIVITY_QUEUE);
        let (saves, mut saved) = mpsc::channel(SAVE_QUEUE);
        let activity = load_activity(&store, activity_reports).await;
        let Link {
            wanted,
            state,
            mut arrivals,
            task: link_task,
        } = Link::start(relay.clone(), &keys, log);
        let (requests, mut controls) = mpsc::channel(16);
        let control_task = tokio::spawn(serve_control(control, requests, state)).abort_handle();
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
            keys,
            relay,
            name: host_name(),
            wanted,
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
            observed: Vec::new(),
            lingering: None,
            pairings: Pairings::default(),
            ids: 0,
        };
        hub.want_relay();
        hub.sessions.refresh().await;
        hub.listed = hub.sessions.frame().into();
        let (mut refresh, mut reload, mut sweep) = (
            every(REFRESH_EVERY),
            every(RELOAD_EVERY),
            every(SWEEP_EVERY),
        );
        let mut shutdown = std::pin::pin!(shutdown);
        let _ = ready.send(());
        loop {
            // `refresh` is not cancel-safe, so it runs in a handler, never as a branch.
            tokio::select! {
                () = &mut shutdown => break,
                Some(arrival) = arrivals.recv() => hub.accept(arrival),
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
        link_task.abort();
        for (_, conn) in hub.conns.drain() {
            conn.task.abort();
        }
        let _ = tokio::fs::remove_file(&control_path).await;
        Ok(())
    }

    /// Tells the relay link whether it should be up: while a device is paired, a pairing secret
    /// is out and unexpired, or a phone waits for the answer to its `pair` (its secret already
    /// spent, its device not yet saved), and for a moment after a revoke or a refused pairing
    /// ([`linger`](Self::linger)), so that phone hears it before its socket goes with the link.
    fn want_relay(&mut self) {
        let now = Instant::now();
        self.lingering = self.lingering.filter(|until| *until > now);
        let wanted = !self.store.devices().is_empty()
            || self.store.pairing_open(SystemTime::now())
            || self.conns.values().any(|state| state.awaiting)
            || self.lingering.is_some();
        self.wanted.send_if_modified(|was| {
            let changed = *was != wanted;
            *was = wanted;
            changed
        });
    }

    /// Keeps the relay link up for a moment, whatever else wants it: a phone just told it is
    /// revoked or refused hears it before its socket goes with the link.
    fn linger(&mut self) {
        self.lingering = Some(Instant::now() + REVOKED_GRACE + CLOSE_WITHIN);
    }

    /// Marks whether `conn`'s phone waits for the answer to its `pair` (see [`Conn::awaiting`]),
    /// and tells the relay link. A connection that goes stops waiting with it.
    fn await_answer(&mut self, conn: u64, awaiting: bool) {
        if let Some(state) = self.conns.get_mut(&conn) {
            state.awaiting = awaiting;
        }
        self.want_relay();
    }

    /// A number never handed out before.
    fn next_id(&mut self) -> u64 {
        self.ids += 1;
        self.ids
    }

    /// Starts the task that serves one phone past its handshake, which brings its seat.
    fn accept(
        &mut self,
        Arrival {
            channel,
            peer,
            seat,
        }: Arrival,
    ) {
        let conn = self.next_id();
        let (out, outbox) = mpsc::channel(OUTBOUND_QUEUE);
        let (replay, replays) = mpsc::channel(REPLAY_QUEUE);
        let listing = Listing::default();
        let writer = Writer {
            outbox,
            listing: Arc::clone(&listing),
            replays,
        };
        let task =
            tokio::spawn(connection(channel, conn, writer, self.events.clone())).abort_handle();
        self.conns.insert(
            conn,
            Conn {
                out,
                listing,
                offered: None,
                replay,
                task,
                opened: seat.since(),
                key: hex(&peer.key),
                device: None,
                awaiting: false,
                subscriptions: HashMap::new(),
                background: false,
                listings: Listings::default(),
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
    async fn on_message(&mut self, conn: u64, text: String) {
        let Some(state) = self.conns.get(&conn) else {
            return;
        };
        match state.device.clone() {
            None => self.unauthenticated(conn, &text).await,
            Some(device) => self.authenticated(conn, &device, &text).await,
        }
    }

    /// A frame before `hello`: only `hello` from a known key and `pair` are taken; anything
    /// else, or a failure, is refused and closes the connection.
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
        let Some(key) = self.conns.get(&conn).map(|state| state.key.clone()) else {
            return;
        };
        match command {
            PhoneCommand::Hello { background } => {
                // A revoke by the CLI must be seen before the key is believed.
                self.store.reload_if_changed().await;
                let Some((device, seen)) = self.store.recognise(&key, now) else {
                    self.refuse(conn, req, Code::Unpaired, "Unknown device; pair it again");
                    self.close(conn);
                    return;
                };
                if let Some(seen) = seen {
                    // Off the loop; a device revoked meanwhile is let go at the next reload.
                    self.save(seen_change(&device.key, seen), |_| AfterSave::Seen);
                }
                let name = device.name.clone();
                // Judged on news from now. Any hello but a background one is the app in front,
                // which drives the Live Activity itself while it is; nothing ends.
                if let Some(activity) = self.activity.as_mut() {
                    activity.hello(&device.key);
                    if !background {
                        let tokens = device.activity.as_ref();
                        activity.connected(&device.key, tokens, now);
                    }
                }
                // Out of the waiting room before the phone hears it is in.
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.seat.admitted();
                    state.device = Some(device);
                    state.background = background;
                }
                self.send(conn, &welcome(&name, &self.name));
                // A fresh hello is answered at once, whenever the last list went.
                let frame = self.sessions.frame().into();
                self.offer_sessions(conn, &frame);
            }
            PhoneCommand::Pair {
                secret,
                name,
                platform,
            } => {
                // The device is the key the handshake proved, paired through this relay once the
                // owner says yes.
                self.pair((conn, req, &key), secret.expose(), &name, &platform)
                    .await;
            }
            _ => {
                self.refuse(conn, req, Code::Unpaired, "Say hello first");
                self.close(conn);
            }
        }
    }

    /// A frame from a device that said hello.
    async fn authenticated(&mut self, conn: u64, device: &Device, text: &str) {
        self.store.reload_if_changed().await;
        if !self.paired(device) {
            self.revoke(conn);
            return;
        }
        if !self.take_token(&device.key) {
            self.refuse(conn, req_of(text), Code::RateLimited, RATE_REFUSED);
            return;
        }
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
        let change = push_change(&device.key, token.clone(), kinds);
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
            if let Some(held) = state.device.as_mut().filter(|held| held.key == device.key) {
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
        let hash = &device.key;
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
            activity.cleared(&device.key);
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
            if let Some(stored) = devices.iter().find(|d| d.key == held.key) {
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
            AfterSave::Paired { pending, result } => self.paired_saved(pending, result).await,
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
                let _ = reply.send(self.open_pairing());
            }
            Control::PairWait(reply) => self.wait_for_phone(reply),
            Control::PairAnswer {
                pending,
                accept,
                reply,
            } => self.answer(pending, accept, reply),
            Control::Status(reply) => {
                self.store.reload_if_changed().await;
                let _ = reply.send(self.store.devices().len());
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
        self.want_relay();
    }

    /// Refuses, for their phones, the commands no session answered in time, and closes
    /// connections that never said hello 10 seconds after their `connect`, but for a phone held
    /// for the owner's answer.
    fn sweep(&mut self) {
        let now = Instant::now();
        self.launches.running(self.sessions.phone_started(), now);
        let silent: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| {
                state.device.is_none()
                    && !state.awaiting
                    && now >= state.opened + FIRST_FRAME_WITHIN
            })
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

    /// Every second: refuses what timed out (a held pairing too), sends the pushes and Live
    /// Activity sends due, and lets the relay link go once the last pairing secret expired with
    /// nothing paired.
    fn tick(&mut self) {
        self.sweep();
        self.pairing_tick(Instant::now());
        self.flush_push();
        self.flush_activity();
        self.want_relay();
    }

    /// Whether `device` is still paired.
    fn paired(&self, device: &Device) -> bool {
        self.store.devices().iter().any(|d| d.key == device.key)
    }

    /// Takes one command from the bucket of the device whose key is `key`.
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
        self.linger();
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
        // What these look at is the snapshots alone, and the same snapshots again change
        // nothing: a storm of state frames that change only rows (their NOW lines) costs one
        // look at each session per frame, not a snapshot of each.
        if self.sessions.same_as(&self.observed) {
            return;
        }
        let snapshots = self.sessions.snapshots();
        let notices = self.pusher.observe(&snapshots);
        let now = Instant::now();
        if let Some(activity) = self.activity.as_mut() {
            activity.observe(&snapshots, (now, SystemTime::now()));
        }
        for device in self.store.devices() {
            if device.push_token.is_some() {
                self.pusher.offer(&device.key, &notices, now);
            }
        }
        self.observed = snapshots;
    }

    /// Posts the pushes whose windows have closed, in the background. A device with a phone
    /// connected right now is looked at, so it gets none.
    fn flush_push(&mut self) {
        let now = Instant::now();
        self.pusher.prune();
        self.cap.prune(now);
        let mut batch = Vec::new();
        for key in self.pusher.waiting() {
            let known = self.store.devices().iter().find(|device| device.key == key);
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
        self.sender.dispatch(batch, self.keys.channel().to_owned());
    }

    /// Sends what each device's Live Activity is due, in the background, while no connection of
    /// the device streams a session: an update to its activity, a start when it has none, an end
    /// once its sessions have all been idle long enough. Each send takes from the allowance
    /// pushes use.
    fn flush_activity(&mut self) {
        let Some(activity) = self.activity.as_mut() else {
            return;
        };
        let conns = &self.conns;
        activity.flush(
            self.store.devices(),
            |key| in_front(conns, key),
            &mut self.cap,
            self.keys.channel(),
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
            .find(|device| device.key == report.device)
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

    /// Removes `conn`, its unanswered commands and a pairing held on it; its replays stop as it
    /// drops. When it was its device's last connection, the approvals that device was spared
    /// while looking are offered to it again.
    fn forget(&mut self, conn: u64) -> Option<Conn> {
        let state = self.conns.remove(&conn)?;
        self.pending.retain(|_, pending| pending.conn != conn);
        self.pairing_conn_gone(conn);
        if let Some(device) = &state.device {
            let key = &device.key;
            // A background connection going was never the app in front.
            let still_here = state.background || in_front(&self.conns, key);
            // The app leaves the front: what its activity shows is current as of now.
            if !still_here && let Some(activity) = self.activity.as_mut() {
                let tokens = self
                    .store
                    .devices()
                    .iter()
                    .find(|known| &known.key == key)
                    .and_then(|known| known.activity.as_ref());
                activity.left(key, tokens, (Instant::now(), SystemTime::now()));
            }
            // A device unpaired, or without a push token, has nowhere to be told.
            let reachable = self
                .store
                .devices()
                .iter()
                .any(|known| &known.key == key && known.push_token.is_some());
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

/// What the hub serves from: its files, the run directory, and the control socket, bound.
struct Bound {
    store: Store,
    run: PathBuf,
    control_path: PathBuf,
    control: UnixListener,
}

/// Opens the hub's files in `directory`, readies its run directory and binds the control
/// socket there.
async fn bind(directory: &Path) -> Result<Bound, String> {
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
    Ok(Bound {
        store,
        run,
        control_path,
        control,
    })
}

/// The relay link's task and its ends the hub holds.
struct Link {
    /// Whether the link should be up.
    wanted: watch::Sender<bool>,
    /// Where it stands.
    state: watch::Receiver<RelayState>,
    /// The phones it hands over past their handshake.
    arrivals: mpsc::Receiver<Arrival>,
    /// Its task.
    task: AbortHandle,
}

impl Link {
    /// Starts the link to the relay at `relay` for the hub with `keys`, down until it is wanted.
    fn start(relay: String, keys: &Arc<HubKeys>, log: Log) -> Self {
        let (arrived, arrivals) = mpsc::channel(ARRIVAL_QUEUE);
        let (wanted, wanted_by_link) = watch::channel(false);
        let (relay_state, state) = watch::channel(RelayState::Off);
        let link = relay::run(
            relay,
            Arc::clone(keys),
            wanted_by_link,
            Arc::new(Room::default()),
            arrived,
            relay_state,
            log,
        );
        Self {
            wanted,
            state,
            arrivals,
            task: tokio::spawn(link).abort_handle(),
        }
    }
}

/// [`bind`], then the hub's keys ([`load_keys`]); a hub that cannot use its keys gives its
/// control socket back.
async fn open(directory: &Path) -> Result<(Bound, Arc<HubKeys>), String> {
    let mut bound = bind(directory).await?;
    match load_keys(directory, &mut bound.store).await {
        Ok(keys) => Ok((bound, Arc::new(keys))),
        Err(error) => {
            let path = bound.control_path.clone();
            drop(bound);
            let _ = tokio::fs::remove_file(&path).await;
            Err(error)
        }
    }
}

/// The hub's keys in `directory`. A hub about to make its key forgets every device first, with
/// its push and Live Activity tokens: those phones pinned another key and can never connect
/// again. The forgetting comes before the key file exists, so no failure after the key is made
/// (a directory sync that fails, a crash) can leave a key whose devices were not forgotten.
async fn load_keys(directory: &Path, store: &mut Store) -> Result<HubKeys, String> {
    let file = directory.join("hub.key");
    let absent = matches!(
        tokio::fs::symlink_metadata(&file).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    if absent {
        forget_devices(directory, store).await?;
    }
    let (keys, made) = HubKeys::load_or_create(directory)
        .await
        .map_err(|e| format!("Couldn't use the hub's key: {e}"))?;
    // The file was removed after the look above: this load made a key the forgetting missed.
    if made && !absent {
        forget_devices(directory, store).await?;
    }
    Ok(keys)
}

/// Forgets every device in `store`, the store of `directory`, for a new key.
async fn forget_devices(directory: &Path, store: &mut Store) -> Result<(), String> {
    store.forget_all().await.map_err(|e| {
        format!(
            "Couldn't forget the devices of an old key in {}: {e}",
            directory.display()
        )
    })
}

/// The Mac's own host name, at most [`NAME_LIMIT`] characters, without a trailing `.local`;
/// `Mac` when it has none.
fn host_name() -> String {
    let uname = rustix::system::uname();
    let full = uname.nodename().to_string_lossy();
    let name: String = full
        .strip_suffix(".local")
        .unwrap_or(&full)
        .chars()
        .filter(|c| !c.is_control())
        .take(NAME_LIMIT)
        .collect();
    if name.is_empty() { "Mac".into() } else { name }
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

/// Whether the device whose key is `key` has the app in front: a connection of it whose
/// hello was not a background one, on any screen. The pusher and the Live Activity share it.
fn in_front(conns: &HashMap<u64, Conn>, key: &str) -> bool {
    conns.values().any(|conn| {
        !conn.background && conn.device.as_ref().is_some_and(|device| device.key == key)
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

/// Accepts control clients, each answered in its own task; `relay` says where the hub stands
/// with the relay.
async fn serve_control(
    listener: UnixListener,
    hub: mpsc::Sender<Control>,
    relay: watch::Receiver<RelayState>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(control_client(stream, hub.clone(), relay.clone()));
            }
            // Out of descriptors, say: wait rather than spin.
            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
        }
    }
}

/// Reads one request line from a control client and writes one answer line.
async fn control_client(
    stream: UnixStream,
    hub: mpsc::Sender<Control>,
    relay: watch::Receiver<RelayState>,
) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(CONTROL_LIMIT));
    if !timeout(CONTROL_WITHIN, reader.read_line(&mut line))
        .await
        .is_ok_and(|read| read.is_ok_and(|n| n > 0))
    {
        return;
    }
    match request_kind(&line).as_str() {
        "watch" => return watch_stream(&mut reader, &mut write, &hub).await,
        "pair_wait" => return pair_wait(&mut reader, &mut write, &hub).await,
        _ => {}
    }
    let answer = control_answer(&line, &hub, &relay).await;
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

/// Answers a `pair_wait` once the hub has an answer for it; a client that leaves first (its side
/// closed) ends the wait.
async fn pair_wait(
    reader: &mut (impl AsyncReadExt + Unpin),
    write: &mut OwnedWriteHalf,
    hub: &mpsc::Sender<Control>,
) {
    let (reply, mut answer) = oneshot::channel();
    let answer = if hub.send(Control::PairWait(reply)).await.is_err() {
        stopping()
    } else {
        let mut ignored = [0u8; 64];
        loop {
            tokio::select! {
                answer = &mut answer => break answer.unwrap_or_else(|_| stopping()),
                // The client sends nothing more; a read that ends means it left.
                read = reader.read(&mut ignored) => {
                    if read.is_ok_and(|n| n > 0) {
                        continue;
                    }
                    return;
                }
            }
        }
    };
    let _ = timeout(
        CONTROL_WITHIN,
        write.write_all(format!("{answer}\n").as_bytes()),
    )
    .await;
}

/// The answer to a control request the hub cannot take: it is stopping.
fn stopping() -> Value {
    json!({"error": "The hub is stopping"})
}

/// The `pending` id and the yes or no of a `pair_answer` request line.
fn pair_answer_request(line: &str) -> Option<(u64, bool)> {
    let request: Value = serde_json::from_str(line).ok()?;
    Some((
        request.get("pending")?.as_u64()?,
        request.get("accept")?.as_bool()?,
    ))
}

/// The answer to one control request: `pair_secret` opens a pairing (which brings the relay link
/// up) and gives its link once the relay has the hub; `pair_answer` says yes or no for the held
/// phone and hears how that went; `status` gives where the hub stands with the relay and how many
/// phones are paired.
async fn control_answer(
    line: &str,
    hub: &mpsc::Sender<Control>,
    relay: &watch::Receiver<RelayState>,
) -> Value {
    let kind = request_kind(line);
    match kind.as_str() {
        "pair_secret" => {
            let (reply, opened) = oneshot::channel();
            if hub.send(Control::PairSecret(reply)).await.is_err() {
                return stopping();
            }
            match opened.await {
                Ok(Opened::Link { link, host }) => {
                    let mut relay = relay.clone();
                    let registered = relay.wait_for(|state| *state == RelayState::Registered);
                    if timeout(REGISTERED_WITHIN, registered)
                        .await
                        .is_ok_and(|registered| registered.is_ok())
                    {
                        json!({"link": link})
                    } else {
                        json!({"link": null, "reason": format!("Can't reach the relay at {host}")})
                    }
                }
                Ok(Opened::Waiting) => json!({"link": null, "reason": PHONE_WAITS}),
                Ok(Opened::Unlinkable) => {
                    json!({"error": "The relay's address can't go in a pairing link"})
                }
                Err(_) => stopping(),
            }
        }
        "pair_answer" => {
            let Some((pending, accept)) = pair_answer_request(line) else {
                return json!({"error": "pair_answer needs pending and accept"});
            };
            let (reply, answer) = oneshot::channel();
            let request = Control::PairAnswer {
                pending,
                accept,
                reply,
            };
            if hub.send(request).await.is_err() {
                return stopping();
            }
            answer.await.unwrap_or_else(|_| stopping())
        }
        "status" => {
            let (reply, devices) = oneshot::channel();
            if hub.send(Control::Status(reply)).await.is_err() {
                return stopping();
            }
            let Ok(devices) = devices.await else {
                return stopping();
            };
            json!({"relay": relay.borrow().as_str(), "devices": devices})
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

/// Serves one phone's channel: frames both ways until either side ends.
async fn connection(
    channel: Secure<Stream>,
    conn: u64,
    mut writer: Writer,
    events: mpsc::Sender<ConnEvent>,
) {
    pump(channel, conn, &mut writer, &events).await;
    let _ = events.send(ConnEvent::Closed { conn }).await;
}

/// Moves frames between the phone and the hub loop: what the phone sends goes to the hub,
/// and replayed and live frames go to the phone. A frame over `MAX_MESSAGE` or anything else the
/// channel refuses, a write that stalls, a ping still unanswered when the next falls due, or the
/// hub letting go ends it.
async fn pump(
    mut channel: Secure<Stream>,
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
    let mut pings = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // `ping` and `send` are not cancel safe: they run in handlers, never as branches.
        tokio::select! {
            _ = pings.tick() => {
                // False when the last ping is still unanswered: the phone is gone, whatever the
                // relay says of its socket.
                let sent = timeout(WRITE_STALL, channel.ping()).await;
                if !sent.unwrap_or(false) {
                    return;
                }
            }
            incoming = channel.next() => match incoming {
                Incoming::Text(text) => {
                    if events.send(ConnEvent::Message { conn, text }).await.is_err() {
                        return;
                    }
                }
                Incoming::Pong | Incoming::Skip => {}
                Incoming::Closed => return,
            },
            item = replays.recv(), if replaying => match item {
                Some(ReplayItem::Frame { frame, current }) => {
                    // A subscription since dropped or replaced has its acknowledgement out
                    // already: what it left queued would follow it.
                    if current.load(Ordering::Acquire) && !write(&mut channel, &frame).await {
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
                    if !write(&mut channel, &frame).await {
                        return;
                    }
                }
                Some(Out::Sessions) => {
                    if let Some(frame) = take_listing(listing)
                        && !write(&mut channel, frame.as_str()).await
                    {
                        return;
                    }
                }
                Some(Out::Close) | None => {
                    let _ = timeout(CLOSE_WITHIN, channel.close()).await;
                    return;
                }
            },
        }
    }
}

/// Writes one frame; false if it failed or stalled.
async fn write(channel: &mut Secure<Stream>, frame: &str) -> bool {
    timeout(WRITE_STALL, channel.send(frame))
        .await
        .is_ok_and(|sent| sent)
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
        let device = store
            .pair(
                &secret,
                "Order phone",
                "ios",
                &"ab".repeat(32),
                "",
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
                    let change = activity_change(&device.key, tokens(n));
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
    fn debug_output_never_shows_what_a_phone_sent_or_was_sent() {
        let secret = "f00dfeed".repeat(8);
        let message = ConnEvent::Message {
            conn: 3,
            text: format!("{{\"type\":\"pair\",\"secret\":\"{secret}\"}}"),
        };
        let paired = Out::Text(format!(
            "{{\"type\":\"register_push\",\"token\":\"{secret}\"}}"
        ));
        for shown in [format!("{message:?}"), format!("{paired:?}")] {
            assert!(!shown.contains(&secret), "{shown}");
            assert!(shown.contains("bytes"), "{shown}");
        }
    }
}
