//! Content-free push through Expo: a phone with a push token and no open connection is told that
//! an approval is waiting, or that a turn finished, and nothing more.
//!
//! [`Pusher`] is pure: it is fed what the sessions look like ([`Snapshot`]) and the time, and says
//! what is due. It pushes for an approval id it has not seen in a session, and for a turn that
//! stopped running. A session's first state seeds what it has seen and pushes nothing, so a hub
//! restart does not repeat what the phone was already told. Pushes to one device collect for
//! [`WINDOW`] and go out as one (an approval beats a finished turn, and the push names the most
//! recent session), at most [`CAP`] in any minute. A device that is connected when its window
//! closes gets nothing, but the approvals it was spared are held: when its last connection drops,
//! those still open are offered to it again.
//!
//! Two edges are accepted. When a session's journal list changes (a new vendor process) the
//! approval ids it has seen are forgotten, so a reused id can stay hidden until the registration
//! refresh shows the new list, and an approval still open in the old state can be pushed once more.
//! A session that disappears mid-turn pushes nothing: it is forgotten with its phase.
//!
//! [`send`] posts a batch with `curl`: literal argv, the JSON body on stdin, so no token is in
//! `ps`. The only log lines are `Push failed` and `Push refused: <code>`.
use serde_json::{Value, json};
use std::sync::Arc;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{Semaphore, mpsc},
    time::{Instant, timeout},
};

/// Where pushes go.
pub(crate) const EXPO_URL: &str = "https://exp.host/--/api/v2/push/send";
/// The environment variable that points pushes at a local fake, in tests only.
const SEAM: &str = "OCTET_HUB_PUSH_URL";
/// The only prefix the seam is honoured for.
const LOOPBACK: &str = "http://127.0.0.1:";
/// How long pushes to one device collect before one goes out.
pub(crate) const WINDOW: Duration = Duration::from_secs(2);
/// Pushes one device gets in any rolling minute, at most.
const CAP: usize = 6;
/// The rolling minute of [`CAP`].
const MINUTE: Duration = Duration::from_secs(60);
/// How long one send may take in all; `curl` itself is told 10 seconds.
const SEND_WITHIN: Duration = Duration::from_secs(15);
/// Sends that may run at once; one that finds them all busy is dropped.
const SENDS_AT_ONCE: usize = 4;
/// Notices kept per window; older ones are let go.
const WINDOW_NOTICES: usize = 64;
/// The longest error code that is logged.
const CODE_CHARS: usize = 64;

/// Where to post: Expo, or, when `OCTET_HUB_PUSH_URL` names a loopback address, that.
pub(crate) fn endpoint() -> String {
    endpoint_from(std::env::var(SEAM).ok().as_deref())
}

/// [`endpoint`] for a given seam value. Only `http://127.0.0.1:<port>[/<path>]` is taken, with a
/// plain path: anything that could name another host (`@`) or add an argument is not.
fn endpoint_from(seam: Option<&str>) -> String {
    let local = seam.and_then(|url| {
        let rest = url.strip_prefix(LOOPBACK)?;
        let (port, path) = rest.split_once('/').map_or((rest, ""), |(p, q)| (p, q));
        let plain = |c: char| c.is_ascii_alphanumeric() || "/-._~".contains(c);
        let port_ok = (1..=5).contains(&port.len()) && port.chars().all(|c| c.is_ascii_digit());
        (port_ok && path.chars().all(plain)).then_some(url)
    });
    local.unwrap_or(EXPO_URL).to_owned()
}

/// What one session looked like at one moment.
#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    /// The hub's opaque session id.
    pub id: String,
    /// The session's journals, oldest first; a new list means a new vendor process.
    pub journals: Vec<PathBuf>,
    /// The state's phase; none while the session has no state.
    pub phase: Option<String>,
    /// The ids of the approvals open now.
    pub approvals: Vec<u64>,
}

/// Something worth a push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    /// An approval id new to the session.
    Approval { session: String, id: u64 },
    /// A turn that stopped running.
    Turn { session: String },
}

/// What kind of push goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `Approval waiting`.
    Approval,
    /// `Turn finished`.
    Turn,
}

/// The one push due to a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Due {
    pub kind: Kind,
    pub session: String,
}

impl Due {
    /// The Expo message for `token`: a fixed title and body, the session's opaque id, nothing else.
    pub(crate) fn message(&self, token: &str) -> Value {
        let mut message = json!({
            "to": token,
            "title": "Octet",
            "data": {"session": self.session},
        });
        let (body, priority, channel) = match self.kind {
            Kind::Approval => ("Approval waiting", "high", "approvals"),
            Kind::Turn => ("Turn finished", "normal", "turns"),
        };
        message["body"] = json!(body);
        message["priority"] = json!(priority);
        message["channelId"] = json!(channel);
        if self.kind == Kind::Approval {
            message["sound"] = json!("default");
        }
        message
    }
}

/// What the pusher remembers of one session.
struct Seen {
    journals: Vec<PathBuf>,
    phase: Option<String>,
    /// Every approval id seen since the journals last changed.
    approvals: HashSet<u64>,
    /// The approval ids open now.
    open: HashSet<u64>,
}

/// Notices collecting for one device.
struct Window {
    opened: Instant,
    approvals: Vec<(String, u64)>,
    turns: Vec<String>,
}

/// Decides what to push and when.
#[derive(Default)]
pub(crate) struct Pusher {
    /// By session id.
    seen: HashMap<String, Seen>,
    /// By device key.
    windows: HashMap<String, Window>,
    /// When each device was last pushed to, by device key.
    sent: HashMap<String, VecDeque<Instant>>,
    /// Approvals held back from a device because it was connected, by device key: offered again
    /// when its last connection drops, if they are still open.
    held: HashMap<String, Vec<(String, u64)>>,
}

// Device keys identify devices in `devices.json`: Debug shows counts only.
impl std::fmt::Debug for Pusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pusher")
            .field("sessions", &self.seen.len())
            .field("waiting", &self.windows.len())
            .field("recipients", &self.sent.len())
            .field("held", &self.held.len())
            .finish()
    }
}

impl Pusher {
    /// Takes the sessions as they stand; returns what is new since the last call. A session seen
    /// for the first time with a state only seeds. A session that is gone is forgotten.
    pub(crate) fn observe(&mut self, snapshots: &[Snapshot]) -> Vec<Notice> {
        let mut notices = Vec::new();
        for snapshot in snapshots {
            let Some(seen) = self.seen.get_mut(&snapshot.id) else {
                if let Some(phase) = &snapshot.phase {
                    let open: HashSet<u64> = snapshot.approvals.iter().copied().collect();
                    self.seen.insert(
                        snapshot.id.clone(),
                        Seen {
                            journals: snapshot.journals.clone(),
                            phase: Some(phase.clone()),
                            approvals: open.clone(),
                            open,
                        },
                    );
                }
                continue;
            };
            if seen.journals != snapshot.journals {
                // A new vendor process numbers its approvals from the start again.
                seen.journals.clone_from(&snapshot.journals);
                seen.approvals.clear();
            }
            let Some(phase) = &snapshot.phase else {
                // Without a state nothing is open; what was seen stays seen.
                seen.open.clear();
                continue;
            };
            seen.open = snapshot.approvals.iter().copied().collect();
            for &id in &snapshot.approvals {
                if seen.approvals.insert(id) {
                    notices.push(Notice::Approval {
                        session: snapshot.id.clone(),
                        id,
                    });
                }
            }
            if seen.phase.as_deref() == Some("running") && phase != "running" {
                notices.push(Notice::Turn {
                    session: snapshot.id.clone(),
                });
            }
            seen.phase = Some(phase.clone());
        }
        self.seen
            .retain(|id, _| snapshots.iter().any(|snapshot| &snapshot.id == id));
        notices
    }

    /// Adds `notices` to `device`'s window, which opens at `now` if none is open.
    pub(crate) fn offer(&mut self, device: &str, notices: &[Notice], now: Instant) {
        if notices.is_empty() {
            return;
        }
        let window = self.windows.entry(device.to_owned()).or_insert(Window {
            opened: now,
            approvals: Vec::new(),
            turns: Vec::new(),
        });
        for notice in notices {
            match notice {
                Notice::Approval { session, id } => window.approvals.push((session.clone(), *id)),
                Notice::Turn { session } => window.turns.push(session.clone()),
            }
        }
        let extra = window.approvals.len().saturating_sub(WINDOW_NOTICES);
        window.approvals.drain(..extra);
        let extra = window.turns.len().saturating_sub(WINDOW_NOTICES);
        window.turns.drain(..extra);
    }

    /// Forgets the devices whose last push is over a minute old, and held-back approvals that are
    /// no longer open.
    pub(crate) fn prune(&mut self, now: Instant) {
        let seen = &self.seen;
        self.held.retain(|_, held| {
            held.retain(|(session, id)| seen.get(session).is_some_and(|s| s.open.contains(id)));
            !held.is_empty()
        });
        self.sent.retain(|_, sent| {
            while sent
                .front()
                .is_some_and(|at| now.saturating_duration_since(*at) >= MINUTE)
            {
                sent.pop_front();
            }
            !sent.is_empty()
        });
    }

    /// `device` has no connection left: the approvals held back from it while it was connected
    /// that are still open are offered to it again, in a window opening at `now` (or the one
    /// already open), and so subject to [`CAP`] like any other.
    pub(crate) fn disconnected(&mut self, device: &str, now: Instant) {
        let Some(held) = self.held.remove(device) else {
            return;
        };
        let notices: Vec<Notice> = held
            .into_iter()
            .filter(|(session, id)| self.is_open(session, *id))
            .map(|(session, id)| Notice::Approval { session, id })
            .collect();
        self.offer(device, &notices, now);
    }

    /// Whether approval `id` is open in `session` now.
    fn is_open(&self, session: &str, id: u64) -> bool {
        self.seen
            .get(session)
            .is_some_and(|seen| seen.open.contains(&id))
    }

    /// How many devices have a push on record.
    #[cfg(test)]
    fn remembered(&self) -> usize {
        self.sent.len()
    }

    /// The devices with a window open.
    pub(crate) fn waiting(&self) -> Vec<String> {
        self.windows.keys().cloned().collect()
    }

    /// The push due to `device` at `now`, if its window has been open for [`WINDOW`]; the window
    /// then closes either way. Nothing is due to a device that `connected` (its user is looking
    /// at it: its open approvals are held for [`disconnected`](Self::disconnected)), for an
    /// approval already answered, or past [`CAP`] in the last minute.
    pub(crate) fn due_for(&mut self, device: &str, connected: bool, now: Instant) -> Option<Due> {
        if now.saturating_duration_since(self.windows.get(device)?.opened) < WINDOW {
            return None;
        }
        let window = self.windows.remove(device)?;
        if connected {
            let open: Vec<(String, u64)> = window
                .approvals
                .into_iter()
                .filter(|(session, id)| self.is_open(session, *id))
                .collect();
            if !open.is_empty() {
                let held = self.held.entry(device.to_owned()).or_default();
                for approval in open {
                    if !held.contains(&approval) {
                        held.push(approval);
                    }
                }
                let extra = held.len().saturating_sub(WINDOW_NOTICES);
                held.drain(..extra);
            }
            return None;
        }
        let still_open = |(session, id): &&(String, u64)| self.is_open(session, *id);
        let due = if let Some((session, _)) = window.approvals.iter().rev().find(still_open) {
            Due {
                kind: Kind::Approval,
                session: session.clone(),
            }
        } else {
            Due {
                kind: Kind::Turn,
                session: window.turns.last()?.clone(),
            }
        };
        let sent = self.sent.entry(device.to_owned()).or_default();
        while sent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= MINUTE)
        {
            sent.pop_front();
        }
        if sent.len() >= CAP {
            return None;
        }
        sent.push_back(now);
        Some(due)
    }
}

/// What Expo said about one message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Accepted.
    Sent,
    /// The token is no longer valid (`DeviceNotRegistered`): drop it.
    Dead,
    /// Refused, with Expo's error code.
    Refused(String),
    /// The post did not complete, or the answer was not understood.
    Failed,
}

/// Posts `messages` to `endpoint` with `program` (`curl`), and says how each fared, in order.
async fn send_with(program: &str, endpoint: &str, messages: &[Value]) -> Vec<Outcome> {
    let failed = || vec![Outcome::Failed; messages.len()];
    let Ok(body) = serde_json::to_vec(messages) else {
        return failed();
    };
    let post = async {
        // `-q` must come first: it stops curl reading `~/.curlrc`, which could add options (a
        // trace, a proxy, an output file) the hub never chose.
        let mut child = Command::new(program)
            .args(["-q", "-sS", "--max-time", "10"])
            .args(["-H", "Content-Type: application/json"])
            .args(["-H", "Accept: application/json"])
            .args(["--data-binary", "@-"])
            .arg(endpoint)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(&body).await.ok()?;
        drop(stdin);
        let output = child.wait_with_output().await.ok()?;
        output.status.success().then_some(output.stdout)
    };
    match timeout(SEND_WITHIN, post).await {
        Ok(Some(answer)) => tickets(&answer, messages.len()),
        _ => failed(),
    }
}

/// Reads Expo's answer to a batch of `count` messages.
fn tickets(body: &[u8], count: usize) -> Vec<Outcome> {
    let Ok(answer) = serde_json::from_slice::<Value>(body) else {
        return vec![Outcome::Failed; count];
    };
    if let Some(data) = answer.get("data").and_then(Value::as_array) {
        if data.len() != count {
            return vec![Outcome::Failed; count];
        }
        return data.iter().map(ticket).collect();
    }
    // A request-wide refusal comes as `{"errors": [{"code": …}]}`.
    match answer
        .get("errors")
        .and_then(|errors| errors.get(0)?.get("code")?.as_str())
    {
        Some(code) => vec![Outcome::Refused(printable(code)); count],
        None => vec![Outcome::Failed; count],
    }
}

/// One ticket.
fn ticket(ticket: &Value) -> Outcome {
    if ticket.get("status").and_then(Value::as_str) == Some("ok") {
        return Outcome::Sent;
    }
    match ticket
        .get("details")
        .and_then(|details| details.get("error")?.as_str())
    {
        Some("DeviceNotRegistered") => Outcome::Dead,
        Some(code) => Outcome::Refused(printable(code)),
        None if ticket.get("status").and_then(Value::as_str) == Some("error") => {
            Outcome::Refused("unknown".into())
        }
        None => Outcome::Failed,
    }
}

/// An error code that is safe to log: a short word of letters, digits, `_` and `-`.
fn printable(code: &str) -> String {
    let word = !code.is_empty()
        && code.chars().count() <= CODE_CHARS
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if word { code } else { "unknown" }.to_owned()
}

/// The log lines for `outcomes`: no token, session or body. A batch logs `Push failed` once, and
/// `Push refused: <code>` once for each distinct code.
fn log_lines(outcomes: &[Outcome]) -> Vec<String> {
    let mut lines = Vec::new();
    if outcomes.contains(&Outcome::Failed) {
        lines.push("Push failed".to_owned());
    }
    let mut seen = HashSet::new();
    for outcome in outcomes {
        if let Outcome::Refused(code) = outcome
            && seen.insert(code)
        {
            lines.push(format!("Push refused: {code}"));
        }
    }
    lines
}

/// Logs what went wrong in `outcomes`.
pub(crate) fn report(outcomes: &[Outcome]) {
    for line in log_lines(outcomes) {
        eprintln!("{line}");
    }
}

/// Runs sends in the background, [`SENDS_AT_ONCE`] at a time.
#[derive(Debug)]
pub(crate) struct Sender {
    program: String,
    endpoint: String,
    slots: Arc<Semaphore>,
    /// Told the tokens Expo says are dead.
    dead: mpsc::Sender<Vec<String>>,
}

impl Sender {
    /// A sender that posts to `endpoint` with `curl`.
    pub(crate) fn new(endpoint: String, dead: mpsc::Sender<Vec<String>>) -> Self {
        Self::with_program("curl", endpoint, dead)
    }

    /// Like [`new`](Self::new), running `program` as `curl`.
    fn with_program(program: &str, endpoint: String, dead: mpsc::Sender<Vec<String>>) -> Self {
        Self {
            program: program.to_owned(),
            endpoint,
            slots: Arc::new(Semaphore::new(SENDS_AT_ONCE)),
            dead,
        }
    }

    /// Posts the pushes `batch` (a token and what it is for) in a task of their own, and reports
    /// the dead tokens. If every slot is busy the batch is dropped, logged as `Push failed`, and
    /// this is false. Never waits.
    pub(crate) fn dispatch(&self, batch: Vec<(String, Due)>) -> bool {
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            report(&[Outcome::Failed]);
            return false;
        };
        let (program, endpoint, dead) = (
            self.program.clone(),
            self.endpoint.clone(),
            self.dead.clone(),
        );
        tokio::spawn(async move {
            let messages: Vec<Value> = batch
                .iter()
                .map(|(token, due)| due.message(token))
                .collect();
            let outcomes = send_with(&program, &endpoint, &messages).await;
            drop(slot);
            report(&outcomes);
            let gone: Vec<String> = batch
                .into_iter()
                .zip(outcomes)
                .filter(|(_, outcome)| *outcome == Outcome::Dead)
                .map(|((token, _), _)| token)
                .collect();
            if !gone.is_empty() {
                let _ = dead.send(gone).await;
            }
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, phase: Option<&str>, approvals: &[u64], journals: &[&str]) -> Snapshot {
        Snapshot {
            id: id.into(),
            journals: journals.iter().map(PathBuf::from).collect(),
            phase: phase.map(str::to_owned),
            approvals: approvals.to_vec(),
        }
    }

    fn approval(session: &str, id: u64) -> Notice {
        Notice::Approval {
            session: session.into(),
            id,
        }
    }

    fn turn(session: &str) -> Notice {
        Notice::Turn {
            session: session.into(),
        }
    }

    #[test]
    fn seeding_sends_nothing() {
        let mut pusher = Pusher::default();
        // A session that appears already running and with an approval open: a restart.
        let first = [snap("s1", Some("running"), &[1, 2], &["a"])];
        assert_eq!(pusher.observe(&first), []);
        assert_eq!(pusher.observe(&first), []);
        // Nor does a session without a state yet seed or push anything.
        let mut pusher = Pusher::default();
        assert_eq!(pusher.observe(&[snap("s1", None, &[], &["a"])]), []);
        assert_eq!(
            pusher.observe(&[snap("s1", Some("running"), &[3], &["a"])]),
            []
        );
        assert_eq!(
            pusher.observe(&[snap("s1", Some("running"), &[3, 4], &["a"])]),
            [approval("s1", 4)]
        );
    }

    #[test]
    fn a_new_approval_pushes_once_and_a_replay_does_not() {
        let mut pusher = Pusher::default();
        pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]);
        let open = [snap("s1", Some("idle"), &[1], &["a"])];
        assert_eq!(pusher.observe(&open), [approval("s1", 1)]);
        assert_eq!(pusher.observe(&open), []);
        // Closed, then replayed by a reconnecting session.
        assert_eq!(pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]), []);
        assert_eq!(pusher.observe(&open), []);
        // The link drops and comes back with the same approval.
        assert_eq!(pusher.observe(&[snap("s1", None, &[], &["a"])]), []);
        assert_eq!(pusher.observe(&open), []);
        // A different id is new.
        assert_eq!(
            pusher.observe(&[snap("s1", Some("idle"), &[2], &["a"])]),
            [approval("s1", 2)]
        );
    }

    #[test]
    fn a_reused_id_after_the_journals_change_pushes_again() {
        let mut pusher = Pusher::default();
        pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]);
        assert_eq!(
            pusher.observe(&[snap("s1", Some("idle"), &[1], &["a"])]),
            [approval("s1", 1)]
        );
        pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]);
        // A new vendor process: a new journal, and its approvals count from 1 again.
        assert_eq!(
            pusher.observe(&[snap("s1", Some("idle"), &[], &["a", "b"])]),
            []
        );
        assert_eq!(
            pusher.observe(&[snap("s1", Some("idle"), &[1], &["a", "b"])]),
            [approval("s1", 1)]
        );
        // And not again while the journals stay.
        assert_eq!(
            pusher.observe(&[snap("s1", Some("idle"), &[1], &["a", "b"])]),
            []
        );
    }

    #[test]
    fn running_to_idle_pushes_turn_finished_once() {
        let mut pusher = Pusher::default();
        let at = |phase| [snap("s1", Some(phase), &[], &["a"])];
        assert_eq!(pusher.observe(&at("running")), []);
        assert_eq!(pusher.observe(&at("running")), []);
        assert_eq!(pusher.observe(&at("idle")), [turn("s1")]);
        assert_eq!(pusher.observe(&at("idle")), []);
        // Idle to running is not a finish; running to stopped is.
        assert_eq!(pusher.observe(&at("running")), []);
        assert_eq!(pusher.observe(&at("stopped")), [turn("s1")]);
        // Connecting to idle is not a finish either.
        assert_eq!(pusher.observe(&at("connecting")), []);
        assert_eq!(pusher.observe(&at("idle")), []);
        // A session that goes away pushes nothing, and is forgotten.
        assert_eq!(pusher.observe(&at("running")), []);
        assert_eq!(pusher.observe(&[]), []);
        assert_eq!(pusher.observe(&at("idle")), []);
    }

    fn later(base: Instant, seconds: f64) -> Instant {
        base + Duration::from_secs_f64(seconds)
    }

    /// Opens approval `id` in session `session` (which is idle with its journal `a`).
    fn open(pusher: &mut Pusher, session: &str, id: u64) -> Vec<Notice> {
        pusher.observe(&[snap(session, Some("idle"), &[id], &["a"])])
    }

    fn seeded(sessions: &[&str]) -> Pusher {
        let mut pusher = Pusher::default();
        let rows: Vec<_> = sessions
            .iter()
            .map(|id| snap(id, Some("idle"), &[], &["a"]))
            .collect();
        pusher.observe(&rows);
        pusher
    }

    #[test]
    fn notices_collapse_in_a_window_and_an_approval_beats_a_turn() {
        let mut pusher = seeded(&["s1", "s2", "s3", "s4"]);
        let base = Instant::now();
        // Two approvals and two finished turns; the approvals are really open.
        pusher.observe(&[
            snap("s2", Some("idle"), &[1], &["a"]),
            snap("s3", Some("idle"), &[7], &["a"]),
        ]);
        let notices = [turn("s1"), approval("s2", 1), approval("s3", 7), turn("s4")];
        pusher.offer("dev", &notices, base);
        assert_eq!(pusher.waiting(), ["dev"]);
        // The window is still open.
        assert_eq!(pusher.due_for("dev", false, later(base, 1.9)), None);
        // One push: the approval, with the most recent session.
        assert_eq!(
            pusher.due_for("dev", false, later(base, 2.0)),
            Some(Due {
                kind: Kind::Approval,
                session: "s3".into()
            })
        );
        assert_eq!(pusher.waiting(), Vec::<String>::new());
        assert_eq!(pusher.due_for("dev", false, later(base, 9.0)), None);

        // Turns alone collapse to the most recent one.
        pusher.offer("dev", &[turn("s1"), turn("s4")], later(base, 10.0));
        assert_eq!(
            pusher.due_for("dev", false, later(base, 12.0)),
            Some(Due {
                kind: Kind::Turn,
                session: "s4".into()
            })
        );
    }

    #[test]
    fn an_approval_answered_inside_the_window_is_not_pushed() {
        let mut pusher = seeded(&["s1", "s2"]);
        let base = Instant::now();
        let notices = open(&mut pusher, "s1", 1);
        pusher.offer("dev", &notices, base);
        // The user answers it at the terminal a second later.
        pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]);
        assert_eq!(pusher.due_for("dev", false, later(base, 2.0)), None);
        // A turn that finished in the same window is still worth telling.
        let mut pusher = seeded(&["s1"]);
        let notices = open(&mut pusher, "s1", 1);
        pusher.offer("dev", &notices, base);
        pusher.offer("dev", &[turn("s1")], base);
        pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]);
        assert_eq!(
            pusher.due_for("dev", false, later(base, 2.0)),
            Some(Due {
                kind: Kind::Turn,
                session: "s1".into()
            })
        );
    }

    #[test]
    fn a_connected_device_gets_nothing_and_the_same_device_gone_gets_the_push() {
        let mut pusher = seeded(&["s1"]);
        let base = Instant::now();
        let first = open(&mut pusher, "s1", 1);
        pusher.offer("dev", &first, base);
        // Looking at the app when the window closes: nothing, and nothing later either.
        assert_eq!(pusher.due_for("dev", true, later(base, 2.0)), None);
        assert_eq!(pusher.due_for("dev", false, later(base, 30.0)), None);
        // The app goes to the background; the next approval reaches it.
        let second = open(&mut pusher, "s1", 2);
        pusher.offer("dev", &second, later(base, 40.0));
        assert_eq!(
            pusher.due_for("dev", false, later(base, 42.0)),
            Some(Due {
                kind: Kind::Approval,
                session: "s1".into()
            })
        );
        // Another device in the same window is judged on its own.
        let third = open(&mut pusher, "s1", 3);
        pusher.offer("a", &third, later(base, 50.0));
        pusher.offer("b", &third, later(base, 50.0));
        assert!(pusher.due_for("a", true, later(base, 52.0)).is_none());
        assert!(pusher.due_for("b", false, later(base, 52.0)).is_some());
    }

    #[test]
    fn an_approval_held_back_while_connected_goes_out_when_the_phone_leaves() {
        let mut pusher = seeded(&["s1"]);
        let base = Instant::now();
        let first = open(&mut pusher, "s1", 1);
        pusher.offer("dev", &first, base);
        assert_eq!(pusher.due_for("dev", true, later(base, 2.0)), None);
        // The app goes to the background: the approval still open is offered again, in a window.
        pusher.disconnected("dev", later(base, 10.0));
        assert_eq!(pusher.waiting(), ["dev"]);
        assert_eq!(pusher.due_for("dev", false, later(base, 11.0)), None);
        assert_eq!(
            pusher.due_for("dev", false, later(base, 12.0)),
            Some(Due {
                kind: Kind::Approval,
                session: "s1".into()
            })
        );
        // Pushed now, so a second goodbye offers nothing.
        pusher.disconnected("dev", later(base, 20.0));
        assert_eq!(pusher.waiting(), Vec::<String>::new());

        // Back within the window: held back again, and offered again at the next goodbye.
        let second = open(&mut pusher, "s1", 5);
        assert_eq!(second, [approval("s1", 5)]);
        pusher.offer("dev", &second, later(base, 30.0));
        assert_eq!(pusher.due_for("dev", true, later(base, 32.0)), None);
        pusher.disconnected("dev", later(base, 33.0));
        assert_eq!(pusher.due_for("dev", true, later(base, 35.0)), None);
        pusher.disconnected("dev", later(base, 36.0));
        assert!(pusher.due_for("dev", false, later(base, 38.0)).is_some());

        // An approval answered while the phone was open is not offered at all.
        let third = open(&mut pusher, "s1", 6);
        assert_eq!(third, [approval("s1", 6)]);
        pusher.offer("dev", &third, later(base, 40.0));
        assert_eq!(pusher.due_for("dev", true, later(base, 42.0)), None);
        pusher.observe(&[snap("s1", Some("idle"), &[], &["a"])]);
        pusher.disconnected("dev", later(base, 43.0));
        assert_eq!(pusher.waiting(), Vec::<String>::new());
        // Nor is anything offered to another device, or to one that was never held back.
        pusher.disconnected("other", later(base, 44.0));
        assert_eq!(pusher.waiting(), Vec::<String>::new());
    }

    #[test]
    fn what_is_offered_again_still_counts_against_the_cap() {
        let mut pusher = seeded(&["s1"]);
        let base = Instant::now();
        // Six pushes in the minute already.
        for n in 0..6_u64 {
            let at = 3.0 * f64::from(u32::try_from(n).expect("small"));
            let notices = open(&mut pusher, "s1", n);
            pusher.offer("dev", &notices, later(base, at));
            assert!(
                pusher
                    .due_for("dev", false, later(base, at + 2.0))
                    .is_some()
            );
        }
        let held = open(&mut pusher, "s1", 50);
        pusher.offer("dev", &held, later(base, 20.0));
        assert_eq!(pusher.due_for("dev", true, later(base, 22.0)), None);
        pusher.disconnected("dev", later(base, 23.0));
        assert_eq!(pusher.waiting(), ["dev"], "offered again");
        assert_eq!(pusher.due_for("dev", false, later(base, 25.0)), None);
    }

    #[test]
    fn push_is_capped_and_never_repeats() {
        let sessions: Vec<String> = (0..10).map(|n| format!("s{n}")).collect();
        let names: Vec<&str> = sessions.iter().map(String::as_str).collect();
        let mut pusher = seeded(&names);
        let base = Instant::now();
        let mut all = Vec::new();
        // Twenty approvals, 2.5 s apart, across ten sessions: every one has its own window.
        for n in 0..20_u64 {
            let at = 2.5 * f64::from(u32::try_from(n).expect("small"));
            let which = usize::try_from(n % 10).expect("small");
            let mut rows: Vec<_> = names
                .iter()
                .map(|id| snap(id, Some("idle"), &[], &["a"]))
                .collect();
            rows[which].approvals = vec![n];
            let notices = pusher.observe(&rows);
            assert_eq!(notices, [approval(names[which], n)]);
            pusher.offer("dev", &notices, later(base, at));
            // A replay of this state adds nothing.
            assert_eq!(pusher.observe(&rows), []);
            if let Some(due) = pusher.due_for("dev", false, later(base, at + 2.0)) {
                all.push(due);
            }
        }
        assert_eq!(all.len(), 6, "{all:?}");
        // A minute after the first, the allowance comes back.
        let notices = pusher.observe(&[snap("s0", Some("idle"), &[99], &["a"])]);
        pusher.offer("dev", &notices, later(base, 60.5));
        assert!(pusher.due_for("dev", false, later(base, 62.5)).is_some());
        // Each device has its own allowance.
        let notices = pusher.observe(&[snap("s0", Some("idle"), &[99, 100], &["a"])]);
        pusher.offer("other", &notices, later(base, 63.0));
        assert!(pusher.due_for("other", false, later(base, 65.0)).is_some());
    }

    #[test]
    fn the_message_is_content_free() {
        let token = "ExpoPushToken[abc]";
        let approval = Due {
            kind: Kind::Approval,
            session: "123-456".into(),
        };
        assert_eq!(
            approval.message(token),
            json!({"to": token, "title": "Octet", "body": "Approval waiting",
                "data": {"session": "123-456"}, "sound": "default", "priority": "high",
                "channelId": "approvals"})
        );
        let turn = Due {
            kind: Kind::Turn,
            session: "123-456".into(),
        };
        assert_eq!(
            turn.message(token),
            json!({"to": token, "title": "Octet", "body": "Turn finished",
                "data": {"session": "123-456"}, "priority": "normal", "channelId": "turns"})
        );
    }

    #[test]
    fn the_seam_is_loopback_only() {
        assert_eq!(endpoint_from(None), EXPO_URL);
        assert_eq!(endpoint_from(Some("")), EXPO_URL);
        assert_eq!(endpoint_from(Some("https://example.com/x")), EXPO_URL);
        assert_eq!(endpoint_from(Some("http://localhost:80/")), EXPO_URL);
        assert_eq!(endpoint_from(Some("http://127.0.0.1/")), EXPO_URL);
        // A userinfo trick names another host.
        assert_eq!(
            endpoint_from(Some("http://127.0.0.1:80@evil.test/")),
            EXPO_URL
        );
        assert_eq!(
            endpoint_from(Some("http://127.0.0.1:80 -o /tmp/x")),
            EXPO_URL
        );
        assert_eq!(endpoint_from(Some("http://127.0.0.1:8080?x")), EXPO_URL);
        assert_eq!(
            endpoint_from(Some("http://127.0.0.1:8080/send")),
            "http://127.0.0.1:8080/send"
        );
        assert_eq!(
            endpoint_from(Some("http://127.0.0.1:1")),
            "http://127.0.0.1:1"
        );
    }

    #[test]
    fn tickets_are_read_one_by_one() {
        let body = br#"{"data":[{"status":"ok","id":"x"},
            {"status":"error","message":"m","details":{"error":"DeviceNotRegistered"}},
            {"status":"error","details":{"error":"MessageRateExceeded"}},
            {"status":"error"}]}"#;
        assert_eq!(
            tickets(body, 4),
            [
                Outcome::Sent,
                Outcome::Dead,
                Outcome::Refused("MessageRateExceeded".into()),
                Outcome::Refused("unknown".into())
            ]
        );
        // The wrong number of tickets, or not JSON: all failed.
        assert_eq!(tickets(body, 2), [Outcome::Failed, Outcome::Failed]);
        assert_eq!(tickets(b"nope", 1), [Outcome::Failed]);
        assert_eq!(tickets(br#"{"data":{}}"#, 1), [Outcome::Failed]);
        // A request-wide error refuses each message.
        assert_eq!(
            tickets(
                br#"{"errors":[{"code":"PUSH_TOO_MANY_EXPERIENCE_IDS","message":"x"}]}"#,
                2
            ),
            [
                Outcome::Refused("PUSH_TOO_MANY_EXPERIENCE_IDS".into()),
                Outcome::Refused("PUSH_TOO_MANY_EXPERIENCE_IDS".into())
            ]
        );
        // A code that is not a plain word is not printed.
        assert_eq!(
            tickets(
                br#"{"data":[{"status":"error","details":{"error":"a b\nc"}}]}"#,
                1
            ),
            [Outcome::Refused("unknown".into())]
        );
    }

    #[tokio::test]
    async fn a_missing_curl_or_an_unreachable_endpoint_is_a_failure() {
        let messages = [json!({"to": "t"}), json!({"to": "u"})];
        assert_eq!(
            send_with("/nonexistent/curl", "http://127.0.0.1:1", &messages).await,
            [Outcome::Failed, Outcome::Failed]
        );
        // Nothing listens on port 1: curl exits non-zero.
        assert_eq!(
            send_with("curl", "http://127.0.0.1:1", &messages).await,
            [Outcome::Failed, Outcome::Failed]
        );
    }

    #[test]
    fn idle_devices_leave_nothing_behind() {
        let mut pusher = seeded(&["s1"]);
        let base = Instant::now();
        let notices = open(&mut pusher, "s1", 1);
        pusher.offer("dev", &notices, base);
        assert_eq!(pusher.waiting(), ["dev"]);
        assert!(pusher.due_for("dev", false, later(base, 2.0)).is_some());
        // The window closed with its push; the minute's record is all that is left.
        assert_eq!(pusher.waiting(), Vec::<String>::new());
        assert_eq!(pusher.remembered(), 1);
        pusher.prune(later(base, 59.0));
        assert_eq!(pusher.remembered(), 1);
        pusher.prune(later(base, 62.0));
        assert_eq!(pusher.remembered(), 0);
        // A window that closes without a push (connected) leaves no record either.
        let notices = pusher.observe(&[snap("s1", Some("idle"), &[1, 2], &["a"])]);
        pusher.offer("dev", &notices, later(base, 70.0));
        assert_eq!(pusher.due_for("dev", true, later(base, 72.0)), None);
        assert_eq!((pusher.waiting().len(), pusher.remembered()), (0, 0));
    }

    #[test]
    fn a_request_wide_refusal_is_logged_once_a_code() {
        let refused = |code: &str| Outcome::Refused(code.into());
        assert_eq!(
            log_lines(&[refused("A"), refused("A"), refused("B"), Outcome::Sent]),
            ["Push refused: A", "Push refused: B"]
        );
        assert_eq!(
            log_lines(&[Outcome::Failed, Outcome::Failed, refused("A")]),
            ["Push failed", "Push refused: A"]
        );
        assert_eq!(
            log_lines(&[Outcome::Sent, Outcome::Dead]),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn at_most_four_sends_run_and_the_rest_are_dropped() {
        // A "curl" that never answers in time.
        let dir = std::env::temp_dir().join(format!("ohpush{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let program = dir.join("slow");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 5\n").expect("script");
        std::fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");
        let (dead, _kept) = tokio::sync::mpsc::channel(1);
        let sender = Sender::with_program(
            program.to_str().expect("UTF-8"),
            "http://127.0.0.1:1".into(),
            dead,
        );
        let batch = || {
            vec![(
                "ExpoPushToken[x]".to_owned(),
                Due {
                    kind: Kind::Turn,
                    session: "s".into(),
                },
            )]
        };
        let accepted: Vec<bool> = (0..6).map(|_| sender.dispatch(batch())).collect();
        assert_eq!(accepted, [true, true, true, true, false, false]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
