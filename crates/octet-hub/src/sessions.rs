//! The sessions the hub can reach: each running terminal UI's registration, its bridge socket
//! and its journals.
//!
//! # Replay and live events
//!
//! A session's [`Tail`] of its newest journal is the source of truth for live events. It starts
//! at the last whole line of the newest journal when the hub first sees the session (older
//! history is for [`replay`](crate::tail::replay)), and moves to each later journal from its
//! first byte, after the old journal's remaining whole lines are emitted. The old session can
//! still write its last records (usage, an error, `stopped`) after the new journal begins, so
//! the old journal's tail is kept and read alongside until it has a `stopped` record or ten
//! seconds have passed; its late records go out under its own name. Within a journal, records
//! are emitted in order, so a client's position is the last `seq` it has of each journal. A client that
//! subscribes registers first, then replays page by page (each page's last record is the next
//! cursor), meanwhile buffering live [`SessionUpdate::Event`]s in a capped buffer (a client that
//! overflows it is dropped, and resumes from its cursor), and then drops every buffered event at
//! or below what it has of that event's journal. Everything the tail emits after the
//! subscription is either in the replay or delivered live, so nothing is lost and nothing
//! repeats.
//!
//! # Facts
//!
//! The records a session's tails read also keep its [`Facts`]: the latest usage line, how many
//! error records came since the hub first saw it (for the error push), and whether the last
//! finished turn's reply ended on a question. When the hub first sees a session they are seeded
//! from at most the last [`SEED_BYTES`] of its newest journal (errors aside: those were before the
//! hub looked). A change to them is a [`SessionUpdate::Changed`] after the records' events.
use crate::{
    push::Snapshot,
    tail::{Tail, event_of, file_name},
};
use octet_core::remote::{
    protocol::{BridgeCommand, Code, MAX_BRIDGE_LINE, VERSION, bridge_command_json},
    run::Registration,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::SeekFrom,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    task::AbortHandle,
    time::{Instant, Interval, MissedTickBehavior, interval, timeout},
};

/// How often every session's journal is read for new records.
const TAIL_EVERY: Duration = Duration::from_millis(100);
/// How long a bridge gets to accept a connection.
const CONNECT_WITHIN: Duration = Duration::from_millis(500);
/// How long one write to a bridge may take before the bridge is let go.
const WRITE_STALL: Duration = Duration::from_secs(30);
/// Lines waiting to be written to one bridge before it counts as too slow.
const LINK_QUEUE: usize = 64;
/// Frames read from bridges and not yet taken by [`Sessions::next`].
const INBOUND_QUEUE: usize = 256;
/// The longest line read from a bridge: a whole line and its newline.
const LINE_LIMIT: u64 = MAX_BRIDGE_LINE as u64 + 1;
/// The largest registration file read.
const REGISTRATION_LIMIT: u64 = 1024 * 1024;
/// The longest `goal` and `now` in a `sessions` row, in characters.
const GOAL_CHARS: usize = 200;
/// The most a `sessions` frame's rows take with their approval gists, in bytes. Past it, the
/// least recently active rows lose their gists (keeping their counts), so the frame stays well
/// under a message however many sessions wait.
const GISTS_BUDGET: usize = 64 * 1024;
/// How long a journal the session has moved on from is still read for its last records.
const FOLLOW_PREVIOUS: Duration = Duration::from_secs(10);
/// The most of a reply kept to tell whether it ended on a question, in characters.
const REPLY_CHARS: usize = 256;
/// The most of the newest journal read to seed a session's facts when the hub first sees it.
const SEED_BYTES: u64 = 256 * 1024;

/// What changed in the sessions since the last call to [`Sessions::next`].
#[derive(Debug, PartialEq)]
pub(crate) enum SessionUpdate {
    /// A new journal record: `record` is the event `{type, data}` and `seq` the record's number
    /// within `journal`, a file name.
    Event {
        id: String,
        journal: String,
        seq: u64,
        record: Value,
    },
    /// The session's state arrived: see [`Sessions::state`].
    State { id: String },
    /// The session's catalog arrived: see [`Sessions::catalog`].
    Catalog { id: String },
    /// The bridge answered command `req`: `ok` is false for a refusal, whose reason is `message`
    /// and code `code` (when the bridge gave a known one).
    Reply {
        id: String,
        req: u64,
        ok: bool,
        message: String,
        code: Option<Code>,
    },
    /// Sessions came, went or changed what a `sessions` row shows.
    Changed,
}

/// A frame from a bridge, or the end of its link.
#[derive(Debug)]
enum Frame {
    State(Value),
    Catalog(Value),
    Reply {
        req: u64,
        ok: bool,
        message: String,
        code: Option<Code>,
    },
    Closed,
}

/// A frame and where it came from: the session and which of its links.
#[derive(Debug)]
struct Inbound {
    session: String,
    link: u64,
    frame: Frame,
}

/// The connection to one bridge: the queue its writer drains, and the tasks.
#[derive(Debug)]
struct Link {
    number: u64,
    lines: mpsc::Sender<String>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// A journal the session moved on from, read for its last records until `until`.
#[derive(Debug)]
struct Previous {
    tail: Tail,
    until: Instant,
}

/// Most characters of a gist's text, and files in it.
const GIST_CHARS: usize = 200;
const GIST_FILES: usize = 3;

/// `text` cut to `GIST_CHARS` characters (on a character boundary), with "…" when it was cut.
fn clip(text: &str) -> String {
    match text.char_indices().nth(GIST_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// A command's first line, clipped; "…" also marks the lines left out.
fn clip_command(command: &str) -> String {
    let command = command.trim_start();
    let (first, rest) = command.split_once('\n').unwrap_or((command, ""));
    let mut line = clip(first.trim_end());
    if !rest.trim().is_empty() && !line.ends_with('…') {
        line.push('…');
    }
    line
}

/// An approval of the bridge's state as the `sessions` row carries it: just its gist, so a row
/// stays small whatever the summary holds. The session's own state frame has the whole summary.
/// None when the entry has no numeric id or no object summary.
fn gist(approval: &Value) -> Option<Value> {
    let id = approval.get("id")?.as_u64()?;
    let summary = approval.get("summary")?.as_object()?;
    let text = |key: &str| summary.get(key)?.as_str();
    let mut out = serde_json::Map::new();
    for key in ["action", "scope", "cwd"] {
        if let Some(value) = text(key) {
            out.insert(key.into(), json!(clip(value)));
        }
    }
    if let Some(command) = text("command") {
        out.insert("command".into(), json!(clip_command(command)));
    }
    if let Some(files) = summary.get("files").and_then(Value::as_array) {
        let mut shown: Vec<String> = files
            .iter()
            .filter_map(Value::as_str)
            .take(GIST_FILES)
            .map(clip)
            .collect();
        if files.len() > GIST_FILES {
            shown.push(format!("+{} more", files.len() - GIST_FILES));
        }
        out.insert("files".into(), json!(shown));
    }
    Some(json!({"id": id, "summary": out}))
}

/// What a session's journal records have said that its row and the pusher show.
#[derive(Debug, Default)]
struct Facts {
    /// The latest `usage` record's text, clipped to [`GOAL_CHARS`].
    usage: Option<String>,
    /// `error` records read since the hub first saw the session.
    errors: u64,
    /// The last [`REPLY_CHARS`] characters of the reply text since the last `user` record.
    reply: String,
    /// The last finished turn's reply, trimmed, ended on `?`; cleared by the next `user` record.
    /// A heuristic: the reply may ask in other words, or quote a question.
    question: bool,
}

impl Facts {
    /// Takes one record of `kind` with `data`; whether the row or the pusher sees a change. A
    /// reply's chunk costs at most [`REPLY_CHARS`] characters of work, however long it or the
    /// reply is.
    fn take(&mut self, kind: &str, data: &Value) -> bool {
        match kind {
            "usage" => {
                let Some(text) = data.as_str() else {
                    return false;
                };
                let usage: String = text.chars().take(GOAL_CHARS).collect();
                if self.usage.as_ref() == Some(&usage) {
                    return false;
                }
                self.usage = Some(usage);
                true
            }
            "error" => {
                self.errors = self.errors.saturating_add(1);
                true
            }
            "text" => {
                if let Some(chunk) = data.as_str() {
                    self.hear(chunk);
                }
                false
            }
            "user" => {
                self.reply.clear();
                std::mem::replace(&mut self.question, false)
            }
            "finished" => {
                let asked = self.reply.trim_end().ends_with('?');
                asked != std::mem::replace(&mut self.question, asked)
            }
            _ => false,
        }
    }

    /// Adds a chunk of the reply, keeping its last [`REPLY_CHARS`] characters.
    fn hear(&mut self, chunk: &str) {
        self.reply.push_str(last_chars(chunk, REPLY_CHARS));
        // At most twice the limit here, so counting is bounded too.
        let extra = self.reply.chars().count().saturating_sub(REPLY_CHARS);
        if extra > 0 {
            let cut = self
                .reply
                .char_indices()
                .nth(extra)
                .map_or(self.reply.len(), |(at, _)| at);
            self.reply.drain(..cut);
        }
    }
}

/// The last `count` characters of `text` (all of it when shorter), found from its end.
fn last_chars(text: &str, count: usize) -> &str {
    match count.checked_sub(1) {
        None => "",
        Some(back) => text
            .char_indices()
            .rev()
            .nth(back)
            .map_or(text, |(at, _)| &text[at..]),
    }
}

/// The facts the last [`SEED_BYTES`] before `end` of `journal` hold, and the bytes read. One
/// bounded read; when it starts past the journal's beginning, what comes before its first newline
/// is the end of an earlier line and is passed over. Errors are not counted: they were before the
/// hub looked. A journal that cannot be read seeds nothing.
async fn seed(journal: &Path, end: u64) -> (Facts, u64) {
    let start = end.saturating_sub(SEED_BYTES);
    let read = async {
        let mut file = tokio::fs::File::open(journal).await?;
        if !file.metadata().await?.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        file.seek(SeekFrom::Start(start)).await?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes).await?;
        Ok(bytes)
    };
    let mut facts = Facts::default();
    let Ok(bytes) = read.await else {
        return (facts, 0);
    };
    let mut lines = bytes.split(|&byte| byte == b'\n');
    if start > 0 {
        lines.next();
    }
    for line in lines {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some(kind) = record.get("type").and_then(Value::as_str) {
            facts.take(kind, record.get("data").unwrap_or(&Value::Null));
        }
    }
    facts.errors = 0;
    (facts, bytes.len() as u64)
}

/// One session: its registration, bridge link, journal tails and what the bridge last said.
#[derive(Debug)]
struct Live {
    registration: Registration,
    link: Option<Link>,
    tail: Option<Tail>,
    /// Journals it moved on from that may still get records, oldest first.
    previous: Vec<Previous>,
    state: Option<Value>,
    catalog: Option<Value>,
    last_activity: Option<u64>,
    facts: Facts,
}

impl Live {
    /// Forgets the link and what it said, which is no longer current.
    fn lose_link(&mut self) {
        self.link = None;
        self.state = None;
        self.catalog = None;
    }

    /// The row of the `sessions` frame.
    fn row(&self) -> Value {
        let registration = &self.registration;
        let state = self.state.as_ref();
        let text = |key: &str| state?.get(key)?.as_str();
        let goal = state
            .and_then(|s| s.get("goal")?.get("objective")?.as_str())
            .and_then(|objective| objective.lines().next())
            .filter(|line| !line.is_empty())
            .map(|line| line.chars().take(GOAL_CHARS).collect::<String>());
        let approvals = state
            .and_then(|s| s.get("approvals")?.as_array())
            .map_or(0, Vec::len);
        let front = state
            .and_then(|s| s.get("approvals")?.as_array()?.first())
            .and_then(gist);
        let mut row = json!({
            "id": registration.id,
            "engine": registration.engine,
            "workspace": registration.workspace.to_string_lossy(),
            "mode": text("mode").unwrap_or(&registration.mode),
            "phase": text("phase").unwrap_or("connecting"),
            "approvals": approvals,
            "from_phone": registration.from_phone,
        });
        let optional = [
            (
                "model",
                (!registration.model.is_empty()).then(|| json!(registration.model)),
            ),
            ("last_activity", self.last_activity.map(|t| json!(t))),
            ("goal", goal.map(|g| json!(g))),
            (
                "now",
                text("now").map(|now| json!(now.chars().take(GOAL_CHARS).collect::<String>())),
            ),
            ("tmux", registration.tmux.as_ref().map(|t| json!(t))),
            ("approval", front),
            ("usage", self.facts.usage.as_ref().map(|u| json!(u))),
            ("question", self.facts.question.then_some(json!(true))),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                row[key] = value;
            }
        }
        row
    }
}

/// Every session the hub can reach.
#[derive(Debug)]
pub(crate) struct Sessions {
    run: PathBuf,
    live: Vec<Live>,
    pending: VecDeque<SessionUpdate>,
    updates: mpsc::Receiver<Inbound>,
    sender: mpsc::Sender<Inbound>,
    tick: Option<Interval>,
    links: u64,
}

impl Sessions {
    /// Sessions found in the run directory `run`; call [`refresh`](Self::refresh) to look.
    pub(crate) fn new(run: PathBuf) -> Self {
        let (sender, updates) = mpsc::channel(INBOUND_QUEUE);
        Self {
            run,
            live: Vec::new(),
            pending: VecDeque::new(),
            updates,
            sender,
            tick: None,
            links: 0,
        }
    }

    /// One pass over the run directory. A registration is used only if its file is named for
    /// its pid, its id and socket follow from that pid (the socket is `<pid>.sock` in this
    /// directory and really a socket), and the socket connects. A registration that fails goes
    /// away, with its `.json` (and its socket, when it was the connection that failed). Sessions
    /// whose registration is gone are dropped, linked ones whose link broke are linked again,
    /// and a session that gained a journal moves its tail there. Every journal it lists must be
    /// a regular file (no symlink, FIFO or device) named like a journal, directly in the data
    /// directory (the run directory's parent); a registration with another is refused and its
    /// `.json` removed, and one whose journal does not exist yet waits.
    ///
    /// Not cancel-safe: call it from the server loop that also drives [`next`](Self::next),
    /// never as a branch of a `select!` that can drop it half done.
    pub(crate) async fn refresh(&mut self) {
        let home = tokio::fs::canonicalize(&self.run).await.ok();
        let mut changed = false;
        let mut present = Vec::new();
        for (file, registration) in scan(&self.run).await {
            if !vetted(home.as_deref(), &file, &registration).await {
                let _ = tokio::fs::remove_file(&file).await;
                continue;
            }
            match check_journals(
                home.as_deref().and_then(Path::parent),
                &registration.journals,
            )
            .await
            {
                Journals::Fine => {}
                Journals::Refused => {
                    let _ = tokio::fs::remove_file(&file).await;
                    continue;
                }
                Journals::Missing => {
                    // Not trusted yet, but not wrong either: a session already known stays.
                    if self
                        .live
                        .iter()
                        .any(|l| l.registration.id == registration.id)
                    {
                        present.push(registration.id.clone());
                    }
                    continue;
                }
            }
            let known = self
                .live
                .iter()
                .position(|l| l.registration.id == registration.id);
            let link = if known.is_none_or(|at| self.live[at].link.is_none()) {
                let Some(link) = self.connect(&registration).await else {
                    let _ = tokio::fs::remove_file(&file).await;
                    let _ = tokio::fs::remove_file(&registration.socket).await;
                    continue;
                };
                Some(link)
            } else {
                None
            };
            present.push(registration.id.clone());
            match known {
                None => {
                    let tail = match registration.journals.last() {
                        Some(newest) => Some(Tail::at_end(newest.clone()).await),
                        None => None,
                    };
                    let last_activity = activity(&registration.journals).await;
                    let facts = match &tail {
                        Some(tail) => seed(tail.path(), tail.offset()).await.0,
                        None => Facts::default(),
                    };
                    self.live.push(Live {
                        registration,
                        link,
                        tail,
                        previous: Vec::new(),
                        state: None,
                        catalog: None,
                        last_activity,
                        facts,
                    });
                    changed = true;
                }
                Some(at) => {
                    let live = &mut self.live[at];
                    if link.is_some() {
                        live.link = link;
                    }
                    changed |= shows_differently(&live.registration, &registration);
                    if live.registration.journals != registration.journals {
                        changed |= follow(live, &registration.journals, &mut self.pending).await;
                    }
                    live.registration = registration;
                    live.last_activity = activity(&live.registration.journals).await;
                }
            }
        }
        let before = self.live.len();
        self.live.retain(|l| present.contains(&l.registration.id));
        if changed || self.live.len() != before {
            self.push_changed();
        }
    }

    /// The `sessions` frame's rows, oldest session first. Rows over [`GISTS_BUDGET`] lose their
    /// approval gists, least recently active first, until they fit.
    pub(crate) fn list(&self) -> Vec<Value> {
        let mut rows: Vec<Value> = self.live.iter().map(Live::row).collect();
        let mut total: usize = rows.iter().map(|row| row.to_string().len() + 1).sum();
        if total <= GISTS_BUDGET {
            return rows;
        }
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by_key(|&i| (self.live[i].last_activity, i));
        for i in order {
            if total <= GISTS_BUDGET {
                break;
            }
            if let Some(gist) = rows[i]
                .as_object_mut()
                .and_then(|row| row.remove("approval"))
            {
                total = total.saturating_sub(gist.to_string().len() + r#","approval":"#.len());
            }
        }
        rows
    }

    /// What each session looks like to the pusher: its journals, phase and open approval ids.
    pub(crate) fn snapshots(&self) -> Vec<Snapshot> {
        self.live
            .iter()
            .map(|live| {
                let state = live.state.as_ref();
                Snapshot {
                    id: live.registration.id.clone(),
                    journals: live.registration.journals.clone(),
                    phase: state
                        .and_then(|s| s.get("phase")?.as_str())
                        .map(str::to_owned),
                    approvals: state
                        .and_then(|s| s.get("approvals")?.as_array())
                        .map(|list| list.iter().filter_map(|a| a.get("id")?.as_u64()).collect())
                        .unwrap_or_default(),
                    errors: live.facts.errors,
                }
            })
            .collect()
    }

    /// How many sessions the hub's `start` launched are registered.
    pub(crate) fn phone_started(&self) -> usize {
        self.live
            .iter()
            .filter(|l| l.registration.from_phone)
            .count()
    }

    /// The journals of session `id`, oldest first.
    pub(crate) fn journals(&self, id: &str) -> Option<Vec<PathBuf>> {
        Some(self.find(id)?.registration.journals.clone())
    }

    /// The last state session `id` sent; none while it has no link.
    pub(crate) fn state(&self, id: &str) -> Option<&Value> {
        self.find(id)?.state.as_ref()
    }

    /// The last catalog session `id` sent; none while it has no link.
    pub(crate) fn catalog(&self, id: &str) -> Option<&Value> {
        self.find(id)?.catalog.as_ref()
    }

    /// Queues `command` for session `id`'s bridge, on behalf of the device `by`. It never
    /// waits for the bridge: one whose queue is full is let go (and linked again by the next
    /// refresh).
    ///
    /// # Errors
    ///
    /// Why the command was not queued, with its code: no such session, no link, or a bridge too
    /// slow.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the hub awaits sends like every session call"
    )]
    pub(crate) async fn send(
        &mut self,
        id: &str,
        req: u64,
        by: &str,
        command: &BridgeCommand,
    ) -> Result<(), (Code, &'static str)> {
        let live = self
            .live
            .iter_mut()
            .find(|l| l.registration.id == id)
            .ok_or((Code::Unavailable, "No such session"))?;
        let link = live
            .link
            .as_ref()
            .ok_or((Code::NotConnected, "The session is not connected"))?;
        let line = format!("{}\n", bridge_command_json(req, by, command));
        let failed = match link.lines.try_send(line) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                (Code::Busy, "The session is not keeping up")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                (Code::NotConnected, "The session disconnected")
            }
        };
        live.lose_link();
        self.push_changed();
        Err(failed)
    }

    /// Whether session `id` is there and its bridge linked.
    pub(crate) fn linked(&self, id: &str) -> bool {
        self.find(id).is_some_and(|live| live.link.is_some())
    }

    /// The next thing that happened. Cancel-safe: nothing is lost if the future is dropped.
    pub(crate) async fn next(&mut self) -> SessionUpdate {
        loop {
            if let Some(update) = self.pending.pop_front() {
                return update;
            }
            let tick = self.tick.get_or_insert_with(|| {
                let mut tick = interval(TAIL_EVERY);
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                tick
            });
            tokio::select! {
                Some(inbound) = self.updates.recv() => self.take(inbound),
                _ = tick.tick() => self.poll_tails().await,
            }
        }
    }

    fn find(&self, id: &str) -> Option<&Live> {
        self.live.iter().find(|l| l.registration.id == id)
    }

    fn push_changed(&mut self) {
        if self.pending.back() != Some(&SessionUpdate::Changed) {
            self.pending.push_back(SessionUpdate::Changed);
        }
    }

    /// Applies a frame from a bridge; one from a link that has since been replaced is ignored.
    fn take(&mut self, inbound: Inbound) {
        let Inbound {
            session,
            link,
            frame,
        } = inbound;
        let Some(live) = self.live.iter_mut().find(|l| l.registration.id == session) else {
            return;
        };
        if live.link.as_ref().is_none_or(|l| l.number != link) {
            return;
        }
        let update = match frame {
            Frame::State(state) => {
                live.state = Some(state);
                SessionUpdate::State { id: session }
            }
            Frame::Catalog(catalog) => {
                live.catalog = Some(catalog);
                SessionUpdate::Catalog { id: session }
            }
            Frame::Reply {
                req,
                ok,
                message,
                code,
            } => SessionUpdate::Reply {
                id: session,
                req,
                ok,
                message,
                code,
            },
            Frame::Closed => {
                live.lose_link();
                SessionUpdate::Changed
            }
        };
        self.pending.push_back(update);
    }

    /// Reads every session's journals for records written since the last pass: the journals
    /// it moved on from first, each let go once it stops or its time is up, then the newest. A
    /// change to the sessions' facts is one `Changed` after the events.
    async fn poll_tails(&mut self) {
        let now = Instant::now();
        let mut changed = false;
        for at in 0..self.live.len() {
            let live = &mut self.live[at];
            let id = &live.registration.id;
            let mut wrote = false;
            live.previous.retain(|previous| previous.until > now);
            let mut index = 0;
            while index < live.previous.len() {
                let tail = &mut live.previous[index].tail;
                let found = match tail.read().await {
                    Ok(records) => emit(
                        &mut self.pending,
                        &mut live.facts,
                        id,
                        &file_name(tail.path()),
                        &records,
                    ),
                    Err(_) => Emitted::default(),
                };
                wrote |= found.any;
                changed |= found.changed;
                if found.stopped {
                    live.previous.remove(index);
                } else {
                    index += 1;
                }
            }
            if let Some(tail) = live.tail.as_mut()
                && let Ok(records) = tail.read().await
            {
                let found = emit(
                    &mut self.pending,
                    &mut live.facts,
                    id,
                    &file_name(tail.path()),
                    &records,
                );
                wrote |= found.any;
                changed |= found.changed;
            }
            if wrote {
                live.last_activity = activity(&live.registration.journals).await;
            }
        }
        if changed {
            self.push_changed();
        }
    }

    /// Connects to `registration`'s bridge and starts the tasks that carry its lines.
    async fn connect(&mut self, registration: &Registration) -> Option<Link> {
        let is_socket = tokio::fs::symlink_metadata(&registration.socket)
            .await
            .is_ok_and(|m| m.file_type().is_socket());
        if !is_socket {
            return None;
        }
        let stream = timeout(CONNECT_WITHIN, UnixStream::connect(&registration.socket))
            .await
            .ok()?
            .ok()?;
        let (read, write) = stream.into_split();
        let (lines, queue) = mpsc::channel(LINK_QUEUE);
        self.links += 1;
        let number = self.links;
        let (id, sender) = (registration.id.clone(), self.sender.clone());
        let reader =
            tokio::spawn(read_frames(read, id.clone(), number, sender.clone())).abort_handle();
        let writer = tokio::spawn(write_lines(write, queue, id, number, sender)).abort_handle();
        Some(Link {
            number,
            lines,
            reader,
            writer,
        })
    }
}

/// Whether two registrations of one session differ in what a `sessions` row shows.
fn shows_differently(old: &Registration, new: &Registration) -> bool {
    (
        &old.engine,
        &old.model,
        &old.workspace,
        &old.mode,
        old.from_phone,
        &old.tmux,
    ) != (
        &new.engine,
        &new.model,
        &new.workspace,
        &new.mode,
        new.from_phone,
        &new.tmux,
    )
}

/// What [`emit`] queued.
#[derive(Debug, Default, Clone, Copy)]
struct Emitted {
    /// Any event.
    any: bool,
    /// A `stopped` record: the journal's session has ended.
    stopped: bool,
    /// A record changed the session's [`Facts`].
    changed: bool,
}

/// Queues an event for each record `records` holds, and keeps `facts` up to date with them.
fn emit(
    pending: &mut VecDeque<SessionUpdate>,
    facts: &mut Facts,
    id: &str,
    journal: &str,
    records: &[Value],
) -> Emitted {
    let mut emitted = Emitted::default();
    for (seq, record) in records.iter().filter_map(event_of) {
        emitted.any = true;
        emitted.stopped |= record["type"] == "stopped";
        if let Some(kind) = record["type"].as_str() {
            emitted.changed |= facts.take(kind, &record["data"]);
        }
        pending.push_back(SessionUpdate::Event {
            id: id.to_owned(),
            journal: journal.to_owned(),
            seq,
            record,
        });
    }
    emitted
}

/// Moves `live`'s tail to the newest of `journals`. What the old journal has left is emitted
/// first, then every journal between whole (every earlier one when it had no tail or its journal
/// left the list), and the tail starts the newest from its first byte. Each journal it moves on
/// from without a `stopped` record is kept in `previous`, to be read for a while longer. Whether
/// what it emitted changed the session's facts.
async fn follow(
    live: &mut Live,
    journals: &[PathBuf],
    pending: &mut VecDeque<SessionUpdate>,
) -> bool {
    let Some(newest) = journals.last() else {
        return false;
    };
    let current = live.tail.as_ref().map(|t| t.path().to_owned());
    if current.as_ref() == Some(newest) {
        return false;
    }
    let id = live.registration.id.clone();
    let until = Instant::now() + FOLLOW_PREVIOUS;
    let mut passed: Vec<Tail> = live.tail.take().into_iter().collect();
    // Journals before the newest that this tail has not passed come whole: all of the new
    // ones when it had none or its journal left the list, those after its journal otherwise.
    let start = current
        .and_then(|c| journals.iter().position(|j| *j == c))
        .map_or(0, |at| at + 1);
    let between = journals.get(start..journals.len() - 1).unwrap_or_default();
    passed.extend(between.iter().map(|path| Tail::new(path.clone())));
    let mut changed = false;
    for mut tail in passed {
        let drained = drain(&mut tail, &mut live.facts, &id, pending).await;
        changed |= drained.changed;
        if !drained.stopped {
            live.previous.push(Previous { tail, until });
        }
    }
    live.tail = Some(Tail::new(newest.clone()));
    changed
}

/// Queues the events of everything `tail` has left to read, keeping `facts` up to date; whether
/// one was a `stopped` record, and whether the facts changed.
async fn drain(
    tail: &mut Tail,
    facts: &mut Facts,
    id: &str,
    pending: &mut VecDeque<SessionUpdate>,
) -> Emitted {
    let journal = file_name(tail.path());
    let mut drained = Emitted::default();
    loop {
        let before = tail.offset();
        let Ok(records) = tail.read().await else {
            return drained;
        };
        let found = emit(pending, facts, id, &journal, &records);
        drained.any |= found.any;
        drained.stopped |= found.stopped;
        drained.changed |= found.changed;
        if tail.offset() <= before {
            return drained;
        }
    }
}

/// Seconds since the Unix epoch at which the newest of `journals` last changed.
async fn activity(journals: &[PathBuf]) -> Option<u64> {
    let modified = tokio::fs::metadata(journals.last()?)
        .await
        .ok()?
        .modified()
        .ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// Every registration file in `run`, with its path; the run directory is not trusted, so a
/// file over a megabyte or one that does not parse is passed over.
async fn scan(run: &Path) -> Vec<(PathBuf, Registration)> {
    let Ok(mut entries) = tokio::fs::read_dir(run).await else {
        return Vec::new();
    };
    let mut found = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        // A symlink, FIFO or directory named like a registration is not one.
        if !entry.file_type().await.is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(file) = tokio::fs::File::open(&path).await else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(REGISTRATION_LIMIT)
            .read_to_end(&mut bytes)
            .await
            .is_err()
        {
            continue;
        }
        let parsed = serde_json::from_slice::<Value>(&bytes).ok();
        if let Some(registration) = parsed.as_ref().and_then(Registration::from_json) {
            found.push((path, registration));
        }
    }
    found.sort_by_key(|(_, r)| (r.started, r.pid));
    found
}

/// Whether `registration`, read from `file`, is what the run directory `home` (canonical, if
/// it could be) should hold: named for its pid, with the id and socket that pid implies.
async fn vetted(home: Option<&Path>, file: &Path, registration: &Registration) -> bool {
    let pid = registration.pid;
    let socket = &registration.socket;
    let in_home = match (home, socket.parent()) {
        (Some(home), Some(parent)) => tokio::fs::canonicalize(parent)
            .await
            .is_ok_and(|p| p == home),
        _ => false,
    };
    file.file_name().and_then(|n| n.to_str()) == Some(&format!("{pid}.json"))
        && registration.id == format!("{pid}-{}", registration.started)
        && socket.file_name().and_then(|n| n.to_str()) == Some(&format!("{pid}.sock"))
        && in_home
}

/// What the journals a registration lists amount to.
#[derive(Debug, PartialEq, Eq)]
enum Journals {
    /// Every one is a regular file the hub may read.
    Fine,
    /// One is not a journal of this data directory, or not a regular file.
    Refused,
    /// One does not exist (yet).
    Missing,
}

/// Whether `journals` are all regular files (not symlinks) named like journals, directly in
/// `data`, the canonical data directory: the hub reads nothing else, and opens nothing that
/// could block, such as a FIFO.
async fn check_journals(data: Option<&Path>, journals: &[PathBuf]) -> Journals {
    let Some(data) = data else {
        return Journals::Refused;
    };
    let mut verified: Option<&Path> = None;
    let mut missing = false;
    for journal in journals {
        let Some(parent) = journal.parent() else {
            return Journals::Refused;
        };
        if octet_store::journal_stamp(journal).is_none() {
            return Journals::Refused;
        }
        if verified != Some(parent) {
            match tokio::fs::canonicalize(parent).await {
                Ok(canonical) if canonical == data => verified = Some(parent),
                _ => return Journals::Refused,
            }
        }
        match tokio::fs::symlink_metadata(journal).await {
            Ok(meta) if meta.file_type().is_file() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing = true,
            _ => return Journals::Refused,
        }
    }
    if missing {
        Journals::Missing
    } else {
        Journals::Fine
    }
}

/// Reads the bridge's lines and forwards its frames, until the link ends.
async fn read_frames(
    read: OwnedReadHalf,
    session: String,
    link: u64,
    sender: mpsc::Sender<Inbound>,
) {
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(LINE_LIMIT)
            .read_until(b'\n', &mut line)
            .await;
        // No newline means the bridge went away, or the line is over the limit.
        if !read.is_ok_and(|n| n > 0) || line.last() != Some(&b'\n') {
            break;
        }
        match parse_frame(&line) {
            Err(()) => break,
            Ok(None) => {}
            Ok(Some(frame)) => {
                let inbound = Inbound {
                    session: session.clone(),
                    link,
                    frame,
                };
                if sender.send(inbound).await.is_err() {
                    return;
                }
            }
        }
    }
    let _ = sender
        .send(Inbound {
            session,
            link,
            frame: Frame::Closed,
        })
        .await;
}

/// Writes the queued lines to the bridge, until the link ends.
async fn write_lines(
    mut write: OwnedWriteHalf,
    mut queue: mpsc::Receiver<String>,
    session: String,
    link: u64,
    sender: mpsc::Sender<Inbound>,
) {
    while let Some(line) = queue.recv().await {
        let sent = timeout(WRITE_STALL, write.write_all(line.as_bytes())).await;
        if !sent.is_ok_and(|result| result.is_ok()) {
            break;
        }
    }
    let _ = sender
        .send(Inbound {
            session,
            link,
            frame: Frame::Closed,
        })
        .await;
}

/// The frame a bridge line holds: `Err` for a line that is not a JSON object (the link goes),
/// `None` for one the hub has no use for.
fn parse_frame(line: &[u8]) -> Result<Option<Frame>, ()> {
    let Ok(Value::Object(message)) = serde_json::from_slice::<Value>(line) else {
        return Err(());
    };
    if message.get("v").and_then(Value::as_u64) != Some(VERSION) {
        return Ok(None);
    }
    let object = |key: &str| message.get(key).filter(|v| v.is_object()).cloned();
    let text = |key: &str| message.get(key).and_then(Value::as_str).map(str::to_owned);
    let req = message.get("req").and_then(Value::as_u64);
    Ok(match message.get("type").and_then(Value::as_str) {
        Some("state") => object("state").map(Frame::State),
        Some("catalog") => object("catalog").map(Frame::Catalog),
        Some("ack") => req.map(|req| Frame::Reply {
            req,
            ok: true,
            message: text("message").unwrap_or_default(),
            code: None,
        }),
        Some("refused") => req.map(|req| Frame::Reply {
            req,
            ok: false,
            message: text("reason").unwrap_or_default(),
            code: text("code").as_deref().and_then(Code::parse),
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_core::remote::{
        protocol::{ack, bridge_catalog_frame, bridge_state_frame, parse_bridge_command, refused},
        run::Registration,
    };
    use serde_json::json;
    use std::{
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        time::timeout,
    };

    /// A short run directory under `/tmp` (socket paths are limited to about 104 bytes).
    struct Env {
        root: PathBuf,
        run: PathBuf,
    }

    impl Env {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = PathBuf::from(format!("/tmp/oh{}-{n}", std::process::id()));
            let run = root.join("run");
            std::fs::create_dir_all(&run).unwrap();
            Self { root, run }
        }

        fn registration(&self, pid: u32, journals: &[&str]) -> Registration {
            Registration {
                id: format!("{pid}-1790000000"),
                pid,
                started: 1_790_000_000,
                socket: self.run.join(format!("{pid}.sock")),
                engine: "claude".into(),
                workspace: "/work".into(),
                model: "opus".into(),
                mode: "ask".into(),
                vendor_session: String::new(),
                journals: journals.iter().map(|j| self.root.join(j)).collect(),
                from_phone: false,
                tmux: None,
            }
        }

        fn listen(registration: &Registration) -> UnixListener {
            UnixListener::bind(&registration.socket).unwrap()
        }

        /// Waits until nothing listens on `registration`'s socket any more. On macOS a socket
        /// is made close-on-exec only after it is made, so a child another test spawns in that
        /// moment (a `sleep 5`, say) keeps a dropped listener open until it exits.
        async fn refused(registration: &Registration) {
            timeout(Duration::from_secs(10), async {
                while std::os::unix::net::UnixStream::connect(&registration.socket).is_ok() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("the dropped listener's socket refuses");
        }

        fn journal(&self, name: &str, seqs: std::ops::Range<u64>) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join(name))
                .unwrap();
            for seq in seqs {
                let line =
                    json!({"format": "octet-preview-1", "seq": seq, "type": "t", "data": name});
                writeln!(file, "{line}").unwrap();
            }
        }

        /// Appends one record of `kind` with `data` to the journal `name`, making it if need be.
        fn write(&self, name: &str, seq: u64, kind: &str, data: &Value) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join(name))
                .unwrap();
            let line = json!({"format": "octet-preview-1", "seq": seq, "type": kind, "data": data});
            writeln!(file, "{line}").unwrap();
        }

        /// Appends one record of `kind` to the journal `name`.
        fn record(&self, name: &str, seq: u64, kind: &str) {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(self.root.join(name))
                .unwrap();
            let line = json!({"format": "octet-preview-1", "seq": seq, "type": kind, "data": null});
            writeln!(file, "{line}").unwrap();
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn next(sessions: &mut Sessions) -> SessionUpdate {
        timeout(Duration::from_secs(5), sessions.next())
            .await
            .expect("an update arrives")
    }

    fn event(update: &SessionUpdate) -> (&str, u64) {
        match update {
            SessionUpdate::Event { journal, seq, .. } => (journal, *seq),
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_reconnect_continues_into_the_next_journal() {
        let env = Env::new();
        env.journal("session-1-1.jsonl", 0..2);
        let one = env.registration(4001, &["session-1-1.jsonl"]);
        let _bridge = Env::listen(&one);
        one.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(sessions.list().len(), 1);
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        // History before the hub looked is for replay, not for the live tail.
        env.journal("session-1-1.jsonl", 2..4);
        assert_eq!(event(&next(&mut sessions).await), ("session-1-1.jsonl", 2));
        assert_eq!(event(&next(&mut sessions).await), ("session-1-1.jsonl", 3));
        // The terminal UI reconnects: a record lands in `a` just before, then `b` begins.
        env.journal("session-1-1.jsonl", 4..5);
        env.journal("session-2-1.jsonl", 0..2);
        let two = env.registration(4001, &["session-1-1.jsonl", "session-2-1.jsonl"]);
        two.write(&env.run).await.unwrap();
        sessions.refresh().await;
        let got = [
            next(&mut sessions).await,
            next(&mut sessions).await,
            next(&mut sessions).await,
        ];
        assert_eq!(event(&got[0]), ("session-1-1.jsonl", 4));
        assert_eq!(event(&got[1]), ("session-2-1.jsonl", 0));
        assert_eq!(event(&got[2]), ("session-2-1.jsonl", 1));
        env.journal("session-2-1.jsonl", 2..3);
        assert_eq!(event(&next(&mut sessions).await), ("session-2-1.jsonl", 2));
    }

    #[tokio::test]
    async fn the_previous_journal_is_followed_until_it_stops() {
        let (old, new) = ("session-1-1.jsonl", "session-2-1.jsonl");
        let env = Env::new();
        env.journal(old, 0..2);
        let one = env.registration(4080, &[old]);
        let _bridge = Env::listen(&one);
        one.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        env.journal(new, 0..1);
        env.registration(4080, &[old, new])
            .write(&env.run)
            .await
            .unwrap();
        sessions.refresh().await;
        assert_eq!(event(&next(&mut sessions).await), (new, 0));
        // The old session's last words land after the switch: they still arrive, once.
        env.record(old, 2, "usage");
        assert_eq!(event(&next(&mut sessions).await), (old, 2));
        env.record(old, 3, "stopped");
        assert_eq!(event(&next(&mut sessions).await), (old, 3));
        // After its stop record the old journal is let go.
        env.record(old, 4, "error");
        env.journal(new, 1..2);
        assert_eq!(event(&next(&mut sessions).await), (new, 1));
        let quiet = timeout(Duration::from_millis(400), sessions.next()).await;
        assert!(quiet.is_err(), "{quiet:?}");
    }

    #[tokio::test]
    async fn the_previous_journal_is_let_go_after_ten_seconds() {
        let (old, new) = ("session-1-1.jsonl", "session-2-1.jsonl");
        let env = Env::new();
        env.journal(old, 0..1);
        let one = env.registration(4081, &[old]);
        let _bridge = Env::listen(&one);
        one.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        env.journal(new, 0..1);
        env.registration(4081, &[old, new])
            .write(&env.run)
            .await
            .unwrap();
        sessions.refresh().await;
        assert_eq!(event(&next(&mut sessions).await), (new, 0));
        assert_eq!(sessions.live[0].previous.len(), 1);
        // Its ten seconds are up: a record written after is not read.
        sessions.live[0].previous[0].until = Instant::now();
        env.record(old, 1, "usage");
        let quiet = timeout(Duration::from_millis(400), sessions.next()).await;
        assert!(quiet.is_err(), "{quiet:?}");
        assert!(sessions.live[0].previous.is_empty());
    }

    #[tokio::test]
    async fn journals_that_appear_later_are_read_whole_and_in_order() {
        let env = Env::new();
        let none = env.registration(4050, &[]);
        let _bridge = Env::listen(&none);
        none.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        // The first journal appears after the hub looked: all of it is new.
        env.journal("session-1-1.jsonl", 0..2);
        env.registration(4050, &["session-1-1.jsonl"])
            .write(&env.run)
            .await
            .unwrap();
        sessions.refresh().await;
        assert_eq!(event(&next(&mut sessions).await), ("session-1-1.jsonl", 0));
        assert_eq!(event(&next(&mut sessions).await), ("session-1-1.jsonl", 1));
        // Two reconnects between passes: `a`'s rest, all of `b`, then `c` as it is written.
        env.journal("session-1-1.jsonl", 2..3);
        env.journal("session-2-1.jsonl", 0..2);
        env.journal("session-3-1.jsonl", 0..1);
        let three = env.registration(
            4050,
            &[
                "session-1-1.jsonl",
                "session-2-1.jsonl",
                "session-3-1.jsonl",
            ],
        );
        three.write(&env.run).await.unwrap();
        sessions.refresh().await;
        let mut got = Vec::new();
        for _ in 0..4 {
            let update = next(&mut sessions).await;
            got.push((event(&update).0.to_owned(), event(&update).1));
        }
        let expected = [
            ("session-1-1.jsonl", 2),
            ("session-2-1.jsonl", 0),
            ("session-2-1.jsonl", 1),
            ("session-3-1.jsonl", 0),
        ];
        assert_eq!(got, expected.map(|(j, s)| (j.to_owned(), s)));
    }

    #[tokio::test]
    async fn journals_that_are_not_the_hubs_to_read_are_refused() {
        let env = Env::new();
        let outside = Env::new();
        let fifo = env.root.join("session-9-1.jsonl");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|status| status.success()) {
            return;
        }
        env.journal("notes.jsonl", 0..1);
        env.journal("real-session-1-1.jsonl", 0..1);
        outside.journal("session-8-1.jsonl", 0..1);
        env.journal("session-7-1.jsonl", 0..1);
        std::os::unix::fs::symlink(
            env.root.join("session-7-1.jsonl"),
            env.root.join("session-6-1.jsonl"),
        )
        .unwrap();
        let cases = [
            (4060, vec![fifo]),
            (4061, vec![outside.root.join("session-8-1.jsonl")]),
            (4062, vec![env.root.join("notes.jsonl")]),
            (4063, vec![env.root.join("session-6-1.jsonl")]),
            (
                4064,
                vec![
                    env.root.join("session-7-1.jsonl"),
                    env.root.join("session-5-1.jsonl"),
                ],
            ),
            (4065, vec![env.root.join("sub/../session-7-1.jsonl")]),
        ];
        let mut keep = Vec::new();
        for (pid, journals) in cases {
            let mut registration = env.registration(pid, &[]);
            registration.journals = journals;
            keep.push(Env::listen(&registration));
            registration.write(&env.run).await.unwrap();
        }
        // The good one, beside them.
        env.journal("session-4-1.jsonl", 0..1);
        let good = env.registration(4066, &["session-4-1.jsonl"]);
        keep.push(Env::listen(&good));
        good.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        timeout(Duration::from_secs(5), sessions.refresh())
            .await
            .expect("a journal that is not a file does not wedge the hub");
        let ids: Vec<_> = sessions
            .list()
            .iter()
            .map(|row| row["id"].clone())
            .collect();
        assert_eq!(ids, [json!("4066-1790000000")]);
        for pid in [4060, 4061, 4062, 4063, 4065] {
            assert!(!env.run.join(format!("{pid}.json")).exists(), "{pid} stays");
        }
        // A journal that does not exist yet is not wrong: the registration waits for it.
        assert!(env.run.join("4064.json").exists());
    }

    #[tokio::test]
    async fn a_session_that_gains_journals_loses_none_of_their_records() {
        let env = Env::new();
        let none = env.registration(4070, &[]);
        let _bridge = Env::listen(&none);
        none.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        env.journal("session-1-1.jsonl", 0..2);
        env.journal("session-2-1.jsonl", 0..1);
        let two = env.registration(4070, &["session-1-1.jsonl", "session-2-1.jsonl"]);
        two.write(&env.run).await.unwrap();
        sessions.refresh().await;
        for want in [
            ("session-1-1.jsonl", 0),
            ("session-1-1.jsonl", 1),
            ("session-2-1.jsonl", 0),
        ] {
            assert_eq!(event(&next(&mut sessions).await), want);
        }
        assert_eq!(
            sessions.journals("4070-1790000000"),
            Some(two.journals.clone())
        );
        // The journal it followed leaves the list: the earlier ones in the new list come whole.
        env.journal("session-3-1.jsonl", 0..2);
        env.journal("session-4-1.jsonl", 0..1);
        let moved = env.registration(4070, &["session-3-1.jsonl", "session-4-1.jsonl"]);
        moved.write(&env.run).await.unwrap();
        sessions.refresh().await;
        for want in [
            ("session-3-1.jsonl", 0),
            ("session-3-1.jsonl", 1),
            ("session-4-1.jsonl", 0),
        ] {
            assert_eq!(event(&next(&mut sessions).await), want);
        }
        assert_eq!(sessions.journals("nobody"), None);
    }

    #[tokio::test]
    async fn a_registration_whose_socket_refuses_is_dropped_and_removed() {
        let env = Env::new();
        let dead = env.registration(4002, &[]);
        dead.write(&env.run).await.unwrap();
        // A stale socket file nobody listens on.
        drop(Env::listen(&dead));
        assert!(dead.socket.exists());
        Env::refused(&dead).await;
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert!(sessions.list().is_empty());
        assert!(!env.run.join("4002.json").exists());
        assert!(!dead.socket.exists());
        // A registration with no socket at all goes the same way.
        let missing = env.registration(4003, &[]);
        missing.write(&env.run).await.unwrap();
        sessions.refresh().await;
        assert!(sessions.list().is_empty());
        assert!(!env.run.join("4003.json").exists());
    }

    #[tokio::test]
    async fn a_registration_that_lies_is_not_trusted() {
        let env = Env::new();
        // Its socket is outside the run directory: not connected to, not removed.
        let outside = env.root.join("outside.sock");
        let _outside = UnixListener::bind(&outside).unwrap();
        let mut stray = env.registration(4004, &[]);
        stray.socket = outside.clone();
        stray.write(&env.run).await.unwrap();
        // Its file name says another pid.
        let honest = env.registration(4005, &[]);
        let _bridge = Env::listen(&honest);
        std::fs::write(env.run.join("9999.json"), honest.to_json().to_string()).unwrap();
        // Its id does not follow from its pid and start time.
        let mut renamed = env.registration(4006, &[]);
        renamed.id = "someone-else".into();
        let _renamed = Env::listen(&renamed);
        renamed.write(&env.run).await.unwrap();
        // Its socket is a symlink to a live one.
        let mut link = env.registration(4007, &[]);
        std::os::unix::fs::symlink(&outside, &link.socket).unwrap();
        link.id = "4007-1790000000".into();
        link.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert!(sessions.list().is_empty());
        assert!(outside.exists());
        for name in ["4004.json", "9999.json", "4006.json", "4007.json"] {
            assert!(!env.run.join(name).exists(), "{name} stays");
        }
    }

    #[tokio::test]
    async fn bridge_frames_and_replies_reach_the_hub() {
        let env = Env::new();
        env.journal("session-1-1.jsonl", 0..1);
        let registration = env.registration(4010, &["session-1-1.jsonl"]);
        let bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        let (stream, _) = bridge.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let state = json!({"phase": "running", "queue": [], "queued": 0, "mode": "ask",
            "goal": {"objective": "ship it\nplease", "status": "active", "turns": 1, "max": 9},
            "approvals": [{"id": 1, "summary": {"action": "Run Bash", "command": "ls",
                "files": [], "scope": "this call"}}, {"id": 2, "summary": {"action": "Edit"}}]});
        let lines = [
            bridge_state_frame(&state),
            bridge_catalog_frame(&json!({"modes": ["ask"]})),
            ack(7, "queued"),
            refused(8, Code::Busy, "busy"),
        ];
        for line in lines {
            write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        let id = "4010-1790000000".to_owned();
        assert_eq!(
            next(&mut sessions).await,
            SessionUpdate::State { id: id.clone() }
        );
        assert_eq!(
            next(&mut sessions).await,
            SessionUpdate::Catalog { id: id.clone() }
        );
        let reply = |req, ok: bool, message: &str, code| SessionUpdate::Reply {
            id: id.clone(),
            req,
            ok,
            message: message.into(),
            code,
        };
        assert_eq!(next(&mut sessions).await, reply(7, true, "queued", None));
        assert_eq!(
            next(&mut sessions).await,
            reply(8, false, "busy", Some(Code::Busy))
        );
        assert_eq!(sessions.state(&id), Some(&state));
        assert_eq!(sessions.catalog(&id), Some(&json!({"modes": ["ask"]})));
        let rows = sessions.list();
        assert_eq!(rows.len(), 1);
        let mtime = std::fs::metadata(env.root.join("session-1-1.jsonl"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(
            rows[0],
            json!({"id": id, "engine": "claude", "model": "opus", "workspace": "/work",
                "mode": "ask", "phase": "running", "approvals": 2, "last_activity": mtime,
                "goal": "ship it", "from_phone": false,
                "approval": {"id": 1, "summary": {"action": "Run Bash", "command": "ls",
                    "files": [], "scope": "this call"}}})
        );
        // The hub's commands reach the bridge as parsable lines.
        let command = BridgeCommand::Prompt {
            text: "hello".into(),
        };
        sessions.send(&id, 11, "phone", &command).await.unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        assert_eq!(
            parse_bridge_command(&line),
            Ok((11, "phone".into(), command.clone()))
        );
        assert!(sessions.send("nobody", 1, "phone", &command).await.is_err());
    }

    #[tokio::test]
    async fn a_row_carries_the_now_line_and_the_tmux_session() {
        let env = Env::new();
        let mut registration = env.registration(4011, &[]);
        registration.from_phone = true;
        registration.tmux = Some("octet-abc123".into());
        let bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        let (stream, _) = bridge.accept().await.unwrap();
        let (_read, mut write) = stream.into_split();
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        let row = &sessions.list()[0];
        assert_eq!(row["tmux"], "octet-abc123");
        assert!(row.get("now").is_none(), "{row}");
        let state = json!({"phase": "running", "queue": [], "queued": 0, "mode": "ask",
            "now": "Running Bash cargo test", "approvals": []});
        write
            .write_all(format!("{}\n", bridge_state_frame(&state)).as_bytes())
            .await
            .unwrap();
        let id = "4011-1790000000".to_owned();
        assert_eq!(next(&mut sessions).await, SessionUpdate::State { id });
        assert_eq!(sessions.list()[0]["now"], "Running Bash cargo test");
    }

    /// The worst summary the bridge's clip lets through: a 4 KiB command, 20 files of 512 bytes,
    /// and long action, scope and cwd.
    fn worst_summary() -> Value {
        json!({"action": "A".repeat(2000), "command": "é".repeat(4096), "scope": "S".repeat(2000),
            "cwd": "/w".repeat(1000),
            "files": (0..20).map(|i| format!("{i}{}", "f".repeat(511))).collect::<Vec<_>>()})
    }

    fn live_with(env: &Env, pid: u32, state: Value) -> Live {
        Live {
            registration: env.registration(pid, &[]),
            link: None,
            tail: None,
            previous: Vec::new(),
            state: Some(state),
            catalog: None,
            last_activity: None,
            facts: Facts::default(),
        }
    }

    /// The updates up to and including the next `Changed`.
    async fn until_changed(sessions: &mut Sessions) -> Vec<SessionUpdate> {
        let mut got = Vec::new();
        loop {
            let update = next(sessions).await;
            let changed = update == SessionUpdate::Changed;
            got.push(update);
            if changed {
                return got;
            }
        }
    }

    #[test]
    fn question_tail_is_bounded() {
        let mut facts = Facts::default();
        facts.take("user", &json!("go on"));
        // A reply of 10,000 chunks of 2 KiB: the tail never holds more than 256 characters, and
        // each chunk costs the same however long the reply has grown.
        let chunk = json!(format!("{} ", "é".repeat(1023)));
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            facts.take("text", &chunk);
            assert!(facts.reply.chars().count() <= REPLY_CHARS);
            assert!(facts.reply.len() <= REPLY_CHARS * 4);
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // One huge chunk is read from its end only.
        facts.take(
            "text",
            &json!(format!("{} Shall I go on?\n", "é".repeat(1_000_000))),
        );
        assert!(facts.reply.chars().count() <= REPLY_CHARS);
        assert!(facts.reply.ends_with("Shall I go on?\n"), "{}", facts.reply);
        facts.take("finished", &json!("completed"));
        assert!(facts.question);
    }

    #[test]
    fn a_reply_ending_in_a_question_asks_until_the_next_user_record() {
        let env = Env::new();
        let mut live = live_with(&env, 4091, json!({"phase": "idle", "approvals": []}));
        assert!(!live.facts.take("user", &json!("Fix the build")));
        live.facts.take("text", &json!("Fixed it. Shall I"));
        live.facts.take("text", &json!(" push too?  \n\n"));
        // Not before the turn is over.
        assert!(live.row().get("question").is_none());
        assert!(live.facts.take("finished", &json!("completed")));
        assert_eq!(live.row()["question"], true);
        // The answer clears it.
        assert!(live.facts.take("user", &json!("yes")));
        assert!(live.row().get("question").is_none());
        // A question mark that is not the reply's last word is no question.
        live.facts
            .take("text", &json!("Why? Because it was stale."));
        assert!(!live.facts.take("finished", &json!("completed")));
        assert!(live.row().get("question").is_none());
        // Nor is a reply from before the last user record.
        live.facts.take("text", &json!("Want more?"));
        live.facts.take("user", &json!("next"));
        live.facts.take("text", &json!("Done."));
        live.facts.take("finished", &json!("completed"));
        assert!(!live.facts.question);
    }

    #[test]
    fn usage_is_latest_wins_and_clipped() {
        let env = Env::new();
        let mut live = live_with(&env, 4092, json!({"phase": "idle", "approvals": []}));
        assert!(live.row().get("usage").is_none());
        assert!(live.facts.take("usage", &json!("5h 10% · week 2%")));
        assert_eq!(live.row()["usage"], "5h 10% · week 2%");
        assert!(!live.facts.take("usage", &json!("5h 10% · week 2%")));
        assert!(live.facts.take("usage", &json!("é".repeat(500))));
        assert_eq!(live.row()["usage"], "é".repeat(200));
        // A record that is not text changes nothing.
        assert!(!live.facts.take("usage", &json!(7)));
        assert_eq!(live.row()["usage"], "é".repeat(200));
        // Errors are counted, for the pusher; the row does not show them.
        assert!(live.facts.take("error", &json!("boom")));
        assert!(live.facts.take("error", &Value::Null));
        assert_eq!(live.facts.errors, 2);
        assert!(live.row().get("errors").is_none());
    }

    #[tokio::test]
    async fn the_seed_reads_at_most_256_kib() {
        let env = Env::new();
        let name = "session-1-1.jsonl";
        let path = env.root.join(name);
        let size = || std::fs::metadata(&path).unwrap().len();
        env.write(name, 0, "usage", &json!("old 1%"));
        env.write(name, 1, "user", &json!("first"));
        env.write(name, 2, "text", &json!("An old question?"));
        env.write(name, 3, "finished", &json!("completed"));
        // Over 256 KiB after them: what came before is not read.
        for seq in 4..304 {
            env.write(name, seq, "notice", &json!("n".repeat(1000)));
        }
        assert!(size() > SEED_BYTES + 1024);
        let (facts, read) = seed(&path, size()).await;
        assert!(read <= SEED_BYTES, "{read}");
        assert_eq!((facts.usage, facts.question), (None, false));
        // What is inside the window seeds, but its errors are from before the hub looked.
        env.write(name, 304, "usage", &json!("new 2%"));
        env.write(name, 305, "user", &json!("again"));
        env.write(name, 306, "error", &json!("boom"));
        env.write(name, 307, "text", &json!("Ready to ship?"));
        env.write(name, 308, "finished", &json!("completed"));
        let (facts, read) = seed(&path, size()).await;
        assert!(read <= SEED_BYTES, "{read}");
        assert_eq!(facts.usage.as_deref(), Some("new 2%"));
        assert!(facts.question);
        // A session the hub first sees gets its row from the seed.
        let registration = env.registration(4093, &[name]);
        let _bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        let row = &sessions.list()[0];
        assert_eq!(
            (&row["usage"], &row["question"]),
            (&json!("new 2%"), &json!(true))
        );
        assert_eq!(sessions.snapshots()[0].errors, 0);
        // A journal too small to fill the window is read whole.
        let small = env.root.join("session-2-1.jsonl");
        env.write("session-2-1.jsonl", 0, "usage", &json!("tiny"));
        let (facts, read) = seed(&small, std::fs::metadata(&small).unwrap().len()).await;
        assert_eq!(facts.usage.as_deref(), Some("tiny"));
        assert_eq!(read, std::fs::metadata(&small).unwrap().len());
    }

    #[tokio::test]
    async fn the_tail_keeps_usage_errors_and_the_question() {
        let env = Env::new();
        let name = "session-1-1.jsonl";
        env.write(name, 0, "error", &json!("from before"));
        let registration = env.registration(4094, &[name]);
        let _bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        assert_eq!(sessions.snapshots()[0].errors, 0);
        env.write(name, 1, "user", &json!("Fix it"));
        env.write(name, 2, "text", &json!("Fixed. Shall I push?"));
        env.write(name, 3, "usage", &json!("5h 3%"));
        env.write(name, 4, "error", &json!("rate limited"));
        env.write(name, 5, "finished", &json!("completed"));
        // The records go out as events, then the row is said to have changed.
        let got = until_changed(&mut sessions).await;
        assert_eq!(got.len(), 6, "{got:?}");
        assert_eq!(event(&got[4]), (name, 5));
        let row = &sessions.list()[0];
        assert_eq!(
            (&row["usage"], &row["question"]),
            (&json!("5h 3%"), &json!(true))
        );
        assert_eq!(sessions.snapshots()[0].errors, 1);
        // Text alone changes nothing the row shows, so it is not a change.
        env.write(name, 6, "text", &json!("more"));
        assert_eq!(event(&next(&mut sessions).await), (name, 6));
        let quiet = timeout(Duration::from_millis(300), sessions.next()).await;
        assert!(quiet.is_err(), "{quiet:?}");
        env.write(name, 7, "user", &json!("yes"));
        let got = until_changed(&mut sessions).await;
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(sessions.list()[0].get("question").is_none());
    }

    #[test]
    fn the_rows_approval_is_a_gist_of_the_summary() {
        let env = Env::new();
        let state = json!({"phase": "waiting", "approvals": [
            {"id": 3, "summary": worst_summary()}]});
        let row = live_with(&env, 4030, state).row();
        let approval = &row["approval"];
        assert_eq!(approval["id"], 3);
        let summary = &approval["summary"];
        let cut = |c: &str| format!("{}…", c.repeat(200));
        assert_eq!(summary["action"], cut("A"));
        assert_eq!(summary["command"], cut("é"));
        assert_eq!(summary["scope"], cut("S"));
        assert_eq!(summary["cwd"], format!("{}…", "/w".repeat(100)));
        let files = summary["files"].as_array().unwrap();
        assert_eq!(files.len(), 4);
        for (i, file) in files.iter().take(3).enumerate() {
            assert_eq!(*file, json!(format!("{i}{}…", "f".repeat(199))));
        }
        assert_eq!(files[3], "+17 more");
    }

    #[test]
    fn the_gist_keeps_a_commands_first_line_and_short_text_whole() {
        let env = Env::new();
        let summary = json!({"action": "Run Bash", "command": "\n\nls -la\nrm -rf /", "files": ["a", "b", "c"],
            "scope": "once"});
        let state = json!({"approvals": [{"id": 1, "summary": summary}]});
        let row = live_with(&env, 4031, state).row();
        assert_eq!(
            row["approval"]["summary"],
            json!({"action": "Run Bash", "command": "ls -la…", "files": ["a", "b", "c"],
                "scope": "once"})
        );
        let one = json!({"approvals": [{"id": 1, "summary": {"command": "ls\n"}}]});
        assert_eq!(
            live_with(&env, 4032, one).row()["approval"]["summary"]["command"],
            "ls"
        );
    }

    #[test]
    fn a_front_approval_without_an_object_summary_is_left_out() {
        let env = Env::new();
        for approval in [
            json!({"id": 1}),
            json!({"id": 1, "summary": "rm"}),
            json!({"id": 1, "summary": null}),
            json!({"id": "1", "summary": {}}),
            json!({"summary": {}}),
        ] {
            let row = live_with(&env, 4033, json!({"approvals": [approval]})).row();
            assert!(row.get("approval").is_none(), "{row}");
            assert_eq!(row["approvals"], 1);
        }
    }

    /// A row's facts at their largest: the longest usage line, and a question.
    fn worst_facts(live: &mut Live) {
        live.facts.take("usage", &json!("é".repeat(5000)));
        live.facts.take("text", &json!("?"));
        live.facts.take("finished", &json!("completed"));
    }

    #[test]
    fn sixteen_worst_case_rows_fit_one_sessions_frame() {
        let env = Env::new();
        let rows: Vec<Value> = (0..16)
            .map(|i| {
                let state = json!({"phase": "waiting", "now": "n".repeat(300),
                    "goal": {"objective": "g".repeat(5000)},
                    "approvals": [{"id": i, "summary": worst_summary()}]});
                let mut live = live_with(&env, 4100 + i, state);
                worst_facts(&mut live);
                live.row()
            })
            .collect();
        let frame = json!({"v": 1, "type": "sessions", "sessions": rows}).to_string();
        assert!(
            frame.len() < octet_core::remote::protocol::MAX_MESSAGE,
            "{} bytes",
            frame.len()
        );
    }

    #[test]
    fn the_rows_now_is_clipped_like_the_goal() {
        let env = Env::new();
        let state = json!({"phase": "running", "now": "é".repeat(5000), "approvals": []});
        let row = live_with(&env, 4040, state).row();
        assert_eq!(row["now"], "é".repeat(200));
        let short = json!({"phase": "running", "now": "Running Bash ls", "approvals": []});
        assert_eq!(live_with(&env, 4041, short).row()["now"], "Running Bash ls");
    }

    #[test]
    fn sixty_worst_case_rows_fit_and_the_most_recent_keep_their_gists() {
        let env = Env::new();
        let mut sessions = Sessions::new(env.run.clone());
        for i in 0..60u32 {
            let state = json!({"phase": "waiting", "now": "é".repeat(300),
                "goal": {"objective": "é".repeat(5000)},
                "approvals": [{"id": i, "summary": worst_summary()}]});
            let mut live = live_with(&env, 4200 + i, state);
            // The newest activity is in the middle of the list, the oldest at its ends.
            live.last_activity = Some(1_790_000_000 + u64::from(30 - i.abs_diff(30)));
            sessions.live.push(live);
        }
        let rows = sessions.list();
        assert_eq!(rows.len(), 60);
        let frame = json!({"v": 1, "type": "sessions", "sessions": rows}).to_string();
        assert!(
            frame.len() < octet_core::remote::protocol::MAX_MESSAGE,
            "{} bytes",
            frame.len()
        );
        assert!(rows.iter().all(|row| row["approvals"] == 1));
        assert!(
            rows[30].get("approval").is_some(),
            "the newest keeps its gist"
        );
        assert!(
            rows[0].get("approval").is_none(),
            "the oldest loses its gist"
        );
        let kept = rows
            .iter()
            .filter(|row| row.get("approval").is_some())
            .count();
        assert!(kept > 0 && kept < 60, "{kept}");
        // Every row that kept its gist is at least as recent as every row that lost one.
        let recency = |row: &Value| row["last_activity"].as_u64().unwrap();
        let newest_lost = rows
            .iter()
            .filter(|row| row.get("approval").is_none())
            .map(recency)
            .max()
            .unwrap();
        let oldest_kept = rows
            .iter()
            .filter(|row| row.get("approval").is_some())
            .map(recency)
            .min()
            .unwrap();
        assert!(oldest_kept >= newest_lost);
        // Every row's facts at their largest too: the gists give way, and the frame still fits.
        for live in &mut sessions.live {
            worst_facts(live);
        }
        let rows = sessions.list();
        let frame = json!({"v": 1, "type": "sessions", "sessions": rows}).to_string();
        assert!(
            frame.len() < octet_core::remote::protocol::MAX_MESSAGE,
            "{} bytes",
            frame.len()
        );
        assert!(rows.iter().all(|row| row["question"] == true));
        // A few rows are under the budget and keep every gist.
        sessions.live.truncate(4);
        assert!(
            sessions
                .list()
                .iter()
                .all(|row| row.get("approval").is_some())
        );
    }

    #[tokio::test]
    async fn a_row_carries_the_first_approval_while_one_waits() {
        let env = Env::new();
        let registration = env.registration(4012, &[]);
        let bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        let (stream, _) = bridge.accept().await.unwrap();
        let (_read, mut write) = stream.into_split();
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        assert!(sessions.list()[0].get("approval").is_none());
        let id = "4012-1790000000".to_owned();
        let summary = |action: &str| {
            json!({"action": action, "command": "rm -rf build", "files": [], "cwd": "/work",
                "scope": "this call"})
        };
        let mut state = json!({"phase": "waiting", "queue": [], "queued": 0, "mode": "ask",
            "approvals": [{"id": 7, "summary": summary("Run Bash")},
                {"id": 8, "summary": summary("Edit")}]});
        write
            .write_all(format!("{}\n", bridge_state_frame(&state)).as_bytes())
            .await
            .unwrap();
        assert_eq!(
            next(&mut sessions).await,
            SessionUpdate::State { id: id.clone() }
        );
        let row = &sessions.list()[0];
        assert_eq!(row["approvals"], 2);
        assert_eq!(
            row["approval"],
            json!({"id": 7, "summary": summary("Run Bash")})
        );
        state["approvals"] = json!([]);
        write
            .write_all(format!("{}\n", bridge_state_frame(&state)).as_bytes())
            .await
            .unwrap();
        assert_eq!(next(&mut sessions).await, SessionUpdate::State { id });
        let row = &sessions.list()[0];
        assert_eq!(row["approvals"], 0);
        assert!(row.get("approval").is_none(), "{row}");
    }

    #[tokio::test]
    async fn a_bridge_that_misbehaves_loses_its_link_and_gets_it_back() {
        let env = Env::new();
        let registration = env.registration(4020, &[]);
        let bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        let (mut stream, _) = bridge.accept().await.unwrap();
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        let id = "4020-1790000000";
        let command = BridgeCommand::Cancel;
        sessions.send(id, 1, "p", &command).await.unwrap();
        // An oversize line drops the link.
        let flood = async {
            let _ = stream.write_all(&vec![b'x'; 200 * 1024]).await;
            let _ = stream.write_all(b"\n").await;
        };
        let (update, ()) = tokio::join!(next(&mut sessions), flood);
        assert_eq!(update, SessionUpdate::Changed);
        assert!(sessions.send(id, 2, "p", &command).await.is_err());
        // The registration is still alive, so the next pass links again.
        sessions.refresh().await;
        let (mut second, _) = timeout(Duration::from_secs(5), bridge.accept())
            .await
            .expect("hub reconnects")
            .unwrap();
        assert!(sessions.send(id, 3, "p", &command).await.is_ok());
        // Garbage that is not JSON drops it too.
        second.write_all(b"not json\n").await.unwrap();
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        assert!(sessions.send(id, 4, "p", &command).await.is_err());
        assert_eq!(sessions.list().len(), 1);
    }

    #[tokio::test]
    async fn a_slow_bridge_never_blocks_the_hub() {
        let env = Env::new();
        let registration = env.registration(4030, &[]);
        let bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        let (_silent, _) = bridge.accept().await.unwrap();
        let id = "4030-1790000000";
        let command = BridgeCommand::Prompt {
            text: "x".repeat(60 * 1024),
        };
        let sent = timeout(Duration::from_secs(5), async {
            for req in 0..10_000 {
                if sessions.send(id, req, "p", &command).await.is_err() {
                    return Some(req);
                }
            }
            None
        })
        .await
        .expect("send never waits for the bridge");
        assert!(sent.is_some(), "a bridge that never reads is let go");
    }

    #[tokio::test]
    async fn a_vanished_registration_drops_its_session() {
        let env = Env::new();
        let registration = env.registration(4040, &[]);
        let _bridge = Env::listen(&registration);
        registration.write(&env.run).await.unwrap();
        let mut sessions = Sessions::new(env.run.clone());
        sessions.refresh().await;
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        registration.remove(&env.run).await;
        sessions.refresh().await;
        assert!(sessions.list().is_empty());
        assert_eq!(next(&mut sessions).await, SessionUpdate::Changed);
        assert!(sessions.state("4040-1790000000").is_none());
    }
}
