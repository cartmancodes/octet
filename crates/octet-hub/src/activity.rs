//! The Live Activity while the phone is away: the hub starts it, keeps it current and ends it
//! through APNs ([`crate::apns`]), and nothing more.
//!
//! [`Driver`] is pure, like the pusher: it is fed what the sessions look like (the pusher's
//! [`Snapshot`]s), which devices connect, and the time, and says what is due to each device.
//!
//! - **What it shows** ([`Content`]): `approval` if any approval waits, else `running` if any
//!   session runs, else `done`. The session that leads is the one with the oldest waiting
//!   approval, else the most recently started running one, else the last that finished; `since`
//!   is when that session's own kind began, and `approvals` counts every approval waiting.
//! - **Connected** means here that a connection of the device streams a session: it sent at least
//!   one `subscribe`, which only the app in the foreground does. A bare `hello`, or a background
//!   wake that only sends `register_activity` (handing over a hub-started activity's token), is
//!   not connected: the activity goes on, and its updates go to the token that wake registered.
//! - **Start**, to the device's `start_token`: on news (an approval new to its session, or a turn
//!   that starts) while the device is not connected and has no activity the hub knows of. News
//!   seen while it is connected is spent, so leaving the app starts nothing by itself. A
//!   session's first state only seeds, so a hub restart starts nothing either.
//! - **Update**, to the device's activity `token`: when the kind, the approval count or the
//!   leading session changes, at most once per [`UPDATE_GAP`]. A token the device already held
//!   when the hub started the activity belongs to an earlier one, so it is never sent anything
//!   for this one: no update, no end, nothing from the cap. Without an activity token the hub cannot update; it only
//!   starts and ends.
//! - **End**, to the activity token, with `dismissal-date`: [`IDLE_END`] after every session went
//!   idle, or when a connection of the device starts streaming, after which the app drives the
//!   activity itself.
//!
//! Every send also takes one from the device's allowance in the [`Limiter`] the pushes use, so the
//! two together stay within its cap, and goes only while one more is left after it, so an approval
//! push that follows still fits. One the allowance has no room for waits for the next look, as
//! does one with no free send slot. Nothing retries a send that went out, but an end APNs failed
//! (other than with a 410) is sent again, a few times at most.
//!
//! The payload ([`Due::payload`]) names no session name, workspace, command or file: only the
//! kind, the hub's raw session id, the host the hub is published at (`mac`), two times and a
//! count.
//!
//! [`Activity`] runs it: it holds the APNs key, sends in the background with `curl`
//! ([`crate::apns::send`]), and reports a 410 or a start that did not go out to the hub. The only
//! log line is [`SEND_FAILED`].
use crate::{
    apns::{self, ApnsConfig, Outcome, Priority, Signer},
    push::{Limiter, Snapshot},
    store::{ActivityTokens, Device},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    time::Instant,
};

/// The least time between two updates to one device.
pub(crate) const UPDATE_GAP: Duration = Duration::from_secs(2);
/// How long every session must have been idle before the hub ends the activity.
pub(crate) const IDLE_END: Duration = Duration::from_mins(5);
/// Sends a device's minute keeps for its pushes: a Live Activity send goes only while at least
/// one more is left after it, so an approval push that follows still fits.
const KEEP_FOR_PUSHES: usize = 1;
/// How many times one end is sent, at most, while APNs fails it (a 410 is never tried again).
const END_TRIES: u8 = 3;
/// The one log line: a send that failed, naming no token, JWT or host.
pub(crate) const SEND_FAILED: &str = "Live Activity push failed";
/// Sends that may run at once; one that finds them all busy is not sent.
const SENDS_AT_ONCE: usize = 4;
/// What sends: `curl`, found on `PATH` as the pushes' is.
const CURL: &str = "curl";
/// The phase of a session in a turn.
const RUNNING: &str = "running";

/// What the activity shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A session runs a turn.
    Running,
    /// An approval waits.
    Approval,
    /// Nothing runs and nothing waits.
    Done,
}

impl Kind {
    /// Its name in the content state.
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Approval => "approval",
            Self::Done => "done",
        }
    }
}

/// What a send does to the activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Starts one, with the push-to-start token.
    Start,
    /// Changes what the running one shows.
    Update,
    /// Ends it.
    End,
}

impl Event {
    /// Its name in the payload.
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Update => "update",
            Self::End => "end",
        }
    }
}

/// What the activity shows: the content state, with `since` as an instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Content {
    pub kind: Kind,
    /// The hub's raw id of the session that leads, or empty when none does.
    pub session: String,
    /// When the leading session's kind began.
    pub since: Instant,
    /// The approvals waiting, in every session.
    pub approvals: usize,
}

/// One send due to a device.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Due {
    pub event: Event,
    /// Which of the device's activities it is for: each start begins a new one.
    pub run: u64,
    /// The `ActivityKit` token it goes to.
    pub token: String,
    pub content: Content,
}

/// Shows everything but the token.
impl fmt::Debug for Due {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Due")
            .field("event", &self.event)
            .field("run", &self.run)
            .field("token", &"<redacted>")
            .field("content", &self.content)
            .finish()
    }
}

impl Due {
    /// How soon APNs should deliver it: at once for a start and for an update while an approval
    /// waits; an end, and any other update, when it suits the phone.
    pub(crate) fn priority(&self) -> Priority {
        let urgent = match self.event {
            Event::Start => true,
            Event::Update => self.content.kind == Kind::Approval,
            Event::End => false,
        };
        if urgent {
            Priority::Immediate
        } else {
            Priority::Normal
        }
    }

    /// The APNs payload at `now` (whose wall-clock time is `wall`), naming `mac` (the host the hub
    /// is published at, or nothing): the event and the content state; a start adds the
    /// attributes and an alert ("Approval waiting" for an approval, else "Session running"), and
    /// an end a `dismissal-date`. Nothing else.
    pub(crate) fn payload(&self, mac: Option<&str>, now: Instant, wall: SystemTime) -> Value {
        let timestamp = wall.duration_since(UNIX_EPOCH).map_or(0, |at| at.as_secs());
        let age = now.saturating_duration_since(self.content.since).as_secs();
        let mut aps = json!({
            "timestamp": timestamp,
            "event": self.event.label(),
            "content-state": {
                "kind": self.content.kind.label(),
                "session": self.content.session,
                "mac": mac.unwrap_or_default(),
                "since": timestamp.saturating_sub(age),
                "approvals": self.content.approvals,
            },
        });
        match self.event {
            Event::Start => {
                // ActivityKit makes the activity from the attributes, and wants an alert to start
                // one by push. For an approval that alert comes on top of the push notification
                // the pusher sends for it: two alerts for one approval, which is accepted.
                let body = if self.content.kind == Kind::Approval {
                    "Approval waiting"
                } else {
                    "Session running"
                };
                aps["attributes-type"] = json!("OctetActivity");
                aps["attributes"] = json!({});
                aps["alert"] = json!({"title": "Octet", "body": body});
            }
            Event::End => aps["dismissal-date"] = json!(timestamp),
            Event::Update => {}
        }
        json!({ "aps": aps })
    }
}

/// What the driver remembers of one session.
struct Session {
    journals: Vec<PathBuf>,
    /// The last phase its state named; kept while it has no state.
    phase: String,
    /// Whether it has a state now.
    linked: bool,
    /// Every approval id seen since the journals last changed.
    seen: HashSet<u64>,
    /// The approvals open now, with when each was first seen open.
    open: Vec<(u64, Instant)>,
    /// What it shows on its own, and since when.
    kind: Kind,
    since: Instant,
    /// When it last went from running or waiting to done.
    finished: Option<Instant>,
}

impl Session {
    /// What this session shows on its own: an approval if one waits, running while it has a
    /// state that says so, else done.
    fn own_kind(&self) -> Kind {
        if !self.open.is_empty() {
            Kind::Approval
        } else if self.linked && self.phase == RUNNING {
            Kind::Running
        } else {
            Kind::Done
        }
    }

    /// When its oldest open approval was first seen.
    fn oldest(&self) -> Option<Instant> {
        self.open.iter().map(|(_, at)| *at).min()
    }
}

/// What the driver remembers of one device. It holds a token, so it has no `Debug`.
struct Phone {
    /// Whether an activity the hub started since the device last connected may still run.
    live: bool,
    /// The kind, the count and the leading session that activity was last sent.
    told: Option<(Kind, usize, String)>,
    /// When the hub last sent to it.
    last: Option<Instant>,
    /// The driver's news this device was last judged on.
    news: u64,
    /// Whether an end waits to go out.
    end: bool,
    /// The activity the hub last started for it ([`Due::run`]); 0 for none.
    run: u64,
    /// The activity token the device held when that activity started, which belongs to an
    /// earlier one: a 410 on it says nothing of this one.
    stale: Option<String>,
    /// How many times that activity's end was sent.
    ends: u8,
}

impl Phone {
    /// A device the driver has not judged before, judged on `news` as it stands.
    fn new(news: u64) -> Self {
        Self {
            live: false,
            told: None,
            last: None,
            news,
            end: false,
            run: 0,
            stale: None,
            ends: 0,
        }
    }

    /// The activity is to be ended: no more updates, and an end to send.
    fn ending(&mut self) {
        if self.live {
            self.end = true;
        }
        self.live = false;
        self.told = None;
    }
}

/// Decides what each device's Live Activity is sent, and when.
#[derive(Default)]
pub(crate) struct Driver {
    /// By session id.
    sessions: HashMap<String, Session>,
    /// Bumped for each look that found news: a new approval or a turn that starts.
    news: u64,
    /// Bumped for each start, so a report can say which activity it was about.
    runs: u64,
    /// When every session last went idle; none while one waits or runs, or before the first look.
    idle_since: Option<Instant>,
    /// By device key.
    phones: HashMap<String, Phone>,
}

// Device keys identify devices in `devices.json`, and session ids are the hub's: counts only.
impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("sessions", &self.sessions.len())
            .field("news", &self.news)
            .field("phones", &self.phones.len())
            .finish_non_exhaustive()
    }
}

impl Driver {
    /// Takes the sessions as they stand at `now`. A session seen for the first time with a state
    /// only seeds; one that is gone is forgotten.
    pub(crate) fn observe(&mut self, snapshots: &[Snapshot], now: Instant) {
        let mut news = false;
        for snapshot in snapshots {
            let Some(session) = self.sessions.get_mut(&snapshot.id) else {
                if let Some(phase) = &snapshot.phase {
                    let mut session = Session {
                        journals: snapshot.journals.clone(),
                        phase: phase.clone(),
                        linked: true,
                        seen: snapshot.approvals.iter().copied().collect(),
                        open: snapshot.approvals.iter().map(|id| (*id, now)).collect(),
                        kind: Kind::Done,
                        since: now,
                        finished: None,
                    };
                    session.kind = session.own_kind();
                    self.sessions.insert(snapshot.id.clone(), session);
                }
                continue;
            };
            if session.journals != snapshot.journals {
                // A new vendor process numbers its approvals from the start again.
                session.journals.clone_from(&snapshot.journals);
                session.seen.clear();
            }
            match &snapshot.phase {
                None => {
                    // Without a state nothing is open and nothing runs; the phase is kept, so a
                    // link back mid-turn is no turn starting.
                    session.linked = false;
                    session.open.clear();
                }
                Some(phase) => {
                    session
                        .open
                        .retain(|(id, _)| snapshot.approvals.contains(id));
                    for &id in &snapshot.approvals {
                        if !session.open.iter().any(|(open, _)| *open == id) {
                            session.open.push((id, now));
                        }
                        news |= session.seen.insert(id);
                    }
                    news |= session.phase != RUNNING && phase == RUNNING;
                    session.phase.clone_from(phase);
                    session.linked = true;
                }
            }
            let kind = session.own_kind();
            if kind != session.kind {
                if kind == Kind::Done {
                    session.finished = Some(now);
                }
                session.kind = kind;
                session.since = now;
            }
        }
        self.sessions
            .retain(|id, _| snapshots.iter().any(|snapshot| &snapshot.id == id));
        if news {
            self.news += 1;
        }
        if self.content(now).kind == Kind::Done {
            self.idle_since.get_or_insert(now);
        } else {
            self.idle_since = None;
        }
    }

    /// What the activity shows at `now`.
    fn content(&self, now: Instant) -> Content {
        let approvals = self.sessions.values().map(|s| s.open.len()).sum();
        let lead = |kind: Kind| self.sessions.iter().filter(move |(_, s)| s.kind == kind);
        let chosen = lead(Kind::Approval)
            .min_by_key(|(id, s)| (s.oldest(), *id))
            .or_else(|| lead(Kind::Running).max_by_key(|(id, s)| (s.since, *id)))
            .or_else(|| {
                lead(Kind::Done)
                    .filter(|(_, s)| s.finished.is_some())
                    .max_by_key(|(id, s)| (s.finished, *id))
            });
        match chosen {
            Some((id, session)) => Content {
                kind: session.kind,
                session: id.clone(),
                since: session.since,
                approvals,
            },
            None => Content {
                kind: Kind::Done,
                session: String::new(),
                since: self.idle_since.unwrap_or(now),
                approvals,
            },
        }
    }

    /// `device` said hello: from now on it is judged on news from now, even if the hub has not
    /// looked at it yet when that news comes. Nothing else changes: a hello is not a connection
    /// that streams (see [`connected`](Self::connected)).
    pub(crate) fn hello(&mut self, device: &str) {
        let news = self.news;
        self.phones
            .entry(device.to_owned())
            .or_insert_with(|| Phone::new(news));
    }

    /// A connection of `device` started streaming a session (the app is in the foreground): the
    /// activity the hub started for it, if any, is to end, and the news so far is spent. From now
    /// on the app drives it.
    pub(crate) fn connected(&mut self, device: &str) {
        let news = self.news;
        let phone = self
            .phones
            .entry(device.to_owned())
            .or_insert_with(|| Phone::new(news));
        phone.ending();
        phone.news = news;
    }

    /// What APNs said to a send that did not go well, given the activity token the device holds
    /// now (`stored`). Only a report about the activity the hub drives now counts:
    ///
    /// - a start that did not go out leaves the device with none, so the next news starts one;
    /// - a 410 on an update forgets the activity only when the token is the one the device holds
    ///   now and was registered after the start (one since replaced says nothing of it); either
    ///   way the next token is told the state again, changed or not;
    /// - an end that failed, other than with a 410, is sent again at a later look, within the
    ///   cap, [`END_TRIES`] times in all.
    pub(crate) fn failed(&mut self, report: &Report, stored: Option<&str>) {
        let Some(phone) = self.phones.get_mut(&report.device) else {
            return;
        };
        if phone.run != report.run {
            return;
        }
        let token = report.token.as_str();
        match (report.event, report.outcome) {
            (_, Outcome::Sent)
            | (Event::Update, Outcome::Failed(_))
            | (Event::End, Outcome::Gone) => {}
            (Event::Start, _) => {
                phone.live = false;
                phone.told = None;
            }
            (Event::Update, Outcome::Gone) => {
                if stored == Some(token) && phone.stale.as_deref() != Some(token) {
                    phone.live = false;
                }
                // Either way that update did not land: whatever token comes next is told the
                // state as it stands, changed or not.
                phone.told = None;
            }
            (Event::End, Outcome::Failed(_)) => {
                if !phone.live && phone.ends < END_TRIES {
                    phone.end = true;
                }
            }
        }
    }

    /// The device has no tokens (`clear_activity`): an activity the hub was driving, or an end
    /// waiting, is forgotten, so tokens registered again start afresh. The hub calls it as the
    /// clear is saved, and a look at a device without tokens does too.
    pub(crate) fn cleared(&mut self, device: &str) {
        let news = self.news;
        if let Some(phone) = self.phones.get_mut(device)
            && (phone.live || phone.end)
        {
            *phone = Phone::new(news);
        }
    }

    /// Forgets the devices `keep` does not name.
    pub(crate) fn prune(&mut self, keep: impl Fn(&str) -> bool) {
        self.phones.retain(|device, _| keep(device));
    }

    /// The send due to `device` at `now`, given its tokens and whether it is `connected`, if
    /// there is one and `cap` has room for it with [`KEEP_FOR_PUSHES`] left (it then takes one).
    pub(crate) fn due_for(
        &mut self,
        device: &str,
        tokens: Option<&ActivityTokens>,
        connected: bool,
        cap: &mut Limiter,
        now: Instant,
    ) -> Option<Due> {
        let Some(tokens) = tokens else {
            self.cleared(device);
            return None;
        };
        let content = self.content(now);
        let idle_long = content.kind == Kind::Done
            && self
                .idle_since
                .is_some_and(|at| now.saturating_duration_since(at) >= IDLE_END);
        let news = self.news;
        let phone = self
            .phones
            .entry(device.to_owned())
            .or_insert_with(|| Phone::new(news));
        let token = tokens.token.clone();
        if phone.live && idle_long {
            phone.ending();
        }
        // The token the device held at the start is an earlier activity's: nothing for this one
        // goes there.
        let token = token.filter(|token| phone.stale.as_deref() != Some(token));
        if phone.end {
            // Without an activity token of its own there is nothing to end it with.
            let Some(token) = token else {
                phone.end = false;
                return None;
            };
            if !cap.allow_keeping(device, now, KEEP_FOR_PUSHES) {
                return None;
            }
            phone.end = false;
            phone.ends += 1;
            phone.last = Some(now);
            return Some(Due {
                run: phone.run,
                event: Event::End,
                token,
                content,
            });
        }
        if connected {
            phone.news = news;
            return None;
        }
        let shown = (content.kind, content.approvals, content.session.clone());
        if phone.live {
            phone.news = news;
            if phone.told.as_ref() == Some(&shown) {
                return None;
            }
            let token = token?;
            let soon = phone
                .last
                .is_some_and(|at| now.saturating_duration_since(at) < UPDATE_GAP);
            if soon || !cap.allow_keeping(device, now, KEEP_FOR_PUSHES) {
                return None;
            }
            phone.told = Some(shown);
            phone.last = Some(now);
            return Some(Due {
                run: phone.run,
                event: Event::Update,
                token,
                content,
            });
        }
        if phone.news == news {
            return None;
        }
        let start = tokens.start_token.clone();
        let Some(start) = start.filter(|_| content.kind != Kind::Done) else {
            // Nothing to show, or nothing to start it with: this news is spent.
            phone.news = news;
            return None;
        };
        if !cap.allow_keeping(device, now, KEEP_FOR_PUSHES) {
            return None;
        }
        self.runs += 1;
        *phone = Phone {
            live: true,
            told: Some(shown),
            last: Some(now),
            news,
            end: false,
            run: self.runs,
            stale: token,
            ends: 0,
        };
        Some(Due {
            run: self.runs,
            event: Event::Start,
            token: start,
            content,
        })
    }
}

/// A send that did not go well, for the hub: a 410 drops `token`, and a start that did not go out
/// leaves the device with no activity.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Report {
    /// The device's key.
    pub device: String,
    /// The token it went to.
    pub token: String,
    pub event: Event,
    /// Which activity it was for ([`Due::run`]).
    pub run: u64,
    pub outcome: Outcome,
}

/// Shows neither the device's key nor the token.
impl fmt::Debug for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Report")
            .field("event", &self.event)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// The Live Activity driver with what it sends with: on only when `hub.json` names APNs and its
/// key loads.
pub(crate) struct Activity {
    driver: Driver,
    signer: Signer,
    config: ApnsConfig,
    program: String,
    slots: Arc<Semaphore>,
    /// Told the sends that did not go well.
    reports: mpsc::Sender<Report>,
}

// The signer prints redacted; the rest is counts and settings.
impl fmt::Debug for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Activity")
            .field("driver", &self.driver)
            .field("signer", &self.signer)
            .field("sandbox", &self.config.sandbox)
            .finish_non_exhaustive()
    }
}

impl Activity {
    /// Sends with `curl` to the host `config` names, signed by `signer`, telling `reports` of
    /// sends that did not go well.
    pub(crate) fn new(config: ApnsConfig, signer: Signer, reports: mpsc::Sender<Report>) -> Self {
        Self {
            driver: Driver::default(),
            signer,
            config,
            program: CURL.to_owned(),
            slots: Arc::new(Semaphore::new(SENDS_AT_ONCE)),
            reports,
        }
    }

    /// See [`Driver::observe`].
    pub(crate) fn observe(&mut self, snapshots: &[Snapshot], now: Instant) {
        self.driver.observe(snapshots, now);
    }

    /// See [`Driver::hello`].
    pub(crate) fn hello(&mut self, device: &str) {
        self.driver.hello(device);
    }

    /// See [`Driver::cleared`].
    pub(crate) fn cleared(&mut self, device: &str) {
        self.driver.cleared(device);
    }

    /// See [`Driver::connected`].
    pub(crate) fn connected(&mut self, device: &str) {
        self.driver.connected(device);
    }

    /// Takes what a send came to, given the activity token the device holds now (see
    /// [`Driver::failed`]). Says whether the token is to be dropped (a 410).
    pub(crate) fn reported(&mut self, report: &Report, stored: Option<&str>) -> bool {
        self.driver.failed(report, stored);
        report.outcome == Outcome::Gone
    }

    /// Sends, in the background, what is due at `now` (wall-clock `wall`) to each of `devices`
    /// with activity tokens, naming `mac`; `connected` says which devices have a connection that
    /// streams a session, and `cap` is the allowance the pushes share.
    pub(crate) fn flush(
        &mut self,
        devices: &[Device],
        connected: impl Fn(&str) -> bool,
        cap: &mut Limiter,
        mac: Option<&str>,
        (now, wall): (Instant, SystemTime),
    ) {
        self.driver
            .prune(|key| devices.iter().any(|device| device.token_sha256 == key));
        for device in devices {
            let key = &device.token_sha256;
            let tokens = device.activity.as_ref();
            if tokens.is_none() {
                // Forgets what it drove for a device that turned the Live Activity off.
                self.driver.due_for(key, None, false, cap, now);
                continue;
            }
            // A slot first: a send there is no room to run takes nothing from the cap, and is
            // judged again at the next look.
            let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
                continue;
            };
            let Some(due) = self.driver.due_for(key, tokens, connected(key), cap, now) else {
                continue;
            };
            let payload = due.payload(mac, now, wall);
            let jwt = self.signer.jwt(wall).to_owned();
            self.dispatch(slot, key.clone(), due, payload, jwt);
        }
    }

    /// Sends `payload` for `due` to `device` in a task of its own, holding `slot` until it is
    /// done, and reports it if it did not go well. Never waits.
    fn dispatch(
        &self,
        slot: OwnedSemaphorePermit,
        device: String,
        due: Due,
        payload: Value,
        jwt: String,
    ) {
        let (config, reports, program) = (
            self.config.clone(),
            self.reports.clone(),
            self.program.clone(),
        );
        tokio::spawn(async move {
            let priority = due.priority();
            let outcome = apns::send(&program, &config, &jwt, &due.token, &payload, priority).await;
            drop(slot);
            if outcome == Outcome::Sent {
                return;
            }
            if matches!(outcome, Outcome::Failed(_)) {
                eprintln!("{SEND_FAILED}");
            }
            let report = Report {
                device,
                token: due.token,
                event: due.event,
                run: due.run,
                outcome,
            };
            let _ = reports.send(report).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::CAP;
    use tokio::time::timeout;

    /// The activity token and the push-to-start token of the device under test.
    const TOKEN: &str = "a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0";
    const START: &str = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
    /// The host the tests' hub is published at.
    const MAC: &str = "mac.tail1234.ts.net";
    /// Where the tests' sessions keep their journals: a name the payload must never carry.
    const JOURNAL: &str = "/work/secret-project/session-1-1.jsonl";

    fn snap(id: &str, phase: Option<&str>, approvals: &[u64]) -> Snapshot {
        Snapshot {
            id: id.into(),
            journals: vec![PathBuf::from(JOURNAL)],
            phase: phase.map(str::to_owned),
            approvals: approvals.to_vec(),
            errors: 0,
        }
    }

    fn idle(id: &str) -> Snapshot {
        snap(id, Some("idle"), &[])
    }

    fn tokens(token: Option<&str>, start_token: Option<&str>) -> ActivityTokens {
        ActivityTokens {
            token: token.map(str::to_owned),
            start_token: start_token.map(str::to_owned),
        }
    }

    fn later(base: Instant, seconds: f64) -> Instant {
        base + Duration::from_secs_f64(seconds)
    }

    /// A driver, an allowance and a device that holds both tokens, all seeded at `base` with
    /// `sessions` idle.
    struct Rig {
        driver: Driver,
        cap: Limiter,
        tokens: ActivityTokens,
        base: Instant,
        /// Whether the app wakes after each start to hand over the new activity's token: the
        /// first is `TOKEN`, the next `TOKEN2`, and so on.
        wakes: bool,
        /// The starts so far.
        starts: usize,
    }

    /// The token the app hands over after the hub's `n`th start (from 0).
    fn woken(n: usize) -> String {
        if n == 0 {
            TOKEN.to_owned()
        } else {
            format!("{:x}", n % 16).repeat(32)
        }
    }

    impl Rig {
        /// A device with only a push-to-start token, whose app hands over each new activity's
        /// token after the start.
        fn new(sessions: &[&str]) -> Self {
            let mut rig = Self::with(sessions, tokens(None, Some(START)));
            rig.wakes = true;
            rig
        }

        /// A device holding `tokens`, which change only when the test says.
        fn with(sessions: &[&str], tokens: ActivityTokens) -> Self {
            let base = Instant::now();
            let mut rig = Self {
                driver: Driver::default(),
                cap: Limiter::default(),
                tokens,
                base,
                wakes: false,
                starts: 0,
            };
            let rows: Vec<Snapshot> = sessions.iter().map(|id| idle(id)).collect();
            rig.driver.observe(&rows, base);
            // The device is judged once at the seed, as the hub does every second.
            assert_eq!(rig.away(0.0), None);
            rig
        }

        fn at(&self, seconds: f64) -> Instant {
            later(self.base, seconds)
        }

        fn observe(&mut self, seconds: f64, rows: &[Snapshot]) {
            let now = self.at(seconds);
            self.driver.observe(rows, now);
        }

        /// What is due to the device at `seconds`, with no phone connected.
        fn away(&mut self, seconds: f64) -> Option<Due> {
            self.due(seconds, false)
        }

        fn due(&mut self, seconds: f64, connected: bool) -> Option<Due> {
            let now = self.at(seconds);
            let tokens = self.tokens.clone();
            let due = self
                .driver
                .due_for("dev", Some(&tokens), connected, &mut self.cap, now);
            if self.wakes && due.as_ref().is_some_and(|due| due.event == Event::Start) {
                self.tokens.token = Some(woken(self.starts));
                self.starts += 1;
            }
            due
        }

        /// The sends the device's minute holds.
        fn sends(&self, seconds: f64) -> usize {
            let now = self.at(seconds);
            self.cap.sends("dev", now)
        }
    }

    /// The event, the token, the kind, the leading session and the approval count of `due`.
    fn sent(due: Option<Due>) -> Option<(Event, String, Kind, String, usize)> {
        due.map(|due| {
            (
                due.event,
                due.token,
                due.content.kind,
                due.content.session,
                due.content.approvals,
            )
        })
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "it is compared with what `sent` gives, which is an option"
    )]
    fn expect(
        event: Event,
        token: &str,
        kind: Kind,
        session: &str,
        approvals: usize,
    ) -> Option<(Event, String, Kind, String, usize)> {
        Some((event, token.into(), kind, session.into(), approvals))
    }

    #[test]
    fn starts_on_an_approval_or_a_turn_while_away_and_nothing_while_connected() {
        // An approval while the phone is away: a start, to the push-to-start token, at once.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        let due = rig.away(1.0).expect("a start");
        assert_eq!(due.priority(), Priority::Immediate);
        assert_eq!(
            sent(Some(due)),
            expect(Event::Start, START, Kind::Approval, "s1", 1)
        );
        // Once.
        assert_eq!(rig.away(1.5), None);
        assert_eq!(rig.away(9.0), None);

        // A turn that starts while away starts one too, at once though it is not an approval.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("running"), &[])]);
        let due = rig.away(1.0).expect("a start");
        assert_eq!(due.priority(), Priority::Immediate);
        assert_eq!(
            sent(Some(due)),
            expect(Event::Start, START, Kind::Running, "s1", 0)
        );

        // While connected: nothing, and nothing when the phone then leaves, for that was no news.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.due(1.0, true), None);
        assert_eq!(rig.away(5.0), None);
        rig.observe(6.0, &[snap("s1", Some("running"), &[1])]);
        assert_eq!(rig.due(6.0, true), None);
        assert_eq!(rig.away(9.0), None);
        // The next news while away starts one.
        rig.observe(10.0, &[snap("s1", Some("running"), &[1, 2])]);
        assert_eq!(
            sent(rig.away(10.0)),
            expect(Event::Start, START, Kind::Approval, "s1", 2)
        );

        // A session first seen already running with approvals open (a hub restart) only seeds.
        let mut rig = Rig::new(&[]);
        rig.observe(1.0, &[snap("s1", Some("running"), &[1, 2])]);
        assert_eq!(rig.away(1.0), None);
        // A replay of an approval already seen, after the link dropped, is no news either.
        rig.observe(2.0, &[snap("s1", None, &[])]);
        rig.observe(3.0, &[snap("s1", Some("running"), &[1, 2])]);
        assert_eq!(rig.away(3.0), None);
        // Nor is a turn that finishes, or news that is over by the time the device is judged.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("running"), &[])]);
        rig.observe(1.2, &[idle("s1")]);
        assert_eq!(rig.away(1.5), None);
        assert_eq!(rig.away(5.0), None);
    }

    #[test]
    fn updates_on_a_kind_or_count_change() {
        let mut rig = Rig::new(&["s1", "s2"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1]), idle("s2")]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        // A second approval: an update, to the activity token, once 2 s have passed.
        rig.observe(2.0, &[snap("s1", Some("idle"), &[1, 2]), idle("s2")]);
        assert_eq!(rig.away(2.0), None);
        assert_eq!(rig.away(2.9), None);
        let due = rig.away(3.0).expect("an update");
        assert_eq!(due.priority(), Priority::Immediate);
        assert_eq!(
            sent(Some(due)),
            expect(Event::Update, TOKEN, Kind::Approval, "s1", 2)
        );
        // The same again is nothing.
        rig.observe(4.0, &[snap("s1", Some("idle"), &[1, 2]), idle("s2")]);
        assert_eq!(rig.away(6.0), None);
        // Both answered and a turn runs: the kind changed, an update at the normal priority.
        rig.observe(7.0, &[snap("s1", Some("running"), &[]), idle("s2")]);
        let due = rig.away(7.0).expect("an update");
        assert_eq!(due.priority(), Priority::Normal);
        assert_eq!(
            sent(Some(due)),
            expect(Event::Update, TOKEN, Kind::Running, "s1", 0)
        );
        // Another session's turn starting changes neither the kind nor the count, but the lead:
        // an update that names it.
        rig.observe(
            10.0,
            &[
                snap("s1", Some("running"), &[]),
                snap("s2", Some("running"), &[]),
            ],
        );
        assert_eq!(
            sent(rig.away(12.0)),
            expect(Event::Update, TOKEN, Kind::Running, "s2", 0)
        );
        // All done: an update to done, 2 s after the last.
        rig.observe(13.0, &[idle("s1"), idle("s2")]);
        assert_eq!(rig.away(13.0), None);
        assert_eq!(
            sent(rig.away(14.0)),
            expect(Event::Update, TOKEN, Kind::Done, "s2", 0)
        );
        // A change while connected is not sent.
        rig.observe(20.0, &[snap("s1", Some("idle"), &[3]), idle("s2")]);
        assert_eq!(rig.due(20.0, true).map(|due| due.event), None);

        // Without an activity token the hub cannot update: it started, and that is all.
        let mut rig = Rig::with(&["s1"], tokens(None, Some(START)));
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(
            sent(rig.away(1.0)),
            expect(Event::Start, START, Kind::Approval, "s1", 1)
        );
        rig.observe(2.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(rig.away(5.0), None);
        rig.observe(6.0, &[idle("s1")]);
        assert_eq!(rig.away(9.0), None);
        // With one registered later, the change it missed goes out.
        rig.tokens = tokens(Some(TOKEN), Some(START));
        assert_eq!(
            sent(rig.away(10.0)),
            expect(Event::Update, TOKEN, Kind::Done, "s1", 0)
        );
    }

    #[test]
    fn activity_updates_are_bounded() {
        // s2 runs a turn throughout; s1 takes 20 approvals in 10 s. The device is judged every
        // quarter second (the hub looks every second), and has had two pushes in the minute.
        let mut rig = Rig::new(&["s1", "s2"]);
        let mut sends: Vec<Instant> = Vec::new();
        for push in [0.0, 0.5] {
            assert!(rig.cap.allow("dev", rig.at(push)));
            sends.push(rig.at(push));
        }
        rig.observe(0.5, &[idle("s1"), snap("s2", Some("running"), &[])]);
        let mut dues = Vec::new();
        let mut take = |rig: &mut Rig, seconds: f64| {
            if let Some(due) = rig.away(seconds) {
                dues.push((seconds, due));
            }
        };
        take(&mut rig, 0.5);
        for n in 1..=20_u32 {
            let at = 0.5 * f64::from(n);
            let ids: Vec<u64> = (1..=u64::from(n)).collect();
            rig.observe(
                at,
                &[
                    snap("s1", Some("idle"), &ids),
                    snap("s2", Some("running"), &[]),
                ],
            );
            take(&mut rig, at);
            take(&mut rig, at + 0.25);
        }
        for quarter in 42..240_u32 {
            take(&mut rig, 0.25 * f64::from(quarter));
        }
        // The start, for the turn, then updates no closer than 2 s apart.
        assert_eq!(dues[0].1.event, Event::Start);
        assert_eq!(dues[0].1.content.kind, Kind::Running);
        for pair in dues.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= 2.0, "{dues:?}");
        }
        // An approval beats the turn that still runs, and the update names the approvals' session.
        for (_, due) in &dues[1..] {
            assert_eq!(due.event, Event::Update);
            assert_eq!(due.content.kind, Kind::Approval);
            assert_eq!(due.content.session, "s1");
        }
        // With the pushes, never more than the cap in any minute.
        sends.extend(dues.iter().map(|(seconds, _)| rig.at(*seconds)));
        for (i, first) in sends.iter().enumerate() {
            let within = sends[i..]
                .iter()
                .filter(|at| at.saturating_duration_since(*first) < Duration::from_secs(60))
                .count();
            assert!(within <= CAP, "{within} in a minute: {dues:?}");
        }
        // Activity sends leave the last of the minute to the pushes: with two pushes before
        // them, three activity sends went out, and an approval push after them still fits.
        assert_eq!(dues.len(), CAP - 3, "{dues:?}");
        assert!(rig.cap.allow("dev", rig.at(59.0)), "a push still fits");
        // The count shown last is not yet the latest; once the minute is over, it is.
        assert_ne!(dues.last().map(|(_, due)| due.content.approvals), Some(20));
        let due = rig.away(61.0).expect("the latest");
        assert_eq!(
            sent(Some(due)),
            expect(Event::Update, TOKEN, Kind::Approval, "s1", 20)
        );
        assert_eq!(rig.away(70.0), None);
    }

    #[test]
    fn ends_five_minutes_after_idle_and_on_connect() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.observe(10.0, &[idle("s1")]);
        assert_eq!(
            sent(rig.away(10.0)),
            expect(Event::Update, TOKEN, Kind::Done, "s1", 0)
        );
        // Idle for a minute short of five, then running again: the five minutes start over.
        rig.observe(250.0, &[snap("s1", Some("running"), &[])]);
        assert_eq!(rig.away(250.0).map(|due| due.event), Some(Event::Update));
        rig.observe(260.0, &[idle("s1")]);
        assert_eq!(rig.away(260.0).map(|due| due.event), Some(Event::Update));
        assert_eq!(rig.away(310.0), None);
        assert_eq!(rig.away(559.0), None);
        let due = rig.away(560.0).expect("an end");
        assert_eq!(due.priority(), Priority::Normal);
        assert_eq!(
            sent(Some(due)),
            expect(Event::End, TOKEN, Kind::Done, "s1", 0)
        );
        // Once; and the next news starts another.
        assert_eq!(rig.away(600.0), None);
        rig.observe(700.0, &[snap("s1", Some("idle"), &[2])]);
        assert_eq!(rig.away(700.0).map(|due| due.event), Some(Event::Start));

        // The phone connects: the activity ends at once, even mid-approval, and the app takes over.
        rig.driver.connected("dev");
        let end = rig.due(701.0, true).expect("an end");
        // An end is never urgent, even with an approval waiting. It goes to the token the app
        // handed over after this second start.
        assert_eq!(end.priority(), Priority::Normal);
        assert_eq!(
            sent(Some(end)),
            expect(Event::End, &woken(1), Kind::Approval, "s1", 1)
        );
        assert_eq!(rig.due(702.0, true), None);
        // Gone again with nothing new: nothing.
        assert_eq!(rig.away(710.0), None);
        // A connection with no activity the hub started ends nothing.
        rig.driver.connected("dev");
        assert_eq!(rig.due(711.0, true), None);

        // Without an activity token there is nothing to end, and the activity is forgotten all
        // the same: the next news while away starts one.
        let mut rig = Rig::with(&["s1"], tokens(None, Some(START)));
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.driver.connected("dev");
        assert_eq!(rig.due(2.0, true), None);
        rig.observe(5.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(rig.away(5.0).map(|due| due.event), Some(Event::Start));
        rig.observe(6.0, &[idle("s1")]);
        assert_eq!(rig.away(400.0), None);
    }

    #[test]
    fn an_end_waits_for_room_in_the_cap() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        // Four pushes after the start: one send is left in the minute, and that is the pushes'.
        for _ in 2..CAP {
            assert!(rig.cap.allow("dev", rig.at(2.0)));
        }
        rig.driver.connected("dev");
        assert_eq!(rig.due(3.0, true), None);
        assert_eq!(rig.due(60.0, true), None);
        // The start's minute is over: two are left, and the end takes one.
        let end = rig.due(61.0, true).expect("the end");
        assert_eq!((end.event, end.priority()), (Event::End, Priority::Normal));
        assert_eq!(rig.due(62.0, true), None);
        assert!(rig.cap.allow("dev", rig.at(62.0)), "a push still fits");
    }

    #[test]
    fn no_start_token_means_no_start() {
        for held in [tokens(Some(TOKEN), None), tokens(None, None)] {
            let mut rig = Rig::with(&["s1"], held);
            rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
            assert_eq!(rig.away(1.0), None);
            // No activity is known, so neither is there anything to update or end.
            rig.observe(3.0, &[snap("s1", Some("running"), &[1, 2])]);
            assert_eq!(rig.away(3.0), None);
            rig.observe(4.0, &[idle("s1")]);
            assert_eq!(rig.away(400.0), None);
            rig.driver.connected("dev");
            assert_eq!(rig.due(401.0, true), None);
            // And nothing was taken from the allowance.
            assert_eq!(rig.cap.remembered(), 0);
        }
        // No tokens at all: nothing.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        let now = rig.at(1.0);
        assert_eq!(
            rig.driver.due_for("other", None, false, &mut rig.cap, now),
            None
        );
    }

    #[test]
    fn a_hello_marks_where_news_starts_and_ends_nothing() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        // A background wake says hello: the activity goes on, and the next change updates it.
        rig.driver.hello("dev");
        rig.observe(4.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(rig.away(4.0).map(|due| due.event), Some(Event::Update));
        // A device that says hello and then has news before the hub first looks at it is
        // judged on that news.
        let now = rig.at(5.0);
        rig.driver.hello("new");
        rig.observe(5.0, &[snap("s1", Some("idle"), &[1, 2, 3])]);
        let held = rig.tokens.clone();
        assert_eq!(
            rig.driver
                .due_for("new", Some(&held), false, &mut rig.cap, now)
                .map(|due| due.event),
            Some(Event::Start)
        );
    }

    /// What APNs said to `due` for `device`, as the hub reports it, with the token it now stores.
    fn answer(rig: &mut Rig, due: &Due, outcome: Outcome) {
        let report = Report {
            device: "dev".into(),
            token: due.token.clone(),
            event: due.event,
            run: due.run,
            outcome,
        };
        let stored = rig.tokens.token.clone();
        rig.driver.failed(&report, stored.as_deref());
    }

    #[test]
    fn a_stale_tokens_410_forgets_no_live_activity() {
        const SOON: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";
        const NEW: &str = "d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3";
        let mut rig = Rig::with(&["s1"], tokens(None, Some(START)));
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        // A wake hands over SOON, and a change goes to it.
        rig.tokens = tokens(Some(SOON), Some(START));
        rig.observe(3.0, &[snap("s1", Some("idle"), &[1, 2])]);
        let update = rig.away(3.0).expect("an update");
        assert_eq!((update.event, update.token.as_str()), (Event::Update, SOON));
        // Another wake replaces SOON with NEW; then APNs answers 410 to SOON.
        rig.tokens = tokens(Some(NEW), Some(START));
        answer(&mut rig, &update, Outcome::Gone);
        // The activity goes on, no second start, and NEW is told the state as it stands,
        // though nothing changed.
        assert_eq!(
            sent(rig.away(5.0)),
            expect(Event::Update, NEW, Kind::Approval, "s1", 2)
        );
        assert_eq!(rig.away(9.0), None);
        // A 410 on the token the activity uses now does forget it: the next news starts one.
        rig.observe(10.0, &[snap("s1", Some("idle"), &[1, 2, 3])]);
        let current = rig.away(10.0).expect("an update");
        answer(&mut rig, &current, Outcome::Gone);
        rig.observe(12.0, &[snap("s1", Some("idle"), &[1, 2, 3, 4])]);
        assert_eq!(rig.away(12.0).map(|due| due.event), Some(Event::Start));
        // A start that does not go out forgets it too.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        let start = rig.away(1.0).expect("a start");
        answer(&mut rig, &start, Outcome::Failed(0));
        rig.observe(2.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(rig.away(2.0).map(|due| due.event), Some(Event::Start));
    }

    /// A first activity, ended by the app streaming a session, its token TOKEN still stored;
    /// then a second start, which TOKEN is therefore stale for. The end, and where the second
    /// start is.
    fn second_start() -> (Rig, Due) {
        let mut rig = Rig::with(&["s1"], tokens(None, Some(START)));
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.tokens = tokens(Some(TOKEN), Some(START));
        rig.driver.connected("dev");
        let end = rig.due(2.0, true).expect("an end");
        assert_eq!((end.event, end.token.as_str()), (Event::End, TOKEN));
        rig.observe(10.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(rig.away(10.0).map(|due| due.event), Some(Event::Start));
        (rig, end)
    }

    #[test]
    fn a_410_on_a_token_from_before_the_start_starts_no_third() {
        let (mut rig, end) = second_start();
        // A change: TOKEN is the first activity's, so nothing is sent and nothing is spent.
        let spent = rig.sends(12.0);
        rig.observe(12.0, &[snap("s1", Some("idle"), &[1, 2, 3])]);
        assert_eq!(rig.away(12.0), None);
        assert_eq!(rig.away(14.0), None);
        assert_eq!(rig.sends(14.0), spent);
        // The 410 to the first activity's end comes back now: it is not the second's.
        answer(&mut rig, &end, Outcome::Gone);
        rig.observe(16.0, &[snap("s1", Some("idle"), &[1, 2, 3, 4])]);
        assert_eq!(rig.away(16.0), None, "no third start");
        assert_eq!(rig.sends(16.0), spent);
        // An end now has only TOKEN to go to: it is not sent either.
        rig.driver.connected("dev");
        assert_eq!(rig.due(17.0, true), None);
        assert_eq!(rig.sends(17.0), spent);
    }

    #[test]
    fn the_real_token_is_told_the_state_without_a_change() {
        const NEW: &str = "d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3";
        let (mut rig, _) = second_start();
        // The approval is answered and a turn runs: the update to TOKEN is not sent.
        let spent = rig.sends(12.0);
        rig.observe(12.0, &[snap("s1", Some("running"), &[])]);
        assert_eq!(rig.away(12.0), None);
        assert_eq!(rig.sends(12.0), spent);
        // The wake hands over the second activity's token: it gets the running state, with
        // nothing else changing, and once.
        rig.tokens = tokens(Some(NEW), Some(START));
        assert_eq!(
            sent(rig.away(13.0)),
            expect(Event::Update, NEW, Kind::Running, "s1", 0)
        );
        assert_eq!(rig.away(20.0), None);
        // And the end, when the app streams again, goes to NEW.
        rig.driver.connected("dev");
        assert_eq!(
            rig.due(21.0, true).map(|due| due.token),
            Some(NEW.to_owned())
        );
    }

    #[test]
    fn a_failed_end_is_sent_again_within_the_cap() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.driver.connected("dev");
        let first = rig.due(2.0, true).expect("an end");
        // APNs failed it: it goes again at the next look, not at once in a loop.
        answer(&mut rig, &first, Outcome::Failed(500));
        let second = rig.due(3.0, true).expect("the end again");
        assert_eq!((second.event, second.token.as_str()), (Event::End, TOKEN));
        answer(&mut rig, &second, Outcome::Failed(0));
        let third = rig.due(4.0, true).expect("the end a third time");
        assert_eq!(third.event, Event::End);
        // Three tries in all.
        answer(&mut rig, &third, Outcome::Failed(0));
        assert_eq!(rig.due(5.0, true), None);
        assert_eq!(rig.due(70.0, true), None);
        // A 410 to an end is not tried again: that activity is gone.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.driver.connected("dev");
        let end = rig.due(2.0, true).expect("an end");
        answer(&mut rig, &end, Outcome::Gone);
        assert_eq!(rig.due(3.0, true), None);
        // Nor is an end whose failure comes back after another activity started.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.driver.connected("dev");
        let end = rig.due(2.0, true).expect("an end");
        rig.observe(5.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(rig.away(5.0).map(|due| due.event), Some(Event::Start));
        answer(&mut rig, &end, Outcome::Failed(500));
        assert_eq!(rig.away(6.0), None);
        // A retried end still waits for room in the cap.
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        rig.driver.connected("dev");
        let end = rig.due(2.0, true).expect("an end");
        for _ in 2..CAP {
            assert!(rig.cap.allow("dev", rig.at(2.5)));
        }
        answer(&mut rig, &end, Outcome::Failed(500));
        assert_eq!(rig.due(3.0, true), None);
        assert_eq!(rig.due(61.0, true), None);
        assert_eq!(rig.due(62.0, true).map(|due| due.event), Some(Event::End));
    }

    #[test]
    fn a_cleared_activity_starts_afresh() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        // The phone turns the Live Activity off: `clear_activity` leaves it no tokens.
        let now = rig.at(2.0);
        assert_eq!(
            rig.driver.due_for("dev", None, false, &mut rig.cap, now),
            None
        );
        // Then on again, with both tokens: the next news is a start, not an update.
        rig.observe(5.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(
            sent(rig.away(5.0)),
            expect(Event::Start, START, Kind::Approval, "s1", 2)
        );
    }

    #[test]
    fn a_clear_and_a_register_within_a_second_start_afresh() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        assert_eq!(rig.away(1.0).map(|due| due.event), Some(Event::Start));
        // `clear_activity` and then `register_activity`, both before the hub's next look: the
        // clear resets the device at once.
        rig.driver.cleared("dev");
        rig.observe(1.5, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(
            sent(rig.away(1.5)),
            expect(Event::Start, START, Kind::Approval, "s1", 2)
        );
    }

    #[test]
    fn a_new_lead_session_is_an_update() {
        let mut rig = Rig::new(&["s1", "s2"]);
        rig.observe(
            1.0,
            &[
                snap("s1", Some("idle"), &[1]),
                snap("s2", Some("idle"), &[5]),
            ],
        );
        assert_eq!(
            sent(rig.away(1.0)),
            expect(Event::Start, START, Kind::Approval, "s1", 2)
        );
        // s1's approval is answered and another comes: still two waiting, but s2's is the
        // oldest now, so the widget's buttons must point at s2.
        rig.observe(
            2.0,
            &[
                snap("s1", Some("idle"), &[2]),
                snap("s2", Some("idle"), &[5]),
            ],
        );
        assert_eq!(rig.away(2.0), None);
        assert_eq!(
            sent(rig.away(3.0)),
            expect(Event::Update, TOKEN, Kind::Approval, "s2", 2)
        );
        assert_eq!(rig.away(9.0), None);
    }

    /// An activity over the test key, sending with a `curl` that is not there, so nothing leaves
    /// the machine.
    fn activity(dir: &octet_testkit::TempDir) -> (Activity, mpsc::Receiver<Report>) {
        use std::os::unix::fs::PermissionsExt;
        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/apns-test.p8");
        let text = std::fs::read_to_string(data).expect("the test key");
        let pem = &text[text.find("-----BEGIN").expect("a PEM block")..];
        std::fs::create_dir_all(dir.path()).expect("dir");
        let key = dir.path().join("key.p8");
        std::fs::write(&key, pem).expect("key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("mode");
        let config = ApnsConfig {
            team_id: "TEAM123456".into(),
            key_id: "KEY1234567".into(),
            key_path: key,
            sandbox: true,
        };
        let signer = Signer::load(&config).expect("the test key loads");
        let (reports, received) = mpsc::channel(4);
        let mut activity = Activity::new(config, signer, reports);
        activity.program = "/nonexistent/curl".into();
        (activity, received)
    }

    fn device(tokens: Option<ActivityTokens>) -> Device {
        Device {
            name: "Test phone".into(),
            platform: "ios".into(),
            login: "owner@example".into(),
            created: 0,
            last_seen: 0,
            token_sha256: "dev".into(),
            push_token: None,
            push_kinds: octet_core::remote::protocol::PushKinds::default(),
            activity: tokens,
        }
    }

    #[tokio::test]
    async fn a_send_with_no_slot_takes_nothing_from_the_cap() {
        let dir = octet_testkit::TempDir::new("activity-slots");
        let (mut activity, mut received) = activity(&dir);
        let devices = [device(Some(tokens(Some(TOKEN), Some(START))))];
        let base = Instant::now();
        let wall = SystemTime::now();
        let mut cap = Limiter::default();
        activity.observe(&[idle("s1")], base);
        activity.hello("dev");
        activity.observe(&[snap("s1", Some("idle"), &[1])], later(base, 1.0));
        // Every slot busy: nothing is judged, nothing is taken from the cap.
        let busy = Arc::clone(&activity.slots)
            .try_acquire_many_owned(u32::try_from(SENDS_AT_ONCE).expect("small"))
            .expect("the slots");
        activity.flush(
            &devices,
            |_| false,
            &mut cap,
            None,
            (later(base, 1.0), wall),
        );
        assert_eq!(cap.remembered(), 0);
        // With a slot free, the start goes, and takes one.
        drop(busy);
        activity.flush(
            &devices,
            |_| false,
            &mut cap,
            None,
            (later(base, 2.0), wall),
        );
        assert_eq!(cap.remembered(), 1);
        // The `curl` that is not there fails it, and the hub hears so; the start is forgotten.
        let report = timeout(Duration::from_secs(5), received.recv())
            .await
            .expect("a report")
            .expect("a report");
        assert_eq!(
            (report.event, report.outcome),
            (Event::Start, Outcome::Failed(0))
        );
        assert!(!activity.reported(&report, Some(TOKEN)));
        activity.observe(&[snap("s1", Some("idle"), &[1, 2])], later(base, 3.0));
        let mut cap = Limiter::default();
        activity.flush(
            &devices,
            |_| false,
            &mut cap,
            None,
            (later(base, 3.0), wall),
        );
        assert_eq!(cap.remembered(), 1, "started again");
    }

    #[test]
    fn a_lost_activity_is_started_again_by_the_next_news() {
        let mut rig = Rig::new(&["s1"]);
        rig.observe(1.0, &[snap("s1", Some("idle"), &[1])]);
        let start = rig.away(1.0).expect("a start");
        // APNs said the start token is gone.
        answer(&mut rig, &start, Outcome::Gone);
        rig.observe(4.0, &[snap("s1", Some("idle"), &[1, 2])]);
        assert_eq!(
            sent(rig.away(4.0)),
            expect(Event::Start, START, Kind::Approval, "s1", 2)
        );
        // A device the driver never saw is nothing to lose.
        let stranger = Report {
            device: "nobody".into(),
            token: START.into(),
            event: Event::Start,
            run: 2,
            outcome: Outcome::Gone,
        };
        rig.driver.failed(&stranger, None);
        // Forgotten devices are judged afresh, on the news as it stands.
        rig.driver.prune(|device| device != "dev");
        rig.observe(5.0, &[snap("s1", Some("idle"), &[1, 2, 3])]);
        let now = rig.at(5.0);
        let held = rig.tokens.clone();
        assert_eq!(
            rig.driver
                .due_for("late", Some(&held), false, &mut rig.cap, now),
            None
        );
    }

    #[test]
    fn the_lead_and_since_follow_the_rulings() {
        let mut rig = Rig::new(&["s1", "s2", "s3"]);
        let content = |rig: &Rig, seconds: f64| rig.driver.content(rig.at(seconds));
        // The one with the oldest waiting approval leads; the count is every session's.
        rig.observe(
            1.0,
            &[snap("s1", Some("idle"), &[1]), idle("s2"), idle("s3")],
        );
        rig.observe(
            2.0,
            &[
                snap("s1", Some("idle"), &[1]),
                snap("s2", Some("running"), &[5, 6]),
                idle("s3"),
            ],
        );
        let now = content(&rig, 2.0);
        assert_eq!((now.kind, now.session.as_str()), (Kind::Approval, "s1"));
        assert_eq!((now.since, now.approvals), (rig.at(1.0), 3));
        // s1's approval is answered and another comes: s2's are older now.
        rig.observe(
            3.0,
            &[
                snap("s1", Some("idle"), &[2]),
                snap("s2", Some("running"), &[5, 6]),
                idle("s3"),
            ],
        );
        let now = content(&rig, 3.0);
        assert_eq!((now.session.as_str(), now.since), ("s2", rig.at(2.0)));
        // No approvals: the most recently started running session leads, since its turn began.
        rig.observe(
            4.0,
            &[
                idle("s1"),
                snap("s2", Some("running"), &[]),
                snap("s3", Some("running"), &[]),
            ],
        );
        let now = content(&rig, 4.0);
        assert_eq!(
            (now.kind, now.session.as_str(), now.since, now.approvals),
            (Kind::Running, "s3", rig.at(4.0), 0)
        );
        rig.observe(
            5.0,
            &[idle("s1"), snap("s2", Some("running"), &[]), idle("s3")],
        );
        let now = content(&rig, 5.0);
        // s2 has been running since its approvals were answered.
        assert_eq!((now.session.as_str(), now.since), ("s2", rig.at(4.0)));
        // All idle: the last that finished leads, since it finished.
        rig.observe(7.0, &[idle("s1"), idle("s2"), idle("s3")]);
        let now = content(&rig, 7.0);
        assert_eq!(
            (now.kind, now.session.as_str(), now.since),
            (Kind::Done, "s2", rig.at(7.0))
        );
        // A session that loses its state runs no more and has nothing open.
        rig.observe(
            8.0,
            &[snap("s1", Some("running"), &[9]), idle("s2"), idle("s3")],
        );
        rig.observe(9.0, &[snap("s1", None, &[]), idle("s2"), idle("s3")]);
        let now = content(&rig, 9.0);
        assert_eq!(
            (now.kind, now.session.as_str(), now.approvals),
            (Kind::Done, "s1", 0)
        );
        // Sessions that never ran lead nothing: no session, since every session went idle.
        let rig = Rig::new(&["s1"]);
        let now = content(&rig, 5.0);
        assert_eq!(
            (now.kind, now.session.as_str(), now.since),
            (Kind::Done, "", rig.at(0.0))
        );
    }

    /// The fixture the app's test reads: one payload of each event.
    fn fixture() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/activity-payload.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("the fixture")).expect("JSON")
    }

    #[test]
    fn the_payload_is_exactly_the_fixture() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(1_790_000_101);
        let due = |event, kind, approvals| Due {
            event,
            run: 1,
            token: TOKEN.into(),
            content: Content {
                kind,
                session: "6101-1790000000".into(),
                since: now - Duration::from_secs(42),
                approvals,
            },
        };
        let fixture = fixture();
        assert_eq!(
            due(Event::Start, Kind::Approval, 2).payload(Some(MAC), now, wall),
            fixture["start"]
        );
        assert_eq!(
            due(Event::Update, Kind::Running, 0).payload(Some(MAC), now, wall),
            fixture["update"]
        );
        assert_eq!(
            due(Event::End, Kind::Done, 0).payload(Some(MAC), now, wall),
            fixture["end"]
        );
        assert_eq!(fixture.as_object().map(serde_json::Map::len), Some(3));
        // A start for a turn says so; one for an approval says that instead.
        let running = due(Event::Start, Kind::Running, 0).payload(Some(MAC), now, wall);
        assert_eq!(
            running["aps"]["alert"],
            json!({"title": "Octet", "body": "Session running"})
        );
        assert_eq!(
            fixture["start"]["aps"]["alert"],
            json!({"title": "Octet", "body": "Approval waiting"})
        );
        // Only a start carries the attributes: ActivityKit reads them to make the activity.
        for event in ["update", "end"] {
            let aps = fixture[event]["aps"].as_object().expect("aps");
            assert!(!aps.contains_key("attributes-type"), "{event}");
            assert!(
                !aps.contains_key("attributes") && !aps.contains_key("alert"),
                "{event}"
            );
        }
        // Unpublished, `mac` is empty; with no session leading, so is `session`.
        let mut lone = due(Event::Update, Kind::Done, 0);
        lone.content.session.clear();
        let payload = lone.payload(None, now, wall);
        assert_eq!(payload["aps"]["content-state"]["mac"], "");
        assert_eq!(payload["aps"]["content-state"]["session"], "");
    }

    #[test]
    fn the_payload_is_content_free() {
        let mut rig = Rig::new(&["4242-1790000000"]);
        let wall = SystemTime::now();
        rig.observe(1.0, &[snap("4242-1790000000", Some("idle"), &[1])]);
        let start = rig.away(1.0).expect("a start");
        rig.observe(2.0, &[snap("4242-1790000000", Some("running"), &[])]);
        let update = rig.away(3.0).expect("an update");
        rig.driver.connected("dev");
        let end = rig.due(4.0, true).expect("an end");
        for (due, extra) in [
            (start, &["alert", "attributes", "attributes-type"][..]),
            (update, &[][..]),
            (end, &["dismissal-date"][..]),
        ] {
            let payload = due.payload(Some(MAC), rig.at(4.0), wall);
            let text = payload.to_string();
            for secret in ["/work", "secret", "jsonl", "session-1", TOKEN, START] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
            let mut keys: Vec<&str> = payload["aps"]
                .as_object()
                .expect("aps")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let mut want = vec!["content-state", "event", "timestamp"];
            want.extend(extra);
            want.sort_unstable();
            assert_eq!(keys, want);
            let state = &payload["aps"]["content-state"];
            let mut fields: Vec<&str> = state
                .as_object()
                .expect("state")
                .keys()
                .map(String::as_str)
                .collect();
            fields.sort_unstable();
            assert_eq!(fields, ["approvals", "kind", "mac", "session", "since"]);
            assert_eq!(state["session"], "4242-1790000000");
            assert_eq!(state["mac"], MAC);
            assert_eq!(payload.as_object().map(serde_json::Map::len), Some(1));
        }
        // Nothing prints a token.
        let due = Due {
            event: Event::Start,
            run: 1,
            token: START.into(),
            content: rig.driver.content(rig.at(5.0)),
        };
        let report = Report {
            device: "f".repeat(64),
            token: TOKEN.into(),
            event: Event::Update,
            run: 1,
            outcome: Outcome::Gone,
        };
        let printed = format!("{due:?} {report:?} {:?}", rig.driver);
        for secret in [START, TOKEN, "ffff"] {
            assert!(!printed.contains(secret), "{printed}");
        }
    }
}
