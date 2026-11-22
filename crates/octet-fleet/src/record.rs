//! The records of a run's `log.jsonl`, the authority on accepted work.
//!
//! One record per line, a JSON object tagged by `"type"` in snake case. The
//! types here are the persistence contract: renaming a field breaks every log
//! already written. [`crate::state::replay`] folds the records into a
//! [`crate::state::RunState`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One attempt of one step, numbered from 0 across the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttemptId(pub u32);

/// One work-tree state in the run's lineage, numbered from 0 (the base).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateId(pub u32);

/// One artifact (document, approval, change, evidence), numbered across the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(pub u32);

/// A process identity that survives PID reuse: the PID and its start time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The process ID.
    pub pid: u32,
    /// The process's start time, as the platform reports it.
    pub start: String,
}

/// What made a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The run's starting tree.
    Base,
    /// A change step's accepted attempt.
    Change,
    /// A tests step's tester turn that edited the tree.
    Tests,
    /// An evidence command that changed the tree.
    Evidence,
    /// Adopted drift.
    Drift,
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Must be resolved before approval.
    Blocking,
    /// Reported at completion, never blocks.
    Minor,
}

/// One problem a verdict found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `<step>/F<n>`, assigned by Octet.
    pub id: String,
    /// How serious it is.
    pub severity: Severity,
    /// What is wrong.
    pub summary: String,
    /// The file it concerns, if any.
    pub file: Option<String>,
}

/// Whether a finding still stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Fixed.
    Resolved,
    /// Still standing.
    Open,
}

/// The outcome of an attempt that was accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Accepted {
    /// A document step produced its document.
    DocumentProduced {
        /// The document artifact.
        document: ArtifactId,
    },
    /// A verdict approved a document.
    DocumentApproved {
        /// The approval artifact.
        approval: ArtifactId,
        /// Findings first raised in this verdict.
        new_findings: Vec<Finding>,
        /// The verdict's account of the step's earlier findings.
        previous: Vec<(String, Status)>,
    },
    /// A verdict asked for changes.
    ChangesRequested {
        /// Findings first raised in this verdict.
        new_findings: Vec<Finding>,
        /// The verdict's account of the step's earlier findings.
        previous: Vec<(String, Status)>,
    },
    /// A change step produced its change.
    ChangeProduced {
        /// The change artifact.
        change: ArtifactId,
    },
    /// Every test command passed.
    TestsPassed {
        /// The evidence artifact.
        evidence: ArtifactId,
    },
    /// A test command failed.
    TestsFailed {
        /// The evidence artifact.
        evidence: ArtifactId,
        /// The indices of the failing commands.
        failing: Vec<usize>,
    },
    /// A verdict approved a change.
    ChangeApproved {
        /// The approval artifact.
        approval: ArtifactId,
        /// Findings first raised in this verdict.
        new_findings: Vec<Finding>,
        /// The verdict's account of the step's earlier findings.
        previous: Vec<(String, Status)>,
    },
}

/// Why an attempt failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Failure {
    /// A signal ended the attempt.
    Interrupted {
        /// The signal's name.
        signal: String,
    },
    /// A failure worth retrying (rate limit, network).
    Transient {
        /// What happened.
        detail: String,
    },
    /// The vendor is misconfigured.
    ConfigError {
        /// What happened.
        detail: String,
    },
    /// A failure on this machine.
    LocalFailure {
        /// What happened.
        detail: String,
    },
    /// A failure Octet cannot classify.
    Unknown {
        /// What happened.
        detail: String,
    },
    /// The output broke the step's contract.
    Invalid {
        /// What is wrong.
        detail: String,
    },
    /// A read-only step changed the tree.
    AccessViolation {
        /// The `git diff --stat` of the change.
        diff_stat: String,
    },
}

/// Why a run paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// An unfinished write attempt left the tree changed.
    Reconcile,
    /// A jump would exceed `max_rounds`.
    RoundLimit,
    /// A stall rule fired.
    Stalled,
    /// Tests changed the tree on every restart.
    UnstableTests,
    /// The live tree is not the one last recorded.
    Drift,
    /// A read-only step changed the tree.
    AccessViolation,
    /// Verified child processes are still running.
    ProcessesRunning,
    /// Leftover processes could not be verified.
    ProcessesUnverified,
    /// A step's output stayed invalid after the corrective prompt.
    InvalidOutput,
    /// Transient failures used up the retries.
    TransientExhausted,
    /// A failure on this machine.
    LocalFailure,
    /// A failure Octet cannot classify.
    Unknown,
    /// Ctrl+C or SIGTERM ended the run.
    Interrupted,
}

/// What the user decided at a pause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    /// Continue the interrupted attempt's work.
    Keep,
    /// The live tree becomes a candidate.
    Adopt {
        /// The candidate adopted.
        candidate: CandidateId,
    },
    /// Take the pending jump anyway.
    Continue,
    /// Raise the round cap, then continue.
    Rounds {
        /// How many more rounds.
        n: u32,
    },
    /// Try again.
    Retry,
    /// Signal verified processes.
    Kill,
    /// Check again.
    Wait,
    /// Log these PIDs as not the run's.
    Release {
        /// The PIDs released.
        pids: Vec<u32>,
    },
    /// End the run.
    Stop,
}

/// One line of `log.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    /// An attempt begins, before its session opens.
    AttemptStarted {
        /// The attempt.
        attempt: AttemptId,
        /// The step's ID.
        step: String,
        /// How many times this step has been attempted before, counting revisions.
        round: u32,
        /// The candidate the attempt starts from.
        input_candidate: CandidateId,
        /// The artifacts the attempt requires.
        requires: Vec<ArtifactId>,
        /// The artifacts kept to explain the attempt.
        history: Vec<ArtifactId>,
    },
    /// The attempt's child session exists.
    AttemptSession {
        /// The attempt.
        attempt: AttemptId,
        /// The child's journal.
        journal: PathBuf,
        /// The child process identities.
        identities: Vec<Identity>,
    },
    /// The vendor's own session ID, as soon as known.
    VendorSession {
        /// The attempt.
        attempt: AttemptId,
        /// The vendor's session ID.
        id: String,
    },
    /// A proposed candidate; it joins the lineage only when a committing record names it.
    Candidate {
        /// The candidate.
        id: CandidateId,
        /// The tree object.
        tree: String,
        /// The candidate it derives from.
        parent: Option<CandidateId>,
        /// What made it.
        origin: Origin,
    },
    /// The tester-to-evidence handoff, or an evidence restart.
    EvidenceStarted {
        /// The attempt.
        attempt: AttemptId,
        /// The candidate the commands run on.
        candidate: CandidateId,
        /// The restart count.
        restart: u32,
    },
    /// An evidence command is about to run.
    EvidenceCommand {
        /// The attempt.
        attempt: AttemptId,
        /// The restart count.
        restart: u32,
        /// The command's position in the list.
        index: usize,
        /// The command line.
        command: String,
        /// The command process.
        identity: Identity,
        /// The tree before the command.
        pre_tree: String,
    },
    /// An evidence command ended.
    EvidenceResult {
        /// The attempt.
        attempt: AttemptId,
        /// The restart count.
        restart: u32,
        /// The command's position in the list.
        index: usize,
        /// The exit code, `None` when killed by a signal.
        exit: Option<i32>,
        /// Whether the command hit its time limit.
        timed_out: bool,
        /// How long it ran.
        duration_ms: u64,
        /// The tree after the command.
        post_tree: String,
    },
    /// The attempt's output was accepted. Never repeated once durable.
    AttemptFinished {
        /// The attempt.
        attempt: AttemptId,
        /// What the step produced.
        outcome: Accepted,
        /// The candidate a change step made.
        candidate: Option<CandidateId>,
    },
    /// The attempt failed.
    AttemptFailed {
        /// The attempt.
        attempt: AttemptId,
        /// Why.
        failure: Failure,
    },
    /// A signal interrupted the attempt; it is not finished.
    AttemptInterrupted {
        /// The attempt.
        attempt: AttemptId,
        /// The signal's name.
        signal: String,
    },
    /// A transient failure will be retried.
    Retry {
        /// The failed attempt.
        attempt: AttemptId,
        /// The failure class.
        class: String,
        /// Which retry this is.
        count: u32,
        /// When to retry, in seconds since the Unix epoch.
        eligible_at: u64,
    },
    /// The run paused.
    Paused {
        /// Why.
        reason: PauseReason,
        /// A line for the user.
        detail: String,
    },
    /// The user decided.
    Decision {
        /// What was decided.
        decision: Decision,
    },
    /// The run completed.
    Completed {
        /// The final candidate.
        candidate: CandidateId,
        /// Its tree.
        tree: String,
        /// The evidence artifact.
        evidence: ArtifactId,
        /// The final approval artifact.
        approval: ArtifactId,
        /// The live tree's last observation.
        observed_tree: String,
    },
    /// The user stopped the run.
    Stopped,
    /// The run failed.
    Failed {
        /// What went wrong.
        error: String,
    },
}

/// What replay needs to know about an artifact; the rest lives in
/// `artifacts/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    /// The artifact.
    pub id: ArtifactId,
    /// The step that produced it, as an index into the workflow's steps.
    pub step: usize,
    /// The artifacts it requires to stay current.
    pub requires: Vec<ArtifactId>,
    /// The candidate it names: for a change, the candidate its step made; for
    /// evidence and a change approval, the candidate they bind to. `None` for
    /// documents and document approvals.
    pub candidate: Option<CandidateId>,
}
