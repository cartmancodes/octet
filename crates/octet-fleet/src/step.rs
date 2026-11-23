//! One attempt of one step against an [`octet_core::Session`].
//!
//! The vendor runs held by the gate ([`crate::gate`]) until its identity is
//! durable: [`run_attempt`] writes the gate, opens the session with the gate
//! as its binary, waits for the gate to register its PID, appends
//! `attempt_session` and only then releases the gate. It sends the prompt
//! once the session is `Ready`, forwards every session event as
//! [`FleetEvent::Session`], answers approvals (always deny for a read-only
//! step, else the [`Approver`]'s answer), logs each new vendor session ID,
//! and waits out the vendor's background tasks before it takes the reply.
//! A reply its [`FollowUp`] does not accept gets that follow-up prompt once,
//! in the same session. The run's interrupt ([`AttemptPlan::interrupt`])
//! ends the turn with [`TurnEnd::Interrupted`]; the future itself is never
//! dropped.
//!
//! Log appends are awaited in the loop's bodies, never inside a `select!`:
//! [`Log::append`] is not cancel-safe.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use octet_core::background::Background;
use octet_core::{
    ApprovalSummary, Command, Config, Engine, Event, Mode, Outcome, SendError, Session,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::classify::{FailureClass, classify};
use crate::gate;
use crate::machine::Dispatch;
use crate::ownership;
use crate::record::{AttemptId, Identity, Record};
use crate::store::{Log, StoreError};
use crate::validate::Step;
use crate::{FleetEvent, Signals, signalled};

/// How long the gate may take to register its PID after the session opens.
const REGISTRATION: Duration = Duration::from_secs(30);

/// How often the gate's registry is read while waiting for it.
const REGISTRATION_POLL: Duration = Duration::from_millis(20);

/// Why a turn has no reply when the session's events end without saying so.
const UNFINISHED: &str = "The session ended without finishing the turn; see its journal";

/// Everything one attempt needs.
#[derive(Debug)]
pub struct AttemptPlan<'a> {
    /// The step, with its resolved profile.
    pub step: &'a Step,
    /// The machine's dispatch: the round and any session to resume.
    pub dispatch: &'a Dispatch,
    /// The attempt, already `attempt_started` in the log.
    pub attempt: AttemptId,
    /// The prompt, as the contract builds it.
    pub prompt: String,
    /// The directory the vendor works in.
    pub cwd: PathBuf,
    /// `<run>/attempts/<attempt>/`: the gate, its registry and the journal.
    pub attempt_dir: PathBuf,
    /// The attempt's `OCTET_FLEET_ATTEMPT` value, `<run>/<attempt>`.
    pub marker: String,
    /// Octet's data directory, for a provider that keeps files there.
    pub data_dir: PathBuf,
    /// The run's signals: once this holds a signal's name, the turn ends
    /// with [`TurnEnd::Interrupted`]. `None`: never interrupted.
    pub interrupt: Option<Signals>,
    /// The one follow-up a reply may get, in the same session.
    pub follow_up: Option<FollowUp<'a>>,
    /// Tests only: with the gate registered, create
    /// `held-session-<attempt>` here and wait for `continue-session-<attempt>`
    /// before `attempt_session` is appended; then `held-go-<attempt>` and
    /// `continue-go-<attempt>` before the gate is released.
    pub pause_hook: Option<PathBuf>,
}

/// A prompt sent once, in the same session, after a reply that `accept`
/// refuses (the contract's corrective prompt). The reply to it is the
/// attempt's reply, accepted or not.
pub struct FollowUp<'a> {
    /// Whether a reply is acceptable as it is.
    pub accept: &'a (dyn Fn(&str) -> bool + Sync),
    /// The prompt sent after a reply `accept` refuses.
    pub prompt: String,
}

impl std::fmt::Debug for FollowUp<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FollowUp")
            .field("prompt", &self.prompt)
            .finish_non_exhaustive()
    }
}

/// Answers a write step's approval requests; the front end implements it.
/// A read-only step never calls it: its requests are all denied.
pub trait Approver: Send {
    /// Whether to allow what `summary` describes.
    fn ask<'a>(
        &'a mut self,
        summary: &'a ApprovalSummary,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// How an attempt's turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// The turn completed, background work included.
    Replied {
        /// The final turn's assistant text.
        text: String,
    },
    /// The turn, or the session, failed; the text says how.
    Failed(FailureClass, String),
    /// The run's own interrupt ([`AttemptPlan::interrupt`]) ended the turn,
    /// or the registration before it.
    Interrupted,
}

/// Why an attempt could not be carried on at all.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The run log refused a record, so nothing more can be made durable.
    #[error("cannot append to the run log: {0}")]
    Log(#[from] StoreError),
}

/// Runs one attempt of `plan.step`: the gate, the session, the registration,
/// the prompt, and the reply or failure. The session is shut down before
/// this returns, whatever the outcome.
///
/// `events` must be read while this runs: each send is awaited, and a
/// receiver that stops reading stalls the attempt (its session then stops
/// as a stalled consumer). A dropped receiver is ignored.
///
/// **Not cancel-safe.** The future appends to the run log, so dropping it
/// (in a `select!` or a `timeout`) can cut an append short, and `Session`'s
/// `Drop` does not wait for the journal's tail. An attempt is interrupted
/// through [`AttemptPlan::interrupt`], which ends the turn with
/// [`TurnEnd::Interrupted`], never by dropping this future.
///
/// # Errors
///
/// [`StepError::Log`] when `attempt_session` or `vendor_session` cannot be
/// appended. A failure of the vendor or of Octet's session is a
/// [`TurnEnd::Failed`], not an error.
pub async fn run_attempt(
    plan: AttemptPlan<'_>,
    log: &mut Log,
    approver: &mut dyn Approver,
    events: &mpsc::Sender<FleetEvent>,
) -> Result<TurnEnd, StepError> {
    let (engine, mode) = match settings(plan.step) {
        Ok(settings) => settings,
        Err(detail) => return Ok(TurnEnd::Failed(FailureClass::ConfigError, detail)),
    };
    let mut session = match open(&plan, engine, mode).await {
        Ok(session) => session,
        Err(detail) => return Ok(local(detail)),
    };
    let end = attend(&plan, &mut session, log, approver, events).await;
    session.shutdown().await;
    end
}

/// Registers the held vendor, makes its identity durable, releases it and
/// drives the turn.
async fn attend(
    plan: &AttemptPlan<'_>,
    session: &mut Session,
    log: &mut Log,
    approver: &mut dyn Approver,
    events: &mpsc::Sender<FleetEvent>,
) -> Result<TurnEnd, StepError> {
    let mut early = VecDeque::new();
    let mut interrupt = plan.interrupt.clone();
    let registered = register(session, &plan.attempt_dir, &mut early, &mut interrupt).await;
    let identities = match registered {
        Ok(identities) => identities,
        Err(Unregistered::Failed(detail)) => return Ok(local(detail)),
        Err(Unregistered::Interrupted) => return Ok(TurnEnd::Interrupted),
    };
    if let Err(detail) = test_pause(plan, "session").await {
        return Ok(local(detail));
    }
    let record = Record::AttemptSession {
        attempt: plan.attempt,
        journal: session.journal().to_path_buf(),
        identities,
    };
    log.append(&record).await?;
    if let Err(detail) = test_pause(plan, "go").await {
        return Ok(local(detail));
    }
    // Only now may the vendor run.
    if let Err(error) = gate::release(&plan.attempt_dir).await {
        return Ok(local(format!("Cannot release the vendor: {error}")));
    }
    Turn::new(plan, early)
        .drive(session, log, approver, events)
        .await
}

/// The plan's test pause `<what>-<attempt>`, if it has a pause hook. An
/// error is a local failure, as text.
async fn test_pause(plan: &AttemptPlan<'_>, what: &str) -> Result<(), String> {
    let Some(dir) = &plan.pause_hook else {
        return Ok(());
    };
    crate::evidence::test_pause(dir, &format!("{what}-{}", plan.attempt.0))
        .await
        .map_err(|error| format!("The test pause failed: {error}"))
}

fn local(detail: String) -> TurnEnd {
    TurnEnd::Failed(FailureClass::LocalFailure, detail)
}

/// The step's engine and mode, and a check that its binary is absolute:
/// the gate names it, and a relative path would name another file.
fn settings(step: &Step) -> Result<(Engine, Mode), String> {
    let profile = &step.profile;
    let engine = Engine::parse(&profile.engine)
        .ok_or_else(|| format!("Unknown engine {:?}", profile.engine))?;
    let mode =
        Mode::parse(&profile.mode).ok_or_else(|| format!("Unknown mode {:?}", profile.mode))?;
    if !profile.binary.is_absolute() {
        return Err(format!(
            "The vendor binary {} is not an absolute path",
            profile.binary.display()
        ));
    }
    Ok((engine, mode))
}

/// Writes the gate and opens the session with the gate as its binary. The
/// session returns before the vendor's handshake, so the held gate cannot
/// block it.
async fn open(plan: &AttemptPlan<'_>, engine: Engine, mode: Mode) -> Result<Session, String> {
    let dir = &plan.attempt_dir;
    octet_store::create_private_dir(dir)
        .await
        .map_err(|error| format!("Cannot create {}: {error}", dir.display()))?;
    let profile = &plan.step.profile;
    let gate = gate::write_gate(dir, &profile.binary, &plan.marker, std::process::id())
        .await
        .map_err(|error| format!("Cannot write the vendor gate: {error}"))?;
    let mut config = Config::new(engine, gate, &plan.cwd);
    config.model.clone_from(&profile.model);
    config.effort.clone_from(&profile.effort);
    config.mode = mode;
    config.approval_timeout = Duration::from_secs(profile.approval_timeout_secs);
    config.resume.clone_from(&plan.dispatch.resume_session);
    config.data_dir = Some(plan.data_dir.clone());
    Session::open(config, dir.clone())
        .await
        .map_err(|error| format!("Cannot open the session: {error}"))
}

/// Why the vendor was not registered; the gate stays held either way.
enum Unregistered {
    /// Registration failed, as the text says.
    Failed(String),
    /// The run's interrupt came first.
    Interrupted,
}

/// Waits for the gate to register at least one PID and reads each one's
/// identity. Session events that arrive meanwhile are kept in `early`, in
/// order. Any failure, and the run's interrupt, leaves the gate held.
async fn register(
    session: &mut Session,
    dir: &Path,
    early: &mut VecDeque<Event>,
    interrupt: &mut Option<Signals>,
) -> Result<Vec<Identity>, Unregistered> {
    let deadline = Instant::now() + REGISTRATION;
    let pids = loop {
        let pids = gate::read_processes(dir).await.map_err(|error| {
            Unregistered::Failed(format!("Cannot read the vendor's registration: {error}"))
        })?;
        if !pids.is_empty() {
            break pids;
        }
        if Instant::now() >= deadline {
            return Err(Unregistered::Failed(format!(
                "The vendor gate did not register within {} s",
                REGISTRATION.as_secs()
            )));
        }
        tokio::select! {
            event = session.events.recv() => {
                let Some(event) = event else {
                    return Err(Unregistered::Failed(ended_before_registration(early)));
                };
                early.push_back(event);
            }
            () = tokio::time::sleep(REGISTRATION_POLL) => {}
            _ = signalled(interrupt) => return Err(Unregistered::Interrupted),
        }
    };
    let mut identities = Vec::with_capacity(pids.len());
    for pid in pids {
        let Some(start) = ownership::start_time(pid).await else {
            return Err(Unregistered::Failed(format!(
                "Cannot read the start time of process {pid}"
            )));
        };
        identities.push(Identity { pid, start });
    }
    Ok(identities)
}

/// Why registration failed when the session ended first, with the last
/// error it reported.
fn ended_before_registration(early: &VecDeque<Event>) -> String {
    let said = early.iter().rev().find_map(|event| {
        if let Event::Error(text) = event {
            Some(format!(": {text}"))
        } else {
            None
        }
    });
    format!(
        "The session ended before the vendor gate registered{}",
        said.unwrap_or_default()
    )
}

/// The reply text of the turn running now, with a blank line between the
/// text before a tool call and the text after it.
#[derive(Debug, Default)]
struct Reply {
    text: String,
    paragraph: bool,
}

impl Reply {
    fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if std::mem::take(&mut self.paragraph) {
            if !self.text.ends_with('\n') {
                self.text.push('\n');
            }
            self.text.push('\n');
        }
        self.text.push_str(chunk);
    }

    fn tool(&mut self) {
        self.paragraph = !self.text.is_empty();
    }
}

/// The approval an `ApprovalClosed` event closes.
fn closed_id(event: &Event) -> Option<u64> {
    if let Event::ApprovalClosed(id) = event {
        Some(*id)
    } else {
        None
    }
}

/// What a turn needs of its session: its events and its commands. A trait so
/// the turn's logic can be tested against a scripted event sequence.
trait Link {
    /// The next event; `None` once the stream has ended. Cancel-safe.
    fn next(&mut self) -> impl Future<Output = Option<Event>> + Send;

    /// Sends `command` to the vendor.
    fn send(&self, command: Command) -> Result<(), SendError>;
}

impl Link for Session {
    fn next(&mut self) -> impl Future<Output = Option<Event>> + Send {
        self.events.recv()
    }

    fn send(&self, command: Command) -> Result<(), SendError> {
        self.handle.send(command)
    }
}

/// One attempt's turn, from `Ready` to its end.
struct Turn<'p> {
    attempt: AttemptId,
    engine: &'p str,
    read_only: bool,
    /// The prompt, until it is sent.
    prompt: Option<String>,
    /// The follow-up, until it is sent.
    follow_up: Option<FollowUp<'p>>,
    /// The run's signals.
    interrupt: Option<Signals>,
    /// The run's interrupt came while an approval was asked.
    interrupted: bool,
    /// Events read but not yet handled.
    queue: VecDeque<Event>,
    /// Approvals whose `ApprovalClosed` is in `queue`: read ahead of their
    /// request, which must then not be asked or answered. An ID leaves the
    /// set when its close is handled.
    closed: HashSet<u64>,
    /// A turn runs: between `Started` and `Finished`.
    running: bool,
    /// The prompt's turn completed and background work may start another.
    held: bool,
    reply: Reply,
    /// The last error the session reported since the turn started.
    error: Option<String>,
    /// The vendor session ID last logged.
    logged: Option<String>,
    background: Background,
}

impl<'p> Turn<'p> {
    fn new(plan: &'p AttemptPlan<'_>, early: VecDeque<Event>) -> Self {
        Self {
            attempt: plan.attempt,
            engine: &plan.step.profile.engine,
            read_only: plan.step.read_only(),
            prompt: Some(plan.prompt.clone()),
            follow_up: plan.follow_up.as_ref().map(|f| FollowUp {
                accept: f.accept,
                prompt: f.prompt.clone(),
            }),
            interrupt: plan.interrupt.clone(),
            interrupted: false,
            closed: early.iter().filter_map(closed_id).collect(),
            queue: early,
            running: false,
            held: false,
            reply: Reply::default(),
            error: None,
            logged: None,
            background: Background::default(),
        }
    }

    async fn drive(
        &mut self,
        session: &mut (impl Link + Send),
        log: &mut Log,
        approver: &mut dyn Approver,
        events: &mpsc::Sender<FleetEvent>,
    ) -> Result<TurnEnd, StepError> {
        loop {
            let event = if let Some(event) = self.queue.pop_front() {
                Some(event)
            } else {
                tokio::select! {
                    event = session.next() => event,
                    () = self.background.grace_over(), if self.held => {
                        if !self.background.pending()
                            && let Some(end) = self.complete(session)
                        {
                            return Ok(end);
                        }
                        continue;
                    }
                    _ = signalled(&mut self.interrupt) => return Ok(TurnEnd::Interrupted),
                }
            };
            let Some(event) = event else {
                // Only the core pump ends the stream without `Stopped`: the
                // journal failed, or this consumer stalled.
                let detail = self.error.take().unwrap_or_else(|| UNFINISHED.into());
                return Ok(local(detail));
            };
            self.background.track(&event, !self.running);
            // Nobody listening is not a reason to stop the attempt.
            let _ = events
                .send(FleetEvent::Session {
                    attempt: self.attempt,
                    event: event.clone(),
                })
                .await;
            if let Some(end) = self.handle(event, session, log, approver).await? {
                return Ok(end);
            }
        }
    }

    /// Handles one event; `Some` when it ends the attempt.
    async fn handle(
        &mut self,
        event: Event,
        session: &mut (impl Link + Send),
        log: &mut Log,
        approver: &mut dyn Approver,
    ) -> Result<Option<TurnEnd>, StepError> {
        match event {
            Event::Ready { session: id } => {
                if !id.is_empty() && self.logged.as_deref() != Some(id.as_str()) {
                    let record = Record::VendorSession {
                        attempt: self.attempt,
                        id: id.clone(),
                    };
                    log.append(&record).await?;
                    self.logged = Some(id);
                }
                if let Some(prompt) = self.prompt.take()
                    && let Err(error) = session.send(Command::Prompt(prompt))
                {
                    return Ok(Some(local(format!("Cannot send the prompt: {error}"))));
                }
            }
            Event::Started => {
                self.running = true;
                self.held = false;
                self.reply = Reply::default();
                self.error = None;
            }
            Event::Text(text) => self.reply.push(&text),
            Event::Tool(_) => self.reply.tool(),
            Event::Error(text) => self.error = Some(text),
            // Closed before its turn came: nobody can answer it any more.
            Event::Approval { id, .. } if self.closed.contains(&id) => {}
            Event::ApprovalClosed(id) => {
                self.closed.remove(&id);
            }
            Event::Approval { id, summary, .. } => {
                let allow = if self.read_only {
                    Some(false)
                } else {
                    self.ask(approver, &summary, id, session).await
                };
                if self.interrupted {
                    return Ok(Some(TurnEnd::Interrupted));
                }
                // An answer that cannot be delivered leaves the request to
                // time out, which denies it.
                if let Some(allow) = allow {
                    let _ = session.send(Command::Answer { id, allow });
                }
            }
            Event::Finished { outcome } => {
                self.running = false;
                return Ok(self.finished(&outcome, session));
            }
            Event::Stopped => return Ok(Some(self.stopped())),
            _ => {}
        }
        Ok(None)
    }

    /// Asks the approver while still reading the session, so a slow answer
    /// cannot stall it. `None` when the request closed (timed out or
    /// withdrawn) or the session ended first: there is nothing to answer;
    /// and when the run's interrupt came, which sets `interrupted`.
    async fn ask(
        &mut self,
        approver: &mut dyn Approver,
        summary: &ApprovalSummary,
        id: u64,
        session: &mut (impl Link + Send),
    ) -> Option<bool> {
        let mut asking = approver.ask(summary);
        let mut interrupt = self.interrupt.clone();
        loop {
            tokio::select! {
                allow = &mut asking => return Some(allow),
                _ = signalled(&mut interrupt) => {
                    self.interrupted = true;
                    return None;
                }
                event = session.next() => match event {
                    Some(event) => {
                        let closed = closed_id(&event);
                        self.queue.push_back(event);
                        if let Some(closed) = closed {
                            self.closed.insert(closed);
                            if closed == id {
                                return None;
                            }
                        }
                    }
                    None => return None,
                },
            }
        }
    }

    /// A turn ended: the reply, unless background work may start another
    /// turn or the reply gets its follow-up, or the turn's failure.
    fn finished(&mut self, outcome: &Outcome, session: &impl Link) -> Option<TurnEnd> {
        match outcome {
            Outcome::Completed if self.background.pending() => {
                self.held = true;
                None
            }
            Outcome::Completed => self.complete(session),
            Outcome::Interrupted => {
                let detail = self
                    .error
                    .take()
                    .unwrap_or_else(|| "The vendor ended the turn as interrupted".into());
                Some(TurnEnd::Failed(FailureClass::Unknown, detail))
            }
            Outcome::Failed | Outcome::Other(_) => {
                let detail = self
                    .error
                    .take()
                    .unwrap_or_else(|| format!("The turn ended {}", outcome.as_str()));
                Some(TurnEnd::Failed(
                    classify(self.engine, &detail, false),
                    detail,
                ))
            }
        }
    }

    /// The vendor's session stopped before the attempt ended.
    fn stopped(&mut self) -> TurnEnd {
        let error = self.error.take();
        if self.held {
            let detail =
                error.unwrap_or_else(|| "The vendor stopped while its background tasks ran".into());
            return TurnEnd::Failed(FailureClass::Unknown, detail);
        }
        // Once the prompt is sent, a stop is mid-turn.
        let mid_turn = self.prompt.is_none();
        match error {
            Some(detail) => TurnEnd::Failed(classify(self.engine, &detail, mid_turn), detail),
            None => TurnEnd::Failed(FailureClass::Unknown, UNFINISHED.into()),
        }
    }

    /// The turn's work is done: its reply, unless the follow-up is due,
    /// which is then sent and the turn goes on.
    fn complete(&mut self, session: &impl Link) -> Option<TurnEnd> {
        let due = self
            .follow_up
            .as_ref()
            .is_some_and(|follow_up| !(follow_up.accept)(&self.reply.text));
        if due && let Some(follow_up) = self.follow_up.take() {
            if let Err(error) = session.send(Command::Prompt(follow_up.prompt)) {
                return Some(local(format!("Cannot send the follow-up prompt: {error}")));
            }
            self.held = false;
            self.reply = Reply::default();
            return None;
        }
        Some(self.replied())
    }

    fn replied(&mut self) -> TurnEnd {
        TurnEnd::Replied {
            text: std::mem::take(&mut self.reply.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use octet_testkit::TempDir;

    use super::*;
    use crate::record::CandidateId;
    use crate::store::{self, RunFile};

    /// A session that plays `script`, then waits forever, and keeps every
    /// command sent to it.
    struct Script {
        events: VecDeque<Event>,
        sent: RefCell<Vec<Command>>,
    }

    impl Link for Script {
        async fn next(&mut self) -> Option<Event> {
            match self.events.pop_front() {
                Some(event) => Some(event),
                None => std::future::pending().await,
            }
        }

        fn send(&self, command: Command) -> Result<(), SendError> {
            self.sent.borrow_mut().push(command);
            Ok(())
        }
    }

    /// An approver that never answers, and counts how often it is asked.
    struct Silent(Arc<AtomicUsize>);

    impl Approver for Silent {
        fn ask<'a>(
            &'a mut self,
            _summary: &'a ApprovalSummary,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    fn approval(id: u64) -> Event {
        Event::Approval {
            id,
            detail: String::new(),
            summary: ApprovalSummary::new("Run a shell command"),
        }
    }

    #[tokio::test]
    async fn an_approval_closed_while_another_is_asked_is_never_asked() {
        let temp = TempDir::new("octet-fleet-step-unit");
        let run_file = RunFile {
            workflow: crate::sim::shipped(),
            hash: "0123456789abcdef".into(),
            task: "a task".into(),
            top: PathBuf::from("/work/repo"),
            cwd_rel: PathBuf::new(),
            base: CandidateId(0),
            base_tree: "t0".into(),
            octet_version: "0.0.0-test".into(),
        };
        let run = store::create(temp.path(), "f0000000001-abcd", &run_file)
            .await
            .expect("run");
        let (mut log, _) = Log::open(&run).await.expect("log");
        let workflow = crate::sim::shipped();
        let step = workflow
            .steps
            .iter()
            .find(|step| !step.read_only())
            .expect("a write step");
        let dispatch = Dispatch {
            step: 0,
            round: 1,
            input_candidate: CandidateId(0),
            requires: Vec::new(),
            history: Vec::new(),
            resume_session: None,
            not_before: None,
        };
        let plan = AttemptPlan {
            step,
            dispatch: &dispatch,
            attempt: AttemptId(0),
            prompt: "go".into(),
            cwd: temp.path().to_path_buf(),
            attempt_dir: temp.path().to_path_buf(),
            marker: "r/0".into(),
            data_dir: temp.path().to_path_buf(),
            interrupt: None,
            follow_up: None,
            pause_hook: None,
        };
        // Two requests wait; the engine closes both at once, the second
        // first, while the first is being asked.
        let mut session = Script {
            events: VecDeque::from([
                Event::Ready {
                    session: String::new(),
                },
                Event::Started,
                approval(1),
                approval(2),
                Event::ApprovalClosed(2),
                Event::ApprovalClosed(1),
                Event::Text("done".into()),
                Event::Finished {
                    outcome: Outcome::Completed,
                },
            ]),
            sent: RefCell::default(),
        };
        let asked = Arc::new(AtomicUsize::new(0));
        let mut approver = Silent(Arc::clone(&asked));
        let (tx, _rx) = mpsc::channel(64);
        let end = tokio::time::timeout(
            Duration::from_secs(5),
            Turn::new(&plan, VecDeque::new()).drive(&mut session, &mut log, &mut approver, &tx),
        )
        .await
        .expect("the attempt waited on a closed approval")
        .expect("no log error");
        assert_eq!(
            end,
            TurnEnd::Replied {
                text: "done".into()
            }
        );
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "asked about a closed request"
        );
        // Neither closed request is answered.
        assert_eq!(session.sent.into_inner(), [Command::Prompt("go".into())]);
    }

    /// A run log in `temp`, for a turn to append to.
    async fn log_in(temp: &TempDir) -> Log {
        let run_file = RunFile {
            workflow: crate::sim::shipped(),
            hash: "0123456789abcdef".into(),
            task: "a task".into(),
            top: PathBuf::from("/work/repo"),
            cwd_rel: PathBuf::new(),
            base: CandidateId(0),
            base_tree: "t0".into(),
            octet_version: "0.0.0-test".into(),
        };
        let run = store::create(temp.path(), "f0000000001-abcd", &run_file)
            .await
            .expect("run");
        Log::open(&run).await.expect("log").0
    }

    /// Drives a turn of the shipped workflow's step `step` over `events`,
    /// with `interrupt` and `follow_up` and an approver that never answers;
    /// its end and the commands sent.
    async fn turn_over(
        step: usize,
        events: Vec<Event>,
        interrupt: Option<Signals>,
        follow_up: Option<FollowUp<'_>>,
    ) -> (TurnEnd, Vec<Command>) {
        let temp = TempDir::new("octet-fleet-step-unit");
        let mut log = log_in(&temp).await;
        let workflow = crate::sim::shipped();
        let dispatch = Dispatch {
            step: 0,
            round: 1,
            input_candidate: CandidateId(0),
            requires: Vec::new(),
            history: Vec::new(),
            resume_session: None,
            not_before: None,
        };
        let plan = AttemptPlan {
            step: &workflow.steps[step],
            dispatch: &dispatch,
            attempt: AttemptId(0),
            prompt: "go".into(),
            cwd: temp.path().to_path_buf(),
            attempt_dir: temp.path().to_path_buf(),
            marker: "r/0".into(),
            data_dir: temp.path().to_path_buf(),
            interrupt,
            follow_up,
            pause_hook: None,
        };
        let mut session = Script {
            events: events.into(),
            sent: RefCell::default(),
        };
        let mut approver = Silent(Arc::new(AtomicUsize::new(0)));
        let (tx, _rx) = mpsc::channel(64);
        let end = tokio::time::timeout(
            Duration::from_secs(5),
            Turn::new(&plan, VecDeque::new()).drive(&mut session, &mut log, &mut approver, &tx),
        )
        .await
        .expect("the turn ended")
        .expect("no log error");
        (end, session.sent.into_inner())
    }

    fn turn_of(text: &str) -> [Event; 3] {
        [
            Event::Started,
            Event::Text(text.into()),
            Event::Finished {
                outcome: Outcome::Completed,
            },
        ]
    }

    #[tokio::test]
    async fn a_refused_reply_gets_its_follow_up_once() {
        let ready = Event::Ready {
            session: String::new(),
        };
        let accept = |text: &str| text == "good";
        let follow_up = || FollowUp {
            accept: &accept,
            prompt: "fix it".into(),
        };
        // Refused, then accepted.
        let events: Vec<Event> = std::iter::once(ready.clone())
            .chain(turn_of("bad"))
            .chain(turn_of("good"))
            .collect();
        let (end, sent) = turn_over(0, events, None, Some(follow_up())).await;
        assert_eq!(
            end,
            TurnEnd::Replied {
                text: "good".into()
            }
        );
        assert_eq!(
            sent,
            [
                Command::Prompt("go".into()),
                Command::Prompt("fix it".into())
            ]
        );
        // Refused twice: the second reply is the attempt's, with no third prompt.
        let events: Vec<Event> = std::iter::once(ready.clone())
            .chain(turn_of("bad"))
            .chain(turn_of("worse"))
            .collect();
        let (end, sent) = turn_over(0, events, None, Some(follow_up())).await;
        assert_eq!(
            end,
            TurnEnd::Replied {
                text: "worse".into()
            }
        );
        assert_eq!(sent.len(), 2);
        // Accepted at once: no follow-up.
        let events: Vec<Event> = std::iter::once(ready).chain(turn_of("good")).collect();
        let (end, sent) = turn_over(0, events, None, Some(follow_up())).await;
        assert_eq!(
            end,
            TurnEnd::Replied {
                text: "good".into()
            }
        );
        assert_eq!(sent, [Command::Prompt("go".into())]);
    }

    #[tokio::test]
    async fn the_run_interrupt_ends_the_turn() {
        let (signal, signals) = tokio::sync::watch::channel(None);
        let events = vec![
            Event::Ready {
                session: String::new(),
            },
            Event::Started,
            Event::Text("partial".into()),
        ];
        let interrupting = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            signal.send(Some("SIGTERM")).expect("the turn listens");
        };
        let ((end, _), ()) = tokio::join!(turn_over(0, events, Some(signals), None), interrupting);
        assert_eq!(end, TurnEnd::Interrupted);
    }

    #[tokio::test]
    async fn a_signal_sent_before_the_turn_waits_ends_it() {
        let (signal, signals) = tokio::sync::watch::channel(None);
        signal.send(Some("SIGINT")).expect("the turn listens");
        let events = vec![
            Event::Ready {
                session: String::new(),
            },
            Event::Started,
        ];
        let (end, _) = turn_over(0, events, Some(signals), None).await;
        assert_eq!(end, TurnEnd::Interrupted);
    }

    #[tokio::test]
    async fn the_run_interrupt_ends_an_approval_being_asked() {
        let workflow = crate::sim::shipped();
        let write = workflow
            .steps
            .iter()
            .position(|step| !step.read_only())
            .expect("a write step");
        let (signal, signals) = tokio::sync::watch::channel(None);
        let events = vec![
            Event::Ready {
                session: String::new(),
            },
            Event::Started,
            approval(1),
        ];
        let interrupting = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            signal.send(Some("SIGINT")).expect("the turn listens");
        };
        let ((end, sent), ()) =
            tokio::join!(turn_over(write, events, Some(signals), None), interrupting);
        assert_eq!(end, TurnEnd::Interrupted);
        // The request is left unanswered.
        assert_eq!(sent, [Command::Prompt("go".into())]);
    }

    #[test]
    fn text_after_a_tool_is_a_new_paragraph() {
        let mut reply = Reply::default();
        reply.tool();
        reply.push("First");
        reply.push(" line");
        reply.tool();
        reply.push("");
        reply.push("After\n");
        reply.tool();
        reply.push("Last");
        assert_eq!(reply.text, "First line\n\nAfter\n\nLast");
    }
}
