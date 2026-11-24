//! The runner: drives a run from its log to its end, a pause, a failure or
//! an interrupt.
//!
//! Every entry point ([`start`], [`resume`], [`stop`]) takes the run's lock
//! and the work tree's, reconciles the work tree's ownership record, and
//! then loops: replay the log, observe the live tree, the latest attempt's
//! `go` file and the leftover processes, ask [`recover`] what comes next, act
//! on it, append what happened, and go round again. Acting is all I/O; every
//! decision about what to do is [`crate::machine`]'s and [`recover`]'s.
//!
//! Records are appended with `.await` in the loop's bodies, never inside a
//! `select!` ([`Log::append`] is not cancel-safe). Signals race only against
//! a vendor turn (through [`AttemptPlan::interrupt`]), an evidence command
//! (through [`crate::evidence::EvidenceRun::interrupt`]) and a backoff wait.
//!
//! [`AttemptPlan::interrupt`]: crate::step::AttemptPlan::interrupt

mod act;
mod decide;
mod evidence;
mod status;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};

pub use status::{RunSummary, status};

use crate::FleetEvent;
use crate::gate;
use crate::machine::Next;
use crate::ownership::{self, Held, OwnershipError, Unresolved};
use crate::record::{AttemptId, CandidateId, Decision, Identity, Origin, PauseReason, Record};
use crate::recover::{self, Observation, ProcessCheck};
use crate::snapshot::{self, Repo, SnapshotError};
use crate::state::{AttemptState, ReplayError, RunState, replay};
use crate::step::{Approver, StepError};
use crate::store::{self, Log, RunDir, RunFile, StoreError};
use crate::validate::Resolved;
use crate::{Signals, signal_now, signalled};

/// The most diffstat lines a `Drift` pause shows.
const DIFFSTAT_LINES: usize = 40;

/// What a new run is started with.
#[derive(Debug, Clone)]
pub struct Start {
    /// The validated workflow.
    pub workflow: Resolved,
    /// The workflow's hash, for `run.json`.
    pub hash: String,
    /// The user's task.
    pub task: String,
    /// The directory the run starts in, inside the work tree.
    pub cwd: PathBuf,
    /// Octet's data directory; runs live in `<data_dir>/fleet/`.
    pub data_dir: PathBuf,
}

/// The clock retries are timed by, in seconds since the Unix epoch. Real
/// runs use [`Clock::system`]; a test injects one that follows paused tokio
/// time.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl Clock {
    /// The system clock.
    #[must_use]
    pub fn system() -> Self {
        Self(Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }))
    }

    /// A clock that reads `now`.
    pub fn new(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    /// The time now, in seconds since the Unix epoch.
    #[must_use]
    pub fn now(&self) -> u64 {
        (self.0)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Clock").field(&self.now()).finish()
    }
}

/// What the runner talks to: the front end's event stream, its answers to
/// write steps' approvals, the process's signals, and the clock.
pub struct RunnerIo {
    /// Where the run's events go. Must be read while the run goes on.
    pub events: mpsc::Sender<FleetEvent>,
    /// Answers write steps' approvals; read-only steps never ask it.
    pub approver: Box<dyn Approver>,
    /// `Some("SIGINT")` or `Some("SIGTERM")` once the process got one.
    pub signals: Signals,
    /// The clock retries are timed by.
    pub clock: Clock,
}

impl RunnerIo {
    /// The system clock and the given channels.
    #[must_use]
    pub fn new(
        events: mpsc::Sender<FleetEvent>,
        approver: Box<dyn Approver>,
        signals: Signals,
    ) -> Self {
        Self {
            events,
            approver,
            signals,
            clock: Clock::system(),
        }
    }
}

impl std::fmt::Debug for RunnerIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerIo")
            .field("signals", &*self.signals.borrow())
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

/// How a call into the runner ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// The run completed: the invariant holds.
    Completed,
    /// The run waits for the user (or, for an interrupt, for `resume`).
    Paused(PauseReason),
    /// The run failed, with its error.
    Failed(String),
    /// A signal interrupted the run: `SIGINT` or `SIGTERM`.
    Interrupted(&'static str),
    /// The user stopped the run.
    Stopped,
}

/// Why the runner could not go on. A vendor's or Octet's failure inside an
/// attempt is logged and paused on, not an error; these are what the log
/// cannot record.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The run store failed: the log cannot be read or appended.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A lock, the worktree record or the process list failed, or another
    /// run owns the work tree.
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    /// The work tree cannot be opened, or a run's ref cannot be made.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// The log contradicts the workflow.
    #[error(transparent)]
    Replay(#[from] ReplayError),
    /// An attempt could not log its session.
    #[error(transparent)]
    Step(#[from] StepError),
    /// A decision or a `stop` that the run cannot take now, and why.
    #[error("{0}")]
    Refused(String),
}

/// Starts a new run of `start.workflow` on the work tree `start.cwd` is in.
///
/// The base candidate is observed first, then the run directory made and
/// its locks taken; when the work tree is another run's, the new run
/// directory is removed again and the refusal returned.
///
/// # Errors
///
/// [`RunnerError::Ownership`] when another run holds or owns the work tree;
/// [`SnapshotError::DataDirInTree`] when the data directory is in the work
/// tree and not ignored (nothing is made then); the other variants when the
/// store, git or the log fail.
pub async fn start(start: Start, io: RunnerIo) -> Result<RunEnd, RunnerError> {
    let repo = snapshot::open_repo(&start.cwd).await?;
    snapshot::check_data_dir(&repo, &start.data_dir).await?;
    let hash = ownership::worktree_hash(&repo.top).await?;
    let id = store::new_run_id();
    let fleet = start.data_dir.join("fleet");
    octet_store::create_private_dir(&fleet)
        .await
        .map_err(StoreError::from)?;
    let scratch = fleet.join(format!("{id}.index.tmp"));
    let base_tree = snapshot::take(&repo, &scratch).await;
    let _ = tokio::fs::remove_file(&scratch).await;
    let base_tree = base_tree?;
    let file = RunFile {
        workflow: start.workflow,
        hash: start.hash,
        task: start.task,
        top: repo.top.clone(),
        cwd_rel: repo.cwd_rel.clone(),
        base: CandidateId(0),
        base_tree: base_tree.clone(),
        octet_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let dir = store::create(&start.data_dir, &id, &file).await?;
    let driver = Driver::open(&start.data_dir, &id, dir.clone(), file, repo, &hash, io).await;
    let mut driver = match driver {
        Ok(driver) => driver,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&dir.root).await;
            return Err(error);
        }
    };
    if let Err(error) = driver.admit().await {
        drop(driver);
        let _ = tokio::fs::remove_dir_all(&dir.root).await;
        return Err(error);
    }
    snapshot::keep(&driver.repo, &id, 0, &base_tree).await?;
    driver
        .append(&Record::Candidate {
            id: CandidateId(0),
            tree: base_tree,
            parent: None,
            origin: Origin::Base,
        })
        .await?;
    // Boxed: the loop's future is large, and the caller's stays small.
    Box::pin(driver.drive(None)).await
}

/// Continues run `run`, first taking `decision` if one is given (it must be
/// one the run's pause offers; [`Decision::Adopt`]'s candidate is assigned
/// here, so any value may be passed).
///
/// # Errors
///
/// [`RunnerError::Refused`] for a decision the run cannot take now;
/// [`RunnerError::Ownership`] when another Octet drives the run or another
/// run owns the work tree; the other variants when the store, git or the log
/// fail.
pub async fn resume(
    data_dir: &Path,
    run: &str,
    decision: Option<Decision>,
    io: RunnerIo,
) -> Result<RunEnd, RunnerError> {
    let (dir, file) = store::open(data_dir, run).await?;
    let mut repo = snapshot::open_repo(&file.top).await?;
    repo.cwd_rel.clone_from(&file.cwd_rel);
    let hash = ownership::worktree_hash(&file.top).await?;
    let driver = Driver::open(data_dir, run, dir, file, repo, &hash, io).await?;
    let view = driver.load().await?;
    if let Some(end) = ended(&view.state) {
        if decision.is_some() {
            return Err(RunnerError::Refused(format!(
                "run {run} has ended; it takes no decision"
            )));
        }
        return Ok(end);
    }
    // `stop` writes nothing to the tree, so another run's claim does not
    // stand in its way.
    if decision != Some(Decision::Stop) {
        driver.admit().await?;
    }
    Box::pin(driver.drive(decision)).await
}

/// Stops run `run`: `resume` with [`Decision::Stop`]. Refused while a
/// verified process of the run is alive or an unverified one is not
/// released; the tree is left as it is.
///
/// # Errors
///
/// As [`resume`].
pub async fn stop(data_dir: &Path, run: &str) -> Result<RunEnd, RunnerError> {
    let (events, _) = mpsc::channel(1);
    let (_, signals) = watch::channel(None);
    let io = RunnerIo::new(events, Box::new(Deny), signals);
    resume(data_dir, run, Some(Decision::Stop), io).await
}

/// The approver of a `stop`, which runs no attempt.
struct Deny;

impl Approver for Deny {
    fn ask<'a>(
        &'a mut self,
        _summary: &'a octet_core::ApprovalSummary,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async { false })
    }
}

/// The run's end, when its log says it ended.
fn ended(state: &RunState) -> Option<RunEnd> {
    if state.completed {
        Some(RunEnd::Completed)
    } else if state.stopped {
        Some(RunEnd::Stopped)
    } else {
        state.failed.clone().map(RunEnd::Failed)
    }
}

/// The log, replayed.
struct View {
    records: Vec<Record>,
    state: RunState,
}

/// What a step of the loop leads to: go round again, or end the call.
type Flow = Option<RunEnd>;

/// One run being driven, its locks held.
struct Driver {
    data_dir: PathBuf,
    id: String,
    dir: RunDir,
    file: RunFile,
    def: Arc<Resolved>,
    repo: Repo,
    /// The work tree's hash, naming its lock and ownership record.
    hash: String,
    held: Held,
    log: Log,
    io: RunnerIo,
}

impl Driver {
    /// Takes the run's locks and opens its log.
    async fn open(
        data_dir: &Path,
        id: &str,
        dir: RunDir,
        file: RunFile,
        repo: Repo,
        hash: &str,
        io: RunnerIo,
    ) -> Result<Self, RunnerError> {
        let held = ownership::lock(data_dir, &dir, hash).await?;
        let (log, _) = Log::open(&dir).await?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            id: id.to_owned(),
            def: Arc::new(file.workflow.clone()),
            dir,
            file,
            repo,
            hash: hash.to_owned(),
            held,
            log,
            io,
        })
    }

    /// Reads and replays the log.
    async fn load(&self) -> Result<View, RunnerError> {
        let records = store::read_log(&self.dir).await?;
        let metas = store::read_artifact_metas(&self.dir).await?;
        let state = replay(&self.def, &records, &metas)?;
        Ok(View { records, state })
    }

    async fn append(&mut self, record: &Record) -> Result<(), RunnerError> {
        self.log.append(record).await?;
        Ok(())
    }

    async fn emit(&self, event: FleetEvent) {
        // Nobody listening is not a reason to stop the run.
        let _ = self.io.events.send(event).await;
    }

    /// The loop. `decision` is taken first, on the first observation.
    async fn drive(mut self, mut decision: Option<Decision>) -> Result<RunEnd, RunnerError> {
        self.emit(FleetEvent::RunStarted {
            run: self.id.clone(),
        })
        .await;
        loop {
            let view = self.load().await?;
            if let Some(end) = ended(&view.state) {
                return Ok(end);
            }
            if let Some(signal) = signal_now(Some(&self.io.signals)) {
                return self.interrupted(None, signal).await;
            }
            let obs = match self.observe(&view).await {
                Ok(obs) => obs,
                Err(detail) => return self.pause(PauseReason::LocalFailure, detail).await,
            };
            if let Some(decision) = decision.take() {
                if let Some(end) = self.decide(&view, &obs, decision).await? {
                    return Ok(end);
                }
                continue;
            }
            if let Some(end) = self.act(&view, &obs).await? {
                return Ok(end);
            }
        }
    }

    /// Acts on what [`recover`] says comes next.
    async fn act(&mut self, view: &View, obs: &Observation) -> Result<Flow, RunnerError> {
        match recover::recover(&self.def, &view.state, obs) {
            Next::Done => Ok(ended(&view.state)),
            Next::Fail(error) => {
                self.append(&Record::Failed {
                    error: error.clone(),
                })
                .await?;
                self.disown().await?;
                Ok(Some(RunEnd::Failed(error)))
            }
            Next::Pause(reason, detail) => self.pause(reason, detail).await.map(Some),
            Next::Run(dispatch) => {
                if let Some(at) = dispatch.not_before
                    && self.io.clock.now() < at
                {
                    return self.back_off(view, at).await;
                }
                let kept = view.state.pending_pause == Some(PauseReason::Reconcile)
                    && view.state.decisions_since_pause.last() == Some(&Decision::Keep);
                self.attempt(view, dispatch, kept).await
            }
            Next::Evidence {
                attempt,
                candidate,
                restart,
            } => self.evidence(view, attempt, candidate, restart).await,
            Next::FinishAdopted { attempt, candidate } => {
                self.finish_adopted(view, attempt, candidate).await
            }
            Next::ReadyToComplete {
                candidate,
                tree,
                evidence,
                approval,
            } => self.complete(candidate, tree, evidence, approval).await,
        }
    }

    /// Waits until `at` for a transient retry, unless a signal comes first.
    async fn back_off(&mut self, view: &View, at: u64) -> Result<Flow, RunnerError> {
        if let Some((&attempt, _)) = view.state.attempts.iter().next_back() {
            self.emit(FleetEvent::Retrying { attempt, at }).await;
        }
        let mut signals = Some(self.io.signals.clone());
        match wait_until(&self.io.clock, at, &mut signals).await {
            Some(signal) => self.interrupted(None, signal).await.map(Some),
            None => Ok(None),
        }
    }

    /// The live tree, the latest attempt's `go` file and the leftover
    /// processes of the run's unfinished and failed attempts. An error is a
    /// local failure, as text.
    async fn observe(&self, view: &View) -> Result<Observation, String> {
        let live_tree = self
            .live_tree()
            .await
            .map_err(|error| format!("cannot observe the work tree: {error}"))?;
        let go_created = match view.state.attempts.keys().next_back() {
            Some(&attempt) => tokio::fs::try_exists(self.attempt_dir(attempt).join("go"))
                .await
                .unwrap_or(false),
            None => false,
        };
        let processes = self
            .leftovers(view)
            .await
            .map_err(|error| format!("cannot check the run's processes: {error}"))?;
        let mut commands = std::collections::BTreeMap::new();
        if let ProcessCheck::Verified(pids) | ProcessCheck::Unverified(pids) = &processes {
            for &pid in pids {
                if let Some(command) = ownership::command_of(pid).await {
                    commands.insert(pid, command);
                }
            }
        }
        Ok(Observation {
            live_tree,
            go_created,
            processes,
            commands,
        })
    }

    /// A fresh snapshot of the live work tree.
    async fn live_tree(&self) -> Result<String, SnapshotError> {
        snapshot::take(&self.repo, &self.dir.root.join("index.tmp")).await
    }

    /// Which live processes are the run's (spec, "Leftover processes"): those
    /// of every attempt not accepted, by its marker, its registered
    /// identities and their groups, less the PIDs the user released.
    ///
    /// The PIDs in `<attempt>/processes` count too (a launch after `go`, or a
    /// crash between the gate's registration and `attempt_session`): as
    /// groups, not identities. Their start times were never recorded, so a
    /// PID reused since is indistinguishable from the gate; a live process
    /// there without the marker is reported unverified and never signalled.
    /// An unreadable registry is an error, never an empty one.
    async fn leftovers(&self, view: &View) -> Result<ProcessCheck, String> {
        let open: Vec<AttemptId> = view
            .state
            .attempts
            .iter()
            .filter(|(_, info)| info.state != AttemptState::Finished)
            .map(|(&attempt, _)| attempt)
            .collect();
        if open.is_empty() {
            return Ok(ProcessCheck::Clear);
        }
        let processes = ownership::list_processes()
            .await
            .map_err(|error| error.to_string())?;
        let released: Vec<u32> = view.state.released_pids.iter().copied().collect();
        let (mut verified, mut unverified) = (Vec::new(), Vec::new());
        for attempt in open {
            let identities = identities_of(&view.records, attempt);
            let mut groups: Vec<u32> = identities.iter().map(|identity| identity.pid).collect();
            let registry = gate::read_processes(&self.attempt_dir(attempt))
                .await
                .map_err(|error| format!("attempt {}'s registry: {error}", attempt.0))?;
            groups.extend(registry);
            groups.sort_unstable();
            groups.dedup();
            let marker = self.marker(attempt);
            match ownership::check(&processes, &marker, &identities, &groups, &released) {
                ProcessCheck::Clear => {}
                ProcessCheck::Verified(pids) => verified.extend(pids),
                ProcessCheck::Unverified(pids) => unverified.extend(pids),
            }
        }
        verified.sort_unstable();
        verified.dedup();
        unverified.sort_unstable();
        unverified.dedup();
        Ok(if !verified.is_empty() {
            ProcessCheck::Verified(verified)
        } else if !unverified.is_empty() {
            ProcessCheck::Unverified(unverified)
        } else {
            ProcessCheck::Clear
        })
    }

    /// Pauses for `reason`, logged as [`logs_pause`] says. A pause already
    /// logged keeps its logged detail, except a processes pause, whose
    /// detail is the observation just made (the processes alive now). A
    /// local failure that is not logged leaves the pending pause in force:
    /// the user is asked about it, or, when it already has its decision, to
    /// `retry` (the decision then stands).
    ///
    /// A local failure while a signal is pending is the signal's doing: the
    /// run is interrupted instead.
    async fn pause(&mut self, reason: PauseReason, detail: String) -> Result<RunEnd, RunnerError> {
        if reason == PauseReason::LocalFailure
            && let Some(signal) = signal_now(Some(&self.io.signals))
        {
            return self.interrupted(None, signal).await;
        }
        let view = self.load().await?;
        let pending = view.state.pending_pause;
        let detail = if pending != Some(reason) && reason == PauseReason::Drift {
            self.with_diffstat(detail).await
        } else {
            detail
        };
        let detail = if pending == Some(reason) {
            shown_detail(reason, detail, last_pause_detail(&view.records))
        } else if !logs_pause(pending, reason) {
            detail
        } else {
            self.append(&Record::Paused {
                reason,
                detail: detail.clone(),
            })
            .await?;
            detail
        };
        self.emit(FleetEvent::Paused { reason, detail }).await;
        let asked = asked_about(&view.state, reason);
        if asked != PauseReason::Interrupted {
            self.emit(FleetEvent::NeedsDecision {
                reason: asked,
                options: recover::options(asked),
            })
            .await;
        }
        Ok(RunEnd::Paused(asked))
    }

    /// A drift detail with the diffstat between its two trees added before
    /// its last line (the command for the whole change), at most
    /// [`DIFFSTAT_LINES`] lines of it.
    async fn with_diffstat(&self, detail: String) -> String {
        let Some((expected, live)) = recover::drift_trees(&detail) else {
            return detail;
        };
        let stat = match snapshot::diff(&self.repo, expected, live, true).await {
            Ok(stat) => {
                let lines: Vec<&str> = stat.lines().collect();
                if lines.len() > DIFFSTAT_LINES {
                    let summary = lines.last().copied().unwrap_or_default();
                    let more = lines.len() - DIFFSTAT_LINES;
                    format!(
                        "{}\n ... {more} more lines\n{summary}",
                        lines[..DIFFSTAT_LINES - 1].join("\n")
                    )
                } else {
                    lines.join("\n")
                }
            }
            Err(error) => format!("(no diffstat: {error})"),
        };
        match detail.rsplit_once('\n') {
            Some((head, command)) if !stat.is_empty() => format!("{head}\n{stat}\n{command}"),
            _ => detail,
        }
    }

    /// A signal ended the run's work: `attempt`, if one ran (its turn or an
    /// evidence command), is interrupted (not finished), and the run pauses.
    /// With nothing running and a pause pending, nothing is logged: that
    /// pause, and any decision already taken at it, stands for the next
    /// `resume`; an interrupt pause would lift it unanswered.
    async fn interrupted(
        &mut self,
        attempt: Option<AttemptId>,
        signal: &'static str,
    ) -> Result<RunEnd, RunnerError> {
        if let Some(attempt) = attempt {
            self.append(&Record::AttemptInterrupted {
                attempt,
                signal: signal.to_owned(),
            })
            .await?;
        } else if self.load().await?.state.pending_pause.is_some() {
            return Ok(RunEnd::Interrupted(signal));
        }
        let detail = format!("{signal} interrupted the run; resume it to go on");
        self.append(&Record::Paused {
            reason: PauseReason::Interrupted,
            detail: detail.clone(),
        })
        .await?;
        self.emit(FleetEvent::Paused {
            reason: PauseReason::Interrupted,
            detail,
        })
        .await;
        Ok(RunEnd::Interrupted(signal))
    }

    /// The signal the run got, if any; a turn that ended interrupted ended
    /// for it.
    fn signal(&self) -> &'static str {
        signal_now(Some(&self.io.signals)).unwrap_or("SIGINT")
    }

    fn attempt_dir(&self, attempt: AttemptId) -> PathBuf {
        store::attempt_dir(&self.dir, attempt)
    }

    /// The `OCTET_FLEET_ATTEMPT` value of `attempt`.
    fn marker(&self, attempt: AttemptId) -> String {
        format!("{}/{}", self.id, attempt.0)
    }

    /// The next candidate ID: past every candidate ever proposed.
    fn next_candidate(view: &View) -> CandidateId {
        let last = view
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Candidate { id, .. } => Some(id.0),
                _ => None,
            })
            .max();
        CandidateId(last.map_or(0, |id| id + 1))
    }

    /// Proposes `tree` as a candidate: its ref first, then its record.
    async fn propose(
        &mut self,
        view: &View,
        tree: &str,
        parent: CandidateId,
        origin: Origin,
    ) -> Result<CandidateId, RunnerError> {
        let id = Self::next_candidate(view);
        snapshot::keep(&self.repo, &self.id, id.0, tree).await?;
        self.append(&Record::Candidate {
            id,
            tree: tree.to_owned(),
            parent: Some(parent),
            origin,
        })
        .await?;
        self.emit(FleetEvent::Candidate { id, origin }).await;
        Ok(id)
    }

    /// Reconciles the work tree's ownership record and admits this run as
    /// its writer. A listed run whose log ended and none of whose processes
    /// is alive holds nothing, and is dropped from the record.
    async fn admit(&self) -> Result<(), RunnerError> {
        let mut record = ownership::load(&self.data_dir, &self.hash).await?;
        let others: Vec<(String, Unresolved)> = record
            .runs
            .iter()
            .filter(|(run, _)| **run != self.id)
            .map(|(run, unresolved)| (run.clone(), unresolved.clone()))
            .collect();
        let mut changed = false;
        for (run, unresolved) in others {
            if self.settled(&run, &unresolved).await? {
                record.runs.remove(&run);
                changed = true;
            }
        }
        if changed {
            ownership::save(&self.held, &self.data_dir, &self.hash, &record).await?;
        }
        ownership::admit(&record, &self.id)?;
        Ok(())
    }

    /// Whether listed run `run` has ended (or is gone) and left no process.
    async fn settled(&self, run: &str, unresolved: &Unresolved) -> Result<bool, RunnerError> {
        let ended = match store::open(&self.data_dir, run).await {
            Ok((dir, _)) => store::read_log(&dir).await?.iter().any(|record| {
                matches!(
                    record,
                    Record::Completed { .. } | Record::Stopped | Record::Failed { .. }
                )
            }),
            Err(StoreError::NoSuchRun(_)) => true,
            Err(error) => return Err(error.into()),
        };
        if !ended {
            return Ok(false);
        }
        let processes = ownership::list_processes().await?;
        let marker = format!("{run}/{}", unresolved.attempt.0);
        let groups: Vec<u32> = unresolved.identities.iter().map(|i| i.pid).collect();
        let check = ownership::check(&processes, &marker, &unresolved.identities, &groups, &[]);
        Ok(check == ProcessCheck::Clear)
    }

    /// Records `attempt` as this run's unresolved claim on the work tree,
    /// with the identities the log registered for it so far.
    async fn own(&self, attempt: AttemptId) -> Result<(), RunnerError> {
        let records = store::read_log(&self.dir).await?;
        let mut record = ownership::load(&self.data_dir, &self.hash).await?;
        record.runs.insert(
            self.id.clone(),
            Unresolved {
                attempt,
                identities: identities_of(&records, attempt),
            },
        );
        ownership::save(&self.held, &self.data_dir, &self.hash, &record).await?;
        Ok(())
    }

    /// Updates this run's claim on the work tree when write attempt
    /// `attempt` has ended. Accepted, with none of its processes alive, it
    /// leaves nothing unresolved (spec, "The worktree ownership record"): the
    /// run is dropped from the record, so a run paused between attempts does
    /// not hold the work tree. Otherwise the claim names the attempt and the
    /// identities registered for it.
    async fn settle(&self, attempt: AttemptId) -> Result<(), RunnerError> {
        let view = self.load().await?;
        let accepted = view
            .state
            .attempts
            .get(&attempt)
            .is_some_and(|info| info.state == AttemptState::Finished);
        if accepted && self.gone(&view.records, attempt).await? {
            return self.disown().await;
        }
        self.own(attempt).await
    }

    /// Whether no process of `attempt` is alive, as [`nothing_left`] says.
    /// An unreadable registry is not known to be empty.
    async fn gone(&self, records: &[Record], attempt: AttemptId) -> Result<bool, RunnerError> {
        let Ok(registry) = gate::read_processes(&self.attempt_dir(attempt)).await else {
            return Ok(false);
        };
        let processes = ownership::list_processes().await?;
        Ok(nothing_left(
            records,
            attempt,
            &registry,
            &processes,
            &self.marker(attempt),
        ))
    }

    /// Removes this run from the work tree's ownership record.
    async fn disown(&self) -> Result<(), RunnerError> {
        let mut record = ownership::load(&self.data_dir, &self.hash).await?;
        if record.runs.remove(&self.id).is_some() {
            ownership::save(&self.held, &self.data_dir, &self.hash, &record).await?;
        }
        Ok(())
    }
}

/// The longest one sleep of a backoff lasts before the clock is read again.
const BACKOFF_SLICE: Duration = Duration::from_secs(60);

/// Waits until `clock` reads `at` or later; the signal `signals` got first,
/// if one did. It sleeps in slices of at most [`BACKOFF_SLICE`], reading the
/// clock after each: a sleep does not count the time a machine is suspended,
/// and the wall clock does.
async fn wait_until(clock: &Clock, at: u64, signals: &mut Option<Signals>) -> Option<&'static str> {
    loop {
        let now = clock.now();
        if now >= at {
            return None;
        }
        let wait = Duration::from_secs(at - now).min(BACKOFF_SLICE);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            signal = signalled(signals) => return Some(signal),
        }
    }
}

/// Every identity the log registered for `attempt`: its vendor's and its
/// evidence commands'.
fn identities_of(records: &[Record], attempt: AttemptId) -> Vec<Identity> {
    let mut identities = Vec::new();
    for record in records {
        match record {
            Record::AttemptSession {
                attempt: a,
                identities: ids,
                ..
            } if *a == attempt => identities.extend(ids.iter().cloned()),
            Record::EvidenceCommand {
                attempt: a,
                identity,
                ..
            } if *a == attempt => identities.push(identity.clone()),
            _ => {}
        }
    }
    identities
}

/// Whether a pause for `reason` is appended while the log waits on
/// `pending`. Not the same pause twice; and a processes pause never replaces
/// a pause that waits for a decision, which lifting it would drop. One the
/// next observation answers (an interrupt, a processes pause) it does
/// replace: a run that waits on unverified processes once the verified ones
/// are killed says so in its log.
///
/// A local failure (an observation or a clone that failed) never replaces a
/// pause that waits for a decision either: that pause, and any decision
/// already logged at it, stands, and the next `resume` observes again.
fn logs_pause(pending: Option<PauseReason>, reason: PauseReason) -> bool {
    match pending {
        Some(pending) if pending == reason => false,
        Some(pending) if processes_pause(reason) || reason == PauseReason::LocalFailure => {
            answered(pending)
        }
        _ => true,
    }
}

/// The detail shown for a pause already logged for `reason`: the logged one,
/// except for a processes pause, whose `fresh` detail lists the processes
/// alive now (a `release` acts on those, not on the ones first logged).
fn shown_detail(reason: PauseReason, fresh: String, logged: Option<String>) -> String {
    if processes_pause(reason) {
        fresh
    } else {
        logged.unwrap_or(fresh)
    }
}

/// A pause the next observation answers, without a decision.
fn answered(reason: PauseReason) -> bool {
    reason == PauseReason::Interrupted || processes_pause(reason)
}

fn processes_pause(reason: PauseReason) -> bool {
    matches!(
        reason,
        PauseReason::ProcessesRunning | PauseReason::ProcessesUnverified
    )
}

/// The pause the user is asked about when the run pauses for `reason`:
/// `reason`, except a local failure that was not logged over a pending
/// pause ([`logs_pause`]): then the pending pause, unless it has its
/// decision already, when `retry` (which lets that decision stand) is asked.
fn asked_about(state: &RunState, reason: PauseReason) -> PauseReason {
    match state.pending_pause {
        Some(pending)
            if reason == PauseReason::LocalFailure && !logs_pause(Some(pending), reason) =>
        {
            if decided(state) {
                PauseReason::LocalFailure
            } else {
                pending
            }
        }
        _ => reason,
    }
}

/// The pending pause has a decision that ends its wait.
pub(super) fn decided(state: &RunState) -> bool {
    state.pending_pause.is_some()
        && state
            .decisions_since_pause
            .last()
            .is_some_and(crate::machine::ends_the_wait)
}

/// Whether none of `processes` is `attempt`'s: none carries its `marker`,
/// matches an identity the log registered for it, or is in a registered
/// group (those identities' and the gate `registry`'s), leaving out every
/// PID the log released: released processes are ignored from then on (spec,
/// "Leftover processes").
fn nothing_left(
    records: &[Record],
    attempt: AttemptId,
    registry: &[u32],
    processes: &[ownership::ProcessInfo],
    marker: &str,
) -> bool {
    let identities = identities_of(records, attempt);
    let mut groups: Vec<u32> = identities.iter().map(|identity| identity.pid).collect();
    groups.extend_from_slice(registry);
    let released: Vec<u32> = records
        .iter()
        .filter_map(|record| match record {
            Record::Decision {
                decision: Decision::Release { pids },
            } => Some(pids.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();
    ownership::check(processes, marker, &identities, &groups, &released) == ProcessCheck::Clear
}

/// Tests only: the directory of the test pauses, which
/// [`crate::evidence::TEST_PAUSE_ENV`] names; none when it is unset.
fn test_pauses() -> Option<PathBuf> {
    crate::evidence::pause_hook_from(std::env::var_os(crate::evidence::TEST_PAUSE_ENV))
}

/// The detail of the latest `paused` record.
fn last_pause_detail(records: &[Record]) -> Option<String> {
    records.iter().rev().find_map(|record| match record {
        Record::Paused { detail, .. } => Some(detail.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ownership::ProcessInfo;
    use crate::record::Identity;

    fn process(pid: u32, pgid: u32) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: 1,
            pgid,
            start: "Mon Jan  1 00:00:00 2026".into(),
            marker: None,
            command: "a stranger".into(),
        }
    }

    #[test]
    fn a_released_process_leaves_nothing_unresolved() {
        let attempt = AttemptId(3);
        let session = Record::AttemptSession {
            attempt,
            journal: PathBuf::from("journal"),
            identities: vec![Identity {
                pid: 100,
                start: "gone".into(),
            }],
        };
        // A process in the attempt's registered group, unverified.
        let processes = [process(101, 100)];
        let mut records = vec![session];
        assert!(!nothing_left(&records, attempt, &[], &processes, "r/3"));
        // Released, it is ignored from then on.
        records.push(Record::Decision {
            decision: Decision::Release { pids: vec![101] },
        });
        assert!(nothing_left(&records, attempt, &[], &processes, "r/3"));
        // A member of a registry group counts as well.
        let processes = [process(201, 200)];
        assert!(!nothing_left(&records, attempt, &[200], &processes, "r/3"));
    }

    #[test]
    fn a_processes_pause_replaces_a_pause_the_observation_answers() {
        use PauseReason::{Interrupted, ProcessesRunning, ProcessesUnverified, UnstableTests};
        // Logged afresh, a processes pause drops nothing: its own kind and an
        // interrupt are answered by the next observation.
        assert!(logs_pause(None, ProcessesRunning));
        assert!(logs_pause(Some(ProcessesRunning), ProcessesUnverified));
        assert!(logs_pause(Some(ProcessesUnverified), ProcessesRunning));
        assert!(logs_pause(Some(Interrupted), ProcessesRunning));
        // It never replaces a pause that waits for a decision.
        assert!(!logs_pause(Some(UnstableTests), ProcessesRunning));
        assert!(!logs_pause(Some(UnstableTests), ProcessesUnverified));
        // The same pause is not logged twice; another pause is logged.
        assert!(!logs_pause(Some(ProcessesRunning), ProcessesRunning));
        assert!(!logs_pause(Some(UnstableTests), UnstableTests));
        assert!(logs_pause(Some(ProcessesRunning), UnstableTests));
    }

    #[test]
    fn a_recurring_processes_pause_shows_the_processes_alive_now() {
        use PauseReason::{ProcessesRunning, ProcessesUnverified, UnstableTests};
        let logged = || Some("alive: [7, 8]".to_owned());
        for reason in [ProcessesRunning, ProcessesUnverified] {
            assert_eq!(
                shown_detail(reason, "alive: [8]".into(), logged()),
                "alive: [8]"
            );
        }
        assert_eq!(
            shown_detail(UnstableTests, "again".into(), logged()),
            "alive: [7, 8]"
        );
        assert_eq!(shown_detail(UnstableTests, "again".into(), None), "again");
    }

    #[test]
    fn a_local_failure_never_replaces_a_pause_that_waits_for_a_decision() {
        use PauseReason::{
            Interrupted, LocalFailure, ProcessesRunning, Reconcile, RoundLimit, UnstableTests,
        };
        assert!(logs_pause(None, LocalFailure));
        assert!(logs_pause(Some(Interrupted), LocalFailure));
        assert!(logs_pause(Some(ProcessesRunning), LocalFailure));
        assert!(!logs_pause(Some(Reconcile), LocalFailure));
        assert!(!logs_pause(Some(UnstableTests), LocalFailure));
        // What the user is asked: the pending pause; or, decided already,
        // `retry` (its decision stands).
        let base = Record::Candidate {
            id: CandidateId(0),
            tree: "t0".into(),
            parent: None,
            origin: Origin::Base,
        };
        let mut state = replay(&crate::sim::shipped(), &[base], &[]).unwrap();
        assert_eq!(asked_about(&state, LocalFailure), LocalFailure);
        state.pending_pause = Some(Reconcile);
        assert_eq!(asked_about(&state, LocalFailure), Reconcile);
        assert_eq!(asked_about(&state, ProcessesRunning), ProcessesRunning);
        state.decisions_since_pause = vec![Decision::Keep];
        assert!(decided(&state));
        assert_eq!(asked_about(&state, LocalFailure), LocalFailure);
        state.pending_pause = Some(RoundLimit);
        state.decisions_since_pause = vec![Decision::Wait];
        assert!(!decided(&state));
        assert_eq!(asked_about(&state, LocalFailure), RoundLimit);
        state.pending_pause = Some(Interrupted);
        assert_eq!(asked_about(&state, LocalFailure), LocalFailure);
    }

    /// A suspended machine: monotonic time stops while the wall clock does
    /// not. Here the wall clock jumps past the retry time after 90 s of
    /// monotonic time; the wait ends within the next slice, not an hour on.
    #[tokio::test(start_paused = true)]
    async fn a_wall_clock_past_the_retry_time_ends_the_wait() {
        let anchor = tokio::time::Instant::now();
        let clock = Clock::new(move || {
            let elapsed = anchor.elapsed().as_secs();
            if elapsed >= 90 {
                10_000
            } else {
                1_000 + elapsed
            }
        });
        let mut signals = None;
        assert_eq!(wait_until(&clock, 1_000 + 3_600, &mut signals).await, None);
        let waited = anchor.elapsed();
        assert!(
            waited >= Duration::from_secs(90) && waited <= Duration::from_secs(150),
            "{waited:?}"
        );
        // An unsuspended wait still lasts until the time.
        let anchor = tokio::time::Instant::now();
        let clock = Clock::new(move || 1_000 + anchor.elapsed().as_secs());
        assert_eq!(wait_until(&clock, 1_000 + 200, &mut signals).await, None);
        assert!(anchor.elapsed() >= Duration::from_secs(200));
        assert!(clock.now() >= 1_200);
    }
}
