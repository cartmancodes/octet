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
//!
//! Log appends are awaited in the loop's bodies, never inside a `select!`:
//! [`Log::append`] is not cancel-safe.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use octet_core::background::Background;
use octet_core::{ApprovalSummary, Command, Config, Engine, Event, Mode, Outcome, Session};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::FleetEvent;
use crate::classify::{FailureClass, classify};
use crate::gate;
use crate::machine::Dispatch;
use crate::ownership;
use crate::record::{AttemptId, Identity, Record};
use crate::store::{Log, StoreError};
use crate::validate::Step;

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
    /// The run's own interrupt ended the turn. The runner's interrupt hook
    /// (Task 14) produces it; nothing in this module does yet.
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
    let identities = match register(session, &plan.attempt_dir, &mut early).await {
        Ok(identities) => identities,
        Err(detail) => return Ok(local(detail)),
    };
    let record = Record::AttemptSession {
        attempt: plan.attempt,
        journal: session.journal().to_path_buf(),
        identities,
    };
    log.append(&record).await?;
    // Only now may the vendor run.
    if let Err(error) = gate::release(&plan.attempt_dir).await {
        return Ok(local(format!("Cannot release the vendor: {error}")));
    }
    Turn::new(plan, early)
        .drive(session, log, approver, events)
        .await
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

/// Waits for the gate to register at least one PID and reads each one's
/// identity. Session events that arrive meanwhile are kept in `early`, in
/// order. Any failure leaves the gate held.
async fn register(
    session: &mut Session,
    dir: &Path,
    early: &mut VecDeque<Event>,
) -> Result<Vec<Identity>, String> {
    let deadline = Instant::now() + REGISTRATION;
    let pids = loop {
        let pids = gate::read_processes(dir)
            .await
            .map_err(|error| format!("Cannot read the vendor's registration: {error}"))?;
        if !pids.is_empty() {
            break pids;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "The vendor gate did not register within {} s",
                REGISTRATION.as_secs()
            ));
        }
        tokio::select! {
            event = session.events.recv() => {
                let Some(event) = event else {
                    return Err(ended_before_registration(early));
                };
                early.push_back(event);
            }
            () = tokio::time::sleep(REGISTRATION_POLL) => {}
        }
    };
    let mut identities = Vec::with_capacity(pids.len());
    for pid in pids {
        let Some(start) = ownership::start_time(pid).await else {
            return Err(format!("Cannot read the start time of process {pid}"));
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

/// One attempt's turn, from `Ready` to its end.
struct Turn<'p> {
    attempt: AttemptId,
    engine: &'p str,
    read_only: bool,
    /// The prompt, until it is sent.
    prompt: Option<String>,
    /// Events read but not yet handled.
    queue: VecDeque<Event>,
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
        session: &mut Session,
        log: &mut Log,
        approver: &mut dyn Approver,
        events: &mpsc::Sender<FleetEvent>,
    ) -> Result<TurnEnd, StepError> {
        loop {
            let event = if let Some(event) = self.queue.pop_front() {
                Some(event)
            } else {
                tokio::select! {
                    event = session.events.recv() => event,
                    () = self.background.grace_over(), if self.held => {
                        if !self.background.pending() {
                            return Ok(self.replied());
                        }
                        continue;
                    }
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
        session: &mut Session,
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
                    && let Err(error) = session.handle.send(Command::Prompt(prompt))
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
            Event::Approval { id, summary, .. } => {
                let allow = if self.read_only {
                    Some(false)
                } else {
                    self.ask(approver, &summary, id, session).await
                };
                // An answer that cannot be delivered leaves the request to
                // time out, which denies it.
                if let Some(allow) = allow {
                    let _ = session.handle.send(Command::Answer { id, allow });
                }
            }
            Event::Finished { outcome } => {
                self.running = false;
                return Ok(self.finished(&outcome));
            }
            Event::Stopped => return Ok(Some(self.stopped())),
            _ => {}
        }
        Ok(None)
    }

    /// Asks the approver while still reading the session, so a slow answer
    /// cannot stall it. `None` when the request closed (timed out or
    /// withdrawn) or the session ended first: there is nothing to answer.
    async fn ask(
        &mut self,
        approver: &mut dyn Approver,
        summary: &ApprovalSummary,
        id: u64,
        session: &mut Session,
    ) -> Option<bool> {
        let mut asking = approver.ask(summary);
        loop {
            tokio::select! {
                allow = &mut asking => return Some(allow),
                event = session.events.recv() => match event {
                    Some(event) => {
                        let closed = matches!(event, Event::ApprovalClosed(closed) if closed == id);
                        self.queue.push_back(event);
                        if closed {
                            return None;
                        }
                    }
                    None => return None,
                },
            }
        }
    }

    /// A turn ended: the reply, unless background work may start another
    /// turn, or the turn's failure.
    fn finished(&mut self, outcome: &Outcome) -> Option<TurnEnd> {
        match outcome {
            Outcome::Completed if self.background.pending() => {
                self.held = true;
                None
            }
            Outcome::Completed => Some(self.replied()),
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

    fn replied(&mut self) -> TurnEnd {
        TurnEnd::Replied {
            text: std::mem::take(&mut self.reply.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
