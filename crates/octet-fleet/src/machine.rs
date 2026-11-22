//! The machine: a pure function of the replayed log that decides what runs
//! next.
//!
//! [`next`] reads a [`RunState`] and nothing else: no I/O, no clock. It runs
//! the first step, in order, whose latest artifact is missing, not current
//! or needs revision, and reports the run ready to complete once the logged
//! parts of the invariant hold. On the way it retries transient failures,
//! pauses on the other failures, on a stall and on a jump past the round
//! cap, and honours the decisions taken at a pause.

use std::collections::BTreeSet;

use crate::definition::{Kind, SessionPolicy};
use crate::record::{
    Accepted, ArtifactId, AttemptId, CandidateId, Decision, Failure, PauseReason, Status,
};
use crate::state::{AttemptInfo, AttemptState, RunState};
use crate::validate::{Refs, Resolved};

/// One attempt the runner should start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    /// The step, as an index.
    pub step: usize,
    /// 1 + the step's accepted attempts so far.
    pub round: u32,
    /// The candidate the attempt starts from: always the tip.
    pub input_candidate: CandidateId,
    /// The current artifacts the step directly requires.
    pub requires: Vec<ArtifactId>,
    /// The artifacts kept to explain the attempt: for a revision, the
    /// artifact being revised and, after failing tests, their evidence.
    pub history: Vec<ArtifactId>,
    /// The vendor session to continue, for a step whose policy is `continue`
    /// and that logged one.
    pub resume_session: Option<String>,
    /// Do not start before this time, in seconds since the Unix epoch.
    pub not_before: Option<u64>,
}

/// What the runner does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Start an attempt of a step.
    Run(Dispatch),
    /// Run (or restart) the evidence commands of a tests attempt whose
    /// evidence phase began.
    Evidence {
        /// The tests attempt.
        attempt: AttemptId,
        /// The candidate the commands run on.
        candidate: CandidateId,
        /// The restart count.
        restart: u32,
    },
    /// Finish an unfinished change attempt with the candidate a `Reconcile`
    /// pause adopted: the runner writes the change artifact, then
    /// `attempt_finished { change_produced, candidate }`.
    FinishAdopted {
        /// The change attempt.
        attempt: AttemptId,
        /// The adopted candidate.
        candidate: CandidateId,
    },
    /// Wait for the user's decision.
    Pause(PauseReason, String),
    /// The logged parts of the invariant hold; the runner takes its last
    /// observation of the live tree.
    ReadyToComplete {
        /// The final candidate, the tip.
        candidate: CandidateId,
        /// Its tree.
        tree: String,
        /// The passing evidence for it.
        evidence: ArtifactId,
        /// The final approval.
        approval: ArtifactId,
    },
    /// The run cannot go on: the runner records `failed` with this error and
    /// exits 1.
    Fail(String),
    /// The run completed, stopped or failed.
    Done,
}

/// Decides what runs next from the replayed log.
///
/// In order: a run that ended is done; a pause waits unless its latest
/// decision resumes the run (keep, adopt, continue, rounds, retry); the
/// latest attempt's failure is retried, paused on or fails the run; a tests
/// attempt whose evidence phase began resumes it; a stall the user has not
/// gone past pauses; otherwise the first step whose latest artifact is
/// missing, not current or needs revision runs, unless it is a revision whose
/// jump exceeds the round cap; with every step settled, the run is ready to
/// complete if the logged parts of the invariant hold.
///
/// `state` must be [`crate::state::replay`]'s output for this `def`; step
/// indices are trusted.
#[must_use]
pub fn next(def: &Resolved, state: &RunState) -> Next {
    if state.completed || state.stopped || state.failed.is_some() {
        return Next::Done;
    }
    if let Some(reason) = state.pending_pause
        && !state
            .decisions_since_pause
            .last()
            .is_some_and(ends_the_wait)
    {
        return Next::Pause(reason, format!("paused ({reason:?}); awaiting a decision"));
    }
    if let Some(next) = after_a_failure(def, state) {
        return next;
    }
    if let Some(evidence) = evidence_in_progress(def, state) {
        return evidence;
    }
    if let Some(stall) = stall(def, state) {
        return Next::Pause(PauseReason::Stalled, stall.detail);
    }
    match first_unsettled(def, state) {
        Some(step) => round_limit(def, state, step)
            .unwrap_or_else(|| Next::Run(dispatch(def, state, step, None))),
        None => ready_to_complete(def, state),
    }
}

/// The first step, in order, whose latest artifact is missing, not current
/// or needs revision.
fn first_unsettled(def: &Resolved, state: &RunState) -> Option<usize> {
    (0..def.steps.len()).find(|&step| {
        let current = state.latest[step].is_some_and(|artifact| is_current(def, state, artifact));
        !current || state.needs_revision[step]
    })
}

/// The rejector whose jump the run would take next: the one that marked the
/// first unsettled step for revision.
pub(crate) fn revising_rejector(def: &Resolved, state: &RunState) -> Option<usize> {
    let step = first_unsettled(def, state)?;
    if state.needs_revision[step] {
        state.rejected_by[step]
    } else {
        None
    }
}

/// The cap on `rejector`'s jumps: `max_rounds`, or the cap a `rounds N` set,
/// plus one per `continue` taken past it.
fn round_cap(def: &Resolved, state: &RunState, rejector: usize) -> u32 {
    let base = state
        .round_caps
        .get(&rejector)
        .copied()
        .unwrap_or(def.limits.max_rounds);
    base.saturating_add(state.extra_rounds.get(&rejector).copied().unwrap_or(0))
}

/// A revision of `step` whose jump exceeds the cap on its edge pauses.
fn round_limit(def: &Resolved, state: &RunState, step: usize) -> Option<Next> {
    if !state.needs_revision[step] {
        return None;
    }
    let rejector = state.rejected_by[step]?;
    let jumps = state.jumps.get(&rejector).copied().unwrap_or(0);
    let cap = round_cap(def, state, rejector);
    (jumps > cap).then(|| {
        Next::Pause(
            PauseReason::RoundLimit,
            format!(
                "{} sent {} back {jumps} times; the cap is {cap}",
                def.steps[rejector].id, def.steps[step].id
            ),
        )
    })
}

/// What follows the latest attempt when it failed: a config error fails the
/// run; a `retry` decision at the pause runs the step again; a transient
/// failure is retried (at its logged eligible time) until the retries are
/// used up; an interrupted one runs the step again through the normal
/// dispatch; every other failure pauses.
fn after_a_failure(def: &Resolved, state: &RunState) -> Option<Next> {
    let (attempt, info) = state.attempts.iter().next_back()?;
    let AttemptState::Failed(failure) = &info.state else {
        return None;
    };
    let step = &def.steps[info.step].id;
    if let Failure::ConfigError { detail } = failure {
        return Some(Next::Fail(format!("{step}: {detail}")));
    }
    let retried = state.pending_pause.is_some()
        && state.decisions_since_pause.last() == Some(&Decision::Retry);
    if retried {
        return Some(Next::Run(dispatch(def, state, info.step, None)));
    }
    let (reason, detail) = match failure {
        Failure::Transient { detail } => {
            let failures = transient_streak(state, info.step);
            if failures <= def.limits.transient_retries {
                let not_before = state.retry.get(attempt).map(|&(_, at)| at);
                return Some(Next::Run(dispatch(def, state, info.step, not_before)));
            }
            (
                PauseReason::TransientExhausted,
                format!("{step} failed transiently {failures} times in a row: {detail}"),
            )
        }
        Failure::Invalid { detail } => (
            PauseReason::InvalidOutput,
            format!("{step}'s output is invalid: {detail}"),
        ),
        Failure::LocalFailure { detail } => (
            PauseReason::LocalFailure,
            format!("{step} failed on this machine: {detail}"),
        ),
        Failure::Unknown { detail } => (PauseReason::Unknown, format!("{step} failed: {detail}")),
        Failure::AccessViolation { diff_stat } => (
            PauseReason::AccessViolation,
            format!("{step} is read-only but changed the tree: {diff_stat}"),
        ),
        Failure::Interrupted { .. } | Failure::ConfigError { .. } => return None,
    };
    Some(Next::Pause(reason, detail))
}

/// The transient failures of `step` in a row, newest first, back to its last
/// accepted attempt or a failure of another class. Unfinished attempts (a
/// crash, an interrupt) neither count nor end the streak, so they cannot
/// reset the retries.
fn transient_streak(state: &RunState, step: usize) -> u32 {
    let mut failures: u32 = 0;
    for info in state
        .attempts
        .values()
        .rev()
        .filter(|info| info.step == step)
    {
        match &info.state {
            AttemptState::Failed(Failure::Transient { .. }) => {
                failures = failures.saturating_add(1);
            }
            AttemptState::Unfinished { .. } => {}
            AttemptState::Finished | AttemptState::Failed(_) => break,
        }
    }
    failures
}

/// A stall: the point it is waived at and a line for the user.
struct Stall {
    point: (usize, u32),
    detail: String,
}

/// The first stall the user has not gone past.
fn stall(def: &Resolved, state: &RunState) -> Option<Stall> {
    stalls(def, state)
        .into_iter()
        .find(|stall| !state.stall_waived.contains(&stall.point))
}

/// The point of the stall the run would pause on, for replay to waive at a
/// `continue`.
pub(crate) fn stall_point(def: &Resolved, state: &RunState) -> Option<(usize, u32)> {
    stall(def, state).map(|stall| stall.point)
}

/// Every stall rule that holds, waived or not: a change given feedback that
/// left its input's tree; and, for the rejection the run would revise next,
/// a verdict that resolved none of its step's earlier open findings or the
/// same tests failing twice on the same tree.
fn stalls(def: &Resolved, state: &RunState) -> Vec<Stall> {
    let mut stalls: Vec<Stall> = (0..def.steps.len())
        .filter(|&step| def.steps[step].kind == Kind::Change)
        .filter_map(|step| unchanged_tree(def, state, step))
        .collect();
    if let Some(rejector) = revising_rejector(def, state) {
        let jumps = state.jumps.get(&rejector).copied().unwrap_or(0);
        let step = &def.steps[rejector];
        let detail = match step.kind {
            Kind::Verdict => resolved_nothing(state, rejector).then(|| {
                format!(
                    "{} asked for changes again without resolving any of its open findings",
                    step.id
                )
            }),
            Kind::Tests => same_failure_twice(state, rejector).then(|| {
                format!(
                    "{} failed the same commands twice on the same tree",
                    step.id
                )
            }),
            Kind::Document | Kind::Change => None,
        };
        stalls.extend(detail.map(|detail| Stall {
            point: (rejector, jumps),
            detail,
        }));
    }
    stalls
}

/// The accepted outcomes of `step`, newest first.
fn outcomes_of(
    state: &RunState,
    step: usize,
) -> impl Iterator<Item = (&AttemptId, &(Accepted, Option<CandidateId>))> {
    state
        .outcomes
        .iter()
        .rev()
        .filter(move |(attempt, _)| state.attempts.get(attempt).is_some_and(|i| i.step == step))
}

/// The tree of a committed candidate.
pub(crate) fn tree_of(state: &RunState, candidate: CandidateId) -> Option<&str> {
    state
        .candidates
        .iter()
        .find(|c| c.id == candidate)
        .map(|c| c.tree.as_str())
}

/// The change step's latest accepted attempt was given feedback and made a
/// candidate whose tree is its input's.
fn unchanged_tree(def: &Resolved, state: &RunState, step: usize) -> Option<Stall> {
    let (attempt, (outcome, candidate)) = outcomes_of(state, step).next()?;
    let info = &state.attempts[attempt];
    if !matches!(outcome, Accepted::ChangeProduced { .. }) || info.history.is_empty() {
        return None;
    }
    let tree = tree_of(state, (*candidate)?)?;
    if tree != tree_of(state, info.input_candidate)? {
        return None;
    }
    let accepted = u32::try_from(outcomes_of(state, step).count()).unwrap_or(u32::MAX);
    Some(Stall {
        point: (step, accepted),
        detail: format!(
            "{} was given feedback and left the tree unchanged",
            def.steps[step].id
        ),
    })
}

/// The verdict step's latest outcome asks for changes and resolves none of
/// the earlier open findings it accounts for (there must be some).
fn resolved_nothing(state: &RunState, verdict: usize) -> bool {
    match outcomes_of(state, verdict).next() {
        Some((_, (Accepted::ChangesRequested { previous, .. }, _))) => {
            !previous.is_empty() && previous.iter().all(|(_, s)| *s == Status::Open)
        }
        _ => false,
    }
}

/// The tests step's two latest outcomes both failed the same commands, on
/// candidates with the same tree.
fn same_failure_twice(state: &RunState, tests: usize) -> bool {
    let failed = |outcome: &Accepted| match outcome {
        Accepted::TestsFailed { evidence, failing } => {
            let candidate = state.artifacts.get(evidence)?.candidate?;
            Some((tree_of(state, candidate)?.to_owned(), failing.clone()))
        }
        _ => None,
    };
    let mut latest = outcomes_of(state, tests).map(|(_, (outcome, _))| failed(outcome));
    match (latest.next(), latest.next()) {
        (Some(Some(last)), Some(Some(before))) => last == before,
        _ => false,
    }
}

/// Whether a decision lets the run go on from its pause. `wait`, `kill` and
/// `release` leave it waiting on its processes, and `stop` ends it (its
/// `stopped` record may not be durable yet); the others resume.
pub(crate) fn ends_the_wait(decision: &Decision) -> bool {
    match decision {
        Decision::Keep
        | Decision::Adopt { .. }
        | Decision::Continue
        | Decision::Rounds { .. }
        | Decision::Retry => true,
        Decision::Kill | Decision::Wait | Decision::Release { .. } | Decision::Stop => false,
    }
}

/// Whether `artifact` is current: the latest of its step, everything it
/// requires current (transitively), and, for evidence and change approvals,
/// bound to the tip. A change is not tip-bound: derived candidates (tester
/// edits, evidence edits, adopted drift) leave it current.
#[must_use]
pub fn is_current(def: &Resolved, state: &RunState, artifact: ArtifactId) -> bool {
    current(def, state, artifact, &mut Vec::new())
}

/// [`is_current`], refusing a requirement cycle (`path` holds the artifacts
/// being checked) rather than recursing forever.
fn current(
    def: &Resolved,
    state: &RunState,
    artifact: ArtifactId,
    path: &mut Vec<ArtifactId>,
) -> bool {
    if path.contains(&artifact) {
        return false;
    }
    let Some(meta) = state.artifacts.get(&artifact) else {
        return false;
    };
    let Some(step) = def.steps.get(meta.step) else {
        return false;
    };
    if state.latest.get(meta.step).copied().flatten() != Some(artifact) {
        return false;
    }
    let tip_bound = matches!(step.refs, Refs::Tests { .. } | Refs::ReviewChange { .. });
    if tip_bound && meta.candidate != Some(state.tip) {
        return false;
    }
    path.push(artifact);
    let requirements = meta
        .requires
        .iter()
        .all(|required| current(def, state, *required, path));
    path.pop();
    requirements
}

/// The latest attempt, when it is an unfinished tests attempt whose evidence
/// phase began. Only the latest attempt can be live: a later
/// `attempt_started` supersedes every earlier unfinished attempt.
fn evidence_in_progress(def: &Resolved, state: &RunState) -> Option<Next> {
    let (attempt, info) = state.attempts.iter().next_back()?;
    let AttemptState::Unfinished {
        evidence: Some((candidate, restart)),
        ..
    } = info.state
    else {
        return None;
    };
    matches!(def.steps[info.step].refs, Refs::Tests { .. }).then_some(Next::Evidence {
        attempt: *attempt,
        candidate,
        restart,
    })
}

/// A new attempt of `info`'s step in `info`'s round: the dispatch [`next`]
/// builds for the step, from the tip.
pub(crate) fn again(def: &Resolved, state: &RunState, info: &AttemptInfo) -> Dispatch {
    Dispatch {
        round: info.round,
        ..dispatch(def, state, info.step, None)
    }
}

fn dispatch(def: &Resolved, state: &RunState, step: usize, not_before: Option<u64>) -> Dispatch {
    let accepted = state
        .attempts
        .values()
        .filter(|info| info.step == step && info.state == AttemptState::Finished)
        .count();
    let resume_session = match def.steps[step].session {
        SessionPolicy::Continue => state.sessions[step].clone(),
        SessionPolicy::Fresh => None,
    };
    Dispatch {
        step,
        round: u32::try_from(accepted).map_or(u32::MAX, |n| n.saturating_add(1)),
        input_candidate: state.tip,
        requires: requires(def, state, step),
        history: history(def, state, step),
        resume_session,
        not_before,
    }
}

/// The latest artifacts `step` directly requires: a document's or change's
/// input approvals, the reviewed document, the change under test, or the
/// reviewed change and its evidence.
fn requires(def: &Resolved, state: &RunState, step: usize) -> Vec<ArtifactId> {
    let latest = |step: usize| state.latest[step];
    match &def.steps[step].refs {
        Refs::Document { inputs } | Refs::Change { inputs } => inputs
            .iter()
            .filter_map(|input| latest(input.approval))
            .collect(),
        Refs::ReviewDocument { document } => latest(*document).into_iter().collect(),
        Refs::Tests { change, .. } => latest(*change).into_iter().collect(),
        Refs::ReviewChange { change, evidence } => [*change, *evidence]
            .into_iter()
            .filter_map(latest)
            .collect(),
    }
}

/// For a revision, the rejected artifact, plus the failing evidence when a
/// tests step rejected it; nothing otherwise. Findings come from the ledger.
fn history(def: &Resolved, state: &RunState, step: usize) -> Vec<ArtifactId> {
    if !state.needs_revision[step] {
        return Vec::new();
    }
    let mut history: Vec<ArtifactId> = state.latest[step].into_iter().collect();
    if let Some(rejector) = state.rejected_by[step]
        && matches!(def.steps[rejector].refs, Refs::Tests { .. })
        && let Some(evidence) = state.latest[rejector]
    {
        history.push(evidence);
    }
    history
}

/// Every step is settled: ready when the logged parts of the invariant
/// hold. The final step is a change review whose latest approval is current
/// and for the tip; it requires the change step's latest change and current,
/// passing evidence for the tip; and every document approval the change step
/// needs is current and required, through the change, by both the evidence
/// and the approval. Anything else pauses naming the part that fails.
fn ready_to_complete(def: &Resolved, state: &RunState) -> Next {
    match invariant(def, state) {
        Ok(ready) => ready,
        Err(part) => Next::Pause(
            PauseReason::Unknown,
            format!("every step is settled but {part}"),
        ),
    }
}

/// [`ready_to_complete`]'s checks, failing with the part that does not hold.
fn invariant(def: &Resolved, state: &RunState) -> Result<Next, String> {
    let (last, step) = def
        .steps
        .iter()
        .enumerate()
        .next_back()
        .ok_or("the workflow has no steps")?;
    let Refs::ReviewChange {
        change: change_step,
        evidence: tests_step,
    } = step.refs
    else {
        return Err(format!(
            "the last step, {}, is not a change review",
            step.id
        ));
    };
    let approval = state.latest[last].ok_or("there is no final approval")?;
    let meta = |artifact: ArtifactId| state.artifacts.get(&artifact);
    let for_tip =
        |artifact: ArtifactId| meta(artifact).is_some_and(|m| m.candidate == Some(state.tip));
    if !is_current(def, state, approval) || !for_tip(approval) {
        return Err("the final approval is not current for the tip".into());
    }
    let required = |step: usize| {
        meta(approval).and_then(|m| {
            m.requires
                .iter()
                .copied()
                .find(|&r| meta(r).is_some_and(|m| m.step == step))
        })
    };
    let evidence = required(tests_step).ok_or("the final approval names no evidence")?;
    if !is_current(def, state, evidence)
        || !for_tip(evidence)
        || !state.tests_passed.contains(&evidence)
    {
        return Err(
            "the final approval's evidence is not a current passing record for the tip".into(),
        );
    }
    if required(change_step).is_none() || required(change_step) != state.latest[change_step] {
        return Err("the final approval is not for the latest change".into());
    }
    let under_approval = requirements(state, approval);
    let under_evidence = requirements(state, evidence);
    for approval_step in def.approvals_required(change_step) {
        let document_approval = state.latest[approval_step]
            .filter(|&a| is_current(def, state, a))
            .filter(|a| under_approval.contains(a) && under_evidence.contains(a));
        if document_approval.is_none() {
            return Err(format!(
                "{}'s approval is not current, or the change was not made under it",
                def.steps[approval_step].id
            ));
        }
    }
    let tree = tree_of(state, state.tip).ok_or("the tip has no tree")?;
    Ok(Next::ReadyToComplete {
        candidate: state.tip,
        tree: tree.to_owned(),
        evidence,
        approval,
    })
}

/// Everything `artifact` requires, transitively.
fn requirements(state: &RunState, artifact: ArtifactId) -> BTreeSet<ArtifactId> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![artifact];
    while let Some(next) = pending.pop() {
        if let Some(meta) = state.artifacts.get(&next) {
            for &required in &meta.requires {
                if seen.insert(required) {
                    pending.push(required);
                }
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::definition::{Kind, parse};
    use crate::record::{
        Accepted, ArtifactMeta, Decision, Failure, Finding, Origin, Record, Severity, Status,
    };
    use crate::state::replay;
    use crate::validate::{Host, Refs, validate};

    const SHIPPED: &str = include_str!("../../../docs/fleet/research-plan-build.yaml");

    struct FakeHost;

    impl Host for FakeHost {
        fn engine_known(&self, _: &str) -> bool {
            true
        }
        fn check_effort(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn resolve_binary(&self, engine: &str, _: Option<&Path>) -> Option<PathBuf> {
            Some(PathBuf::from(format!("/bin/{engine}")))
        }
    }

    /// A run log written by hand, one accepted outcome at a time, driven by
    /// the machine's own dispatches: `run` asserts what `next` says and logs
    /// the attempt it names.
    struct Log {
        def: Resolved,
        records: Vec<Record>,
        metas: Vec<ArtifactMeta>,
        dispatched: BTreeMap<AttemptId, Dispatch>,
        next_candidate: u32,
    }

    impl Log {
        fn new() -> Self {
            Self {
                def: validate(&parse(SHIPPED).unwrap(), &FakeHost).unwrap(),
                records: vec![Record::Candidate {
                    id: CandidateId(0),
                    tree: "t0".into(),
                    parent: None,
                    origin: Origin::Base,
                }],
                metas: Vec::new(),
                dispatched: BTreeMap::new(),
                next_candidate: 1,
            }
        }

        fn state(&self) -> RunState {
            replay(&self.def, &self.records, &self.metas).unwrap()
        }

        fn next(&self) -> Next {
            next(&self.def, &self.state())
        }

        fn current(&self, artifact: ArtifactId) -> bool {
            is_current(&self.def, &self.state(), artifact)
        }

        fn idx(&self, step: &str) -> usize {
            self.def.steps.iter().position(|s| s.id == step).unwrap()
        }

        /// The step `next` would run, if it says to run one.
        fn next_step(&self) -> Option<String> {
            match self.next() {
                Next::Run(dispatch) => Some(self.def.steps[dispatch.step].id.clone()),
                _ => None,
            }
        }

        /// How many attempts of `step` the log started.
        fn attempts_of(&self, step: &str) -> usize {
            let step = self.idx(step);
            self.dispatched.values().filter(|d| d.step == step).count()
        }

        /// Asserts `next` runs `step` and logs the attempt's start.
        fn run(&mut self, step: &str) -> (AttemptId, Dispatch) {
            self.run_after(step, None)
        }

        /// Asserts `next` runs `step` not before `not_before` and logs the
        /// attempt's start.
        fn run_after(&mut self, step: &str, not_before: Option<u64>) -> (AttemptId, Dispatch) {
            let next = self.next();
            let Next::Run(dispatch) = next else {
                panic!("expected Run({step}), got {next:?}");
            };
            assert_eq!(self.def.steps[dispatch.step].id, step, "{dispatch:?}");
            assert_eq!(dispatch.not_before, not_before);
            self.start(dispatch)
        }

        /// Logs the start of `dispatch`'s attempt.
        fn start(&mut self, dispatch: Dispatch) -> (AttemptId, Dispatch) {
            let step = self.def.steps[dispatch.step].id.clone();
            let attempt = AttemptId(u32::try_from(self.dispatched.len()).unwrap());
            self.records.push(Record::AttemptStarted {
                attempt,
                step,
                round: dispatch.round,
                input_candidate: dispatch.input_candidate,
                requires: dispatch.requires.clone(),
                history: dispatch.history.clone(),
            });
            self.dispatched.insert(attempt, dispatch.clone());
            (attempt, dispatch)
        }

        /// A new artifact of the attempt's step, requiring what the attempt
        /// was given.
        fn artifact(&mut self, attempt: AttemptId, candidate: Option<CandidateId>) -> ArtifactId {
            let dispatch = &self.dispatched[&attempt];
            let id = ArtifactId(u32::try_from(self.metas.len()).unwrap());
            self.metas.push(ArtifactMeta {
                id,
                step: dispatch.step,
                requires: dispatch.requires.clone(),
                candidate,
            });
            id
        }

        fn finish(
            &mut self,
            attempt: AttemptId,
            outcome: Accepted,
            candidate: Option<CandidateId>,
        ) {
            self.records.push(Record::AttemptFinished {
                attempt,
                outcome,
                candidate,
            });
        }

        /// Proposes a candidate derived from the tip.
        fn propose(&mut self, origin: Origin) -> CandidateId {
            let tree = format!("t{}", self.next_candidate);
            self.propose_tree(origin, &tree)
        }

        /// Proposes a candidate with `tree`, derived from the tip.
        fn propose_tree(&mut self, origin: Origin, tree: &str) -> CandidateId {
            let id = CandidateId(self.next_candidate);
            self.next_candidate += 1;
            self.records.push(Record::Candidate {
                id,
                tree: tree.into(),
                parent: Some(self.state().tip),
                origin,
            });
            id
        }

        /// Logs an attempt of `step` the machine did not dispatch, accepting
        /// `outcome` for a new artifact with `requires` and `candidate`.
        fn forge(
            &mut self,
            step: &str,
            requires: Vec<ArtifactId>,
            candidate: Option<CandidateId>,
            outcome: impl FnOnce(ArtifactId) -> Accepted,
        ) -> ArtifactId {
            let state = self.state();
            let (attempt, _) = self.start(Dispatch {
                step: self.idx(step),
                round: 1,
                input_candidate: state.tip,
                requires,
                history: vec![],
                resume_session: None,
                not_before: None,
            });
            let artifact = self.artifact(attempt, candidate);
            self.finish(attempt, outcome(artifact), None);
            artifact
        }

        fn fail(&mut self, attempt: AttemptId, failure: Failure) {
            self.records
                .push(Record::AttemptFailed { attempt, failure });
        }

        fn retry(&mut self, attempt: AttemptId, count: u32, eligible_at: u64) {
            self.records.push(Record::Retry {
                attempt,
                class: "transient".into(),
                count,
                eligible_at,
            });
        }

        /// The runner logs the pause `next` asked for.
        fn pause(&mut self, reason: PauseReason) {
            self.records.push(Record::Paused {
                reason,
                detail: String::new(),
            });
        }

        fn decide(&mut self, decision: Decision) {
            self.records.push(Record::Decision { decision });
        }

        /// Asserts `next` pauses for `reason` and returns its detail.
        fn paused(&self, reason: PauseReason) -> String {
            match self.next() {
                Next::Pause(r, detail) if r == reason => detail,
                other => panic!("expected Pause({reason:?}), got {other:?}"),
            }
        }

        fn vendor_session(&mut self, attempt: AttemptId, id: &str) {
            self.records.push(Record::VendorSession {
                attempt,
                id: id.into(),
            });
        }

        fn produce(&mut self, attempt: AttemptId) -> ArtifactId {
            let document = self.artifact(attempt, None);
            self.finish(attempt, Accepted::DocumentProduced { document }, None);
            document
        }

        fn change(&mut self, attempt: AttemptId) -> (ArtifactId, CandidateId) {
            let tree = format!("t{}", self.next_candidate);
            self.change_tree(attempt, &tree)
        }

        /// The change step's candidate, with `tree`.
        fn change_tree(&mut self, attempt: AttemptId, tree: &str) -> (ArtifactId, CandidateId) {
            let candidate = self.propose_tree(Origin::Change, tree);
            let change = self.artifact(attempt, Some(candidate));
            self.finish(
                attempt,
                Accepted::ChangeProduced { change },
                Some(candidate),
            );
            (change, candidate)
        }

        /// The open findings of the attempt's step, as resolved.
        fn resolve_all(&self, attempt: AttemptId) -> Vec<(String, Status)> {
            let step = &self.def.steps[self.dispatched[&attempt].step].id;
            self.state()
                .findings
                .iter()
                .filter(|(id, (_, status))| {
                    id.starts_with(&format!("{step}/")) && *status == Status::Open
                })
                .map(|(id, _)| (id.clone(), Status::Resolved))
                .collect()
        }

        fn approve(&mut self, attempt: AttemptId) -> ArtifactId {
            let previous = self.resolve_all(attempt);
            let dispatch = self.dispatched[&attempt].clone();
            if matches!(
                self.def.steps[dispatch.step].refs,
                Refs::ReviewChange { .. }
            ) {
                let approval = self.artifact(attempt, Some(dispatch.input_candidate));
                let outcome = Accepted::ChangeApproved {
                    approval,
                    new_findings: vec![],
                    previous,
                };
                self.finish(attempt, outcome, None);
                approval
            } else {
                let approval = self.artifact(attempt, None);
                let outcome = Accepted::DocumentApproved {
                    approval,
                    new_findings: vec![],
                    previous,
                };
                self.finish(attempt, outcome, None);
                approval
            }
        }

        /// One new blocking finding; the earlier ones stay open.
        fn request_changes(&mut self, attempt: AttemptId) {
            self.request_changes_resolving(attempt, 0);
        }

        /// One new blocking finding; the first `resolved` earlier open
        /// findings are resolved, the rest stay open.
        fn request_changes_resolving(&mut self, attempt: AttemptId, resolved: usize) {
            let step = self.def.steps[self.dispatched[&attempt].step].id.clone();
            let state = self.state();
            let previous = state
                .findings
                .iter()
                .filter(|(id, (_, status))| {
                    id.starts_with(&format!("{step}/")) && *status == Status::Open
                })
                .enumerate()
                .map(|(n, (id, _))| {
                    let status = if n < resolved {
                        Status::Resolved
                    } else {
                        Status::Open
                    };
                    (id.clone(), status)
                })
                .collect();
            let new_findings = state
                .next_finding_ids(&step, 1)
                .into_iter()
                .map(|id| Finding {
                    id,
                    severity: Severity::Blocking,
                    summary: "fix it".into(),
                    file: None,
                })
                .collect();
            self.finish(
                attempt,
                Accepted::ChangesRequested {
                    new_findings,
                    previous,
                },
                None,
            );
        }

        fn start_evidence(&mut self, attempt: AttemptId, candidate: CandidateId, restart: u32) {
            self.records.push(Record::EvidenceStarted {
                attempt,
                candidate,
                restart,
            });
        }

        /// The tester edits the tree: a derived candidate, committed by the
        /// handoff to evidence.
        fn tester_edit(&mut self, attempt: AttemptId) -> CandidateId {
            let candidate = self.propose(Origin::Tests);
            self.start_evidence(attempt, candidate, 0);
            candidate
        }

        /// The evidence runs on the tip (starting it if the tester did not
        /// edit) and passes or fails command 0.
        fn tests(&mut self, attempt: AttemptId, pass: bool) -> ArtifactId {
            self.tests_failing(attempt, if pass { vec![] } else { vec![0] })
        }

        /// The evidence runs on the tip (starting it if the tester did not
        /// edit); the `failing` commands fail, and it passes if none do.
        fn tests_failing(&mut self, attempt: AttemptId, failing: Vec<usize>) -> ArtifactId {
            let state = self.state();
            let started = matches!(
                state.attempts[&attempt].state,
                crate::state::AttemptState::Unfinished {
                    evidence: Some(_),
                    ..
                }
            );
            if !started {
                self.start_evidence(attempt, state.tip, 0);
            }
            let evidence = self.artifact(attempt, Some(state.tip));
            let outcome = if failing.is_empty() {
                Accepted::TestsPassed { evidence }
            } else {
                Accepted::TestsFailed { evidence, failing }
            };
            self.finish(attempt, outcome, None);
            evidence
        }

        /// The live tree drifted after the last record; the user adopts it.
        fn adopt_drift(&mut self) -> CandidateId {
            let candidate = self.propose(Origin::Drift);
            self.records.push(Record::Paused {
                reason: PauseReason::Drift,
                detail: "the live tree changed".into(),
            });
            self.records.push(Record::Decision {
                decision: Decision::Adopt { candidate },
            });
            candidate
        }

        /// Research and its approval, in one round: `(D, A(D))`.
        fn approved_research(&mut self) -> (ArtifactId, ArtifactId) {
            let (attempt, _) = self.run("research");
            let document = self.produce(attempt);
            let (attempt, _) = self.run("review_research");
            (document, self.approve(attempt))
        }

        /// Research, the plan and both approvals: `(A(D), A(P))`.
        fn approved_plan(&mut self) -> (ArtifactId, ArtifactId) {
            let (_, research) = self.approved_research();
            let (attempt, _) = self.run("plan");
            self.produce(attempt);
            let (attempt, _) = self.run("review_plan");
            (research, self.approve(attempt))
        }

        /// Everything up to the coder's change, the coder logging a vendor
        /// session: `(A(P), C, K)`.
        fn implemented(&mut self) -> (ArtifactId, ArtifactId, CandidateId) {
            let (_, plan) = self.approved_plan();
            let (attempt, dispatch) = self.run("implement");
            assert_eq!(dispatch.requires, [plan]);
            self.vendor_session(attempt, "coder-1");
            let (change, candidate) = self.change(attempt);
            (plan, change, candidate)
        }
    }

    #[test]
    fn research_rejected_then_revised_goes_to_review_research() {
        let mut log = Log::new();
        let (attempt, dispatch) = log.run("research");
        assert_eq!(dispatch.round, 1);
        assert_eq!(dispatch.input_candidate, CandidateId(0));
        assert!(dispatch.requires.is_empty() && dispatch.history.is_empty());
        let d1 = log.produce(attempt);

        let (review, dispatch) = log.run("review_research");
        assert_eq!(dispatch.requires, [d1]);
        assert_eq!(dispatch.resume_session, None);
        log.request_changes(review);

        // The revision: D1 needs revision, the rejection is history.
        let (attempt, dispatch) = log.run("research");
        assert_eq!(dispatch.requires, []);
        assert_eq!(dispatch.history, [d1]);
        assert_eq!(dispatch.round, 2);
        let d2 = log.produce(attempt);
        assert!(log.current(d2));
        assert!(!log.current(d1));

        let (_, dispatch) = log.run("review_research");
        assert_eq!(dispatch.requires, [d2]);
        assert!(dispatch.history.is_empty());
        // The open findings come from the ledger, not from history.
        assert_eq!(log.state().findings["review_research/F1"].1, Status::Open);
    }

    #[test]
    fn nothing_downstream_runs_before_research_is_approved() {
        let mut log = Log::new();
        let downstream = |log: &Log| {
            let step = log.next_step();
            assert!(
                !matches!(step.as_deref(), Some("plan" | "implement")),
                "{step:?}"
            );
        };
        downstream(&log);
        let (attempt, _) = log.run("research");
        log.produce(attempt);
        downstream(&log);
        assert_eq!(log.next_step().as_deref(), Some("review_research"));
        let (review, _) = log.run("review_research");
        log.request_changes(review);
        downstream(&log);
        let (attempt, _) = log.run("research");
        log.produce(attempt);
        downstream(&log);
        assert_eq!(log.next_step().as_deref(), Some("review_research"));
    }

    #[test]
    fn plan_runs_with_the_approved_research() {
        let mut log = Log::new();
        let (research, approval) = log.approved_research();
        assert!(log.current(research) && log.current(approval));
        let (_, dispatch) = log.run("plan");
        assert_eq!(dispatch.requires, [approval]);
        assert!(dispatch.history.is_empty());
        assert_eq!(dispatch.round, 1);
    }

    #[test]
    fn plan_loop_does_not_rerun_research() {
        let mut log = Log::new();
        let (_, research) = log.approved_research();
        let (attempt, _) = log.run("plan");
        let p1 = log.produce(attempt);
        let (review, dispatch) = log.run("review_plan");
        assert_eq!(dispatch.requires, [p1]);
        log.request_changes(review);
        assert!(log.current(research));

        let (attempt, dispatch) = log.run("plan");
        assert_eq!(dispatch.requires, [research]);
        assert_eq!(dispatch.history, [p1]);
        let p2 = log.produce(attempt);
        assert!(log.current(research));

        let (_, dispatch) = log.run("review_plan");
        assert_eq!(dispatch.requires, [p2]);
        assert!(log.current(research));
        assert_eq!(log.attempts_of("research"), 1);
        assert_eq!(log.attempts_of("review_research"), 1);
    }

    #[test]
    fn a_tester_edit_does_not_rerun_the_coder() {
        let mut log = Log::new();
        let (plan, c1, k1) = log.implemented();
        let (test, dispatch) = log.run("test");
        assert_eq!(dispatch.input_candidate, k1);
        assert_eq!(dispatch.requires, [c1]);
        let k2 = log.tester_edit(test);
        assert_eq!(log.state().tip, k2);
        // C1 is not tip-bound: the tester's derived candidate leaves it current.
        assert!(log.current(c1));
        let evidence = log.tests(test, true);
        assert!(log.current(evidence));

        let (review, dispatch) = log.run("review_change");
        assert_eq!(dispatch.input_candidate, k2);
        assert_eq!(dispatch.requires, [c1, evidence]);
        let approval = log.approve(review);

        assert_eq!(
            log.next(),
            Next::ReadyToComplete {
                candidate: k2,
                tree: "t2".into(),
                evidence,
                approval,
            }
        );
        assert!(log.current(c1) && log.current(plan));
        assert_eq!(log.attempts_of("implement"), 1);
    }

    #[test]
    fn failing_tests_send_the_run_to_the_coder_from_the_tip() {
        let mut log = Log::new();
        let (plan, c1, _) = log.implemented();
        let (test, _) = log.run("test");
        let k2 = log.tester_edit(test);
        let failing = log.tests(test, false);

        let (attempt, dispatch) = log.run("implement");
        assert_eq!(dispatch.input_candidate, k2);
        assert_eq!(dispatch.round, 2);
        assert_eq!(dispatch.resume_session.as_deref(), Some("coder-1"));
        assert_eq!(dispatch.requires, [plan]);
        assert_eq!(dispatch.history, [c1, failing]);

        // The revision makes K3 from K2; the failing evidence is stale.
        let (c2, k3) = log.change(attempt);
        assert_eq!(log.state().candidates.last().unwrap().parent, Some(k2));
        assert!(!log.current(failing));
        let (_, dispatch) = log.run("test");
        assert_eq!(dispatch.input_candidate, k3);
        assert_eq!(dispatch.requires, [c2]);
        assert!(dispatch.history.is_empty());
    }

    #[test]
    fn a_change_rejection_reruns_tests_and_review() {
        let mut log = Log::new();
        let (_, c1, _) = log.implemented();
        let (test, _) = log.run("test");
        let k2 = log.tester_edit(test);
        let e2 = log.tests(test, true);
        let (review, _) = log.run("review_change");
        log.vendor_session(review, "reviewer-1");
        log.request_changes(review);

        let (attempt, dispatch) = log.run("implement");
        assert_eq!(dispatch.input_candidate, k2);
        assert_eq!(dispatch.history, [c1]);
        let (c2, k3) = log.change(attempt);
        assert!(!log.current(e2));

        let (test, dispatch) = log.run("test");
        assert_eq!(dispatch.input_candidate, k3);
        assert_eq!(dispatch.requires, [c2]);
        let e3 = log.tests(test, true);

        let (review, dispatch) = log.run("review_change");
        assert_eq!(dispatch.resume_session, None, "a fresh review");
        assert_eq!(dispatch.requires, [c2, e3]);
        assert_eq!(dispatch.input_candidate, k3);
        let approval = log.approve(review);
        assert_eq!(
            log.next(),
            Next::ReadyToComplete {
                candidate: k3,
                tree: "t3".into(),
                evidence: e3,
                approval,
            }
        );
    }

    #[test]
    fn adopted_drift_reruns_tests_not_the_coder() {
        let mut log = Log::new();
        let (_, c1, _) = log.implemented();
        let (test, _) = log.run("test");
        log.tester_edit(test);
        let e2 = log.tests(test, true);
        let (review, _) = log.run("review_change");
        let r2 = log.approve(review);
        assert!(matches!(log.next(), Next::ReadyToComplete { .. }));

        let adopted = log.adopt_drift();
        assert_eq!(log.state().tip, adopted);
        assert!(log.current(c1));
        assert!(!log.current(e2) && !log.current(r2));

        let (test, dispatch) = log.run("test");
        assert_eq!(dispatch.input_candidate, adopted);
        assert_eq!(dispatch.requires, [c1]);
        let evidence = log.tests(test, true);
        let (review, _) = log.run("review_change");
        let approval = log.approve(review);
        assert_eq!(
            log.next(),
            Next::ReadyToComplete {
                candidate: adopted,
                tree: format!("t{}", adopted.0),
                evidence,
                approval,
            }
        );
        assert_eq!(log.attempts_of("implement"), 1);
    }

    #[test]
    fn an_evidence_phase_resumes_as_evidence() {
        let mut log = Log::new();
        let (_, _, k1) = log.implemented();
        let (test, _) = log.run("test");
        log.start_evidence(test, k1, 0);
        assert_eq!(
            log.next(),
            Next::Evidence {
                attempt: test,
                candidate: k1,
                restart: 0,
            }
        );
        // A restart on a tree the commands changed.
        let k2 = log.propose(Origin::Evidence);
        log.start_evidence(test, k2, 1);
        assert_eq!(
            log.next(),
            Next::Evidence {
                attempt: test,
                candidate: k2,
                restart: 1,
            }
        );
    }

    #[test]
    fn a_superseded_evidence_phase_is_not_resumed() {
        let mut log = Log::new();
        let (_, _, k1) = log.implemented();
        let (stale, dispatch) = log.run("test");
        log.start_evidence(stale, k1, 0);
        // A later attempt of the step supersedes the unfinished one.
        let (test, _) = log.start(dispatch);
        // Only the latest attempt can be live: `next` never resumes the
        // superseded evidence phase (`recover` owns the unfinished latest one).
        assert_eq!(log.next_step().as_deref(), Some("test"));
        log.tests(test, true);
        assert_eq!(log.next_step().as_deref(), Some("review_change"));
    }

    #[test]
    fn a_pause_waits_until_a_decision() {
        let mut log = Log::new();
        let (attempt, _) = log.run("research");
        log.records.push(Record::AttemptInterrupted {
            attempt,
            signal: "SIGINT".into(),
        });
        log.records.push(Record::Paused {
            reason: PauseReason::Interrupted,
            detail: "Ctrl+C".into(),
        });
        assert!(matches!(
            log.next(),
            Next::Pause(PauseReason::Interrupted, _)
        ));
        // A pause during evidence waits too.
        let mut log = Log::new();
        let (_, _, k1) = log.implemented();
        let (test, _) = log.run("test");
        log.start_evidence(test, k1, 0);
        log.records.push(Record::Paused {
            reason: PauseReason::Interrupted,
            detail: String::new(),
        });
        assert!(matches!(
            log.next(),
            Next::Pause(PauseReason::Interrupted, _)
        ));
    }

    /// A log paused for `reason` after the coder's change, with `decision`
    /// logged (and its adopted candidate proposed, for `adopt`).
    fn paused_with(reason: PauseReason, decision: Option<Decision>) -> Log {
        let mut log = Log::new();
        log.implemented();
        log.records.push(Record::Paused {
            reason,
            detail: String::new(),
        });
        if let Some(decision) = decision {
            if let Decision::Adopt { candidate } = decision {
                assert_eq!(log.propose(Origin::Drift), candidate);
            }
            log.records.push(Record::Decision { decision });
        }
        log
    }

    #[test]
    fn wait_kill_release_and_stop_keep_the_pause() {
        let waiting = [
            (PauseReason::ProcessesRunning, Decision::Wait),
            (PauseReason::ProcessesRunning, Decision::Kill),
            (
                PauseReason::ProcessesUnverified,
                Decision::Release { pids: vec![42] },
            ),
            (PauseReason::Stalled, Decision::Stop),
        ];
        for (reason, decision) in waiting {
            let log = paused_with(reason, Some(decision.clone()));
            assert!(
                matches!(log.next(), Next::Pause(r, _) if r == reason),
                "{decision:?}: {:?}",
                log.next()
            );
        }
        // No decision at all waits too.
        let log = paused_with(PauseReason::Drift, None);
        assert!(matches!(log.next(), Next::Pause(PauseReason::Drift, _)));
        // A resuming decision followed by a waiting one waits.
        let mut log = paused_with(PauseReason::ProcessesRunning, Some(Decision::Retry));
        log.records.push(Record::Decision {
            decision: Decision::Wait,
        });
        assert!(matches!(
            log.next(),
            Next::Pause(PauseReason::ProcessesRunning, _)
        ));
    }

    #[test]
    fn adopt_keep_continue_rounds_and_retry_end_the_wait() {
        let resuming = [
            (
                PauseReason::Drift,
                Decision::Adopt {
                    candidate: CandidateId(2),
                },
            ),
            (PauseReason::Reconcile, Decision::Keep),
            (PauseReason::RoundLimit, Decision::Continue),
            (PauseReason::RoundLimit, Decision::Rounds { n: 2 }),
            (PauseReason::TransientExhausted, Decision::Retry),
        ];
        for (reason, decision) in resuming {
            let log = paused_with(reason, Some(decision.clone()));
            assert_eq!(log.next_step().as_deref(), Some("test"), "{decision:?}");
        }
        // A waiting decision followed by a resuming one resumes.
        let mut log = paused_with(PauseReason::ProcessesRunning, Some(Decision::Wait));
        log.records.push(Record::Decision {
            decision: Decision::Retry,
        });
        assert_eq!(log.next_step().as_deref(), Some("test"));
    }

    /// The whole happy path: `(A(P), C, E, R)`, ready to complete.
    fn ready(log: &mut Log) -> (ArtifactId, ArtifactId, ArtifactId, ArtifactId) {
        let (plan, change, _) = log.implemented();
        let (test, _) = log.run("test");
        let evidence = log.tests(test, true);
        let (review, _) = log.run("review_change");
        let approval = log.approve(review);
        assert!(matches!(log.next(), Next::ReadyToComplete { .. }));
        (plan, change, evidence, approval)
    }

    #[test]
    fn a_completed_run_is_done() {
        let mut log = Log::new();
        let (_, _, evidence, approval) = ready(&mut log);
        log.records.push(Record::Completed {
            candidate: CandidateId(1),
            tree: "t1".into(),
            evidence,
            approval,
            observed_tree: "t1".into(),
        });
        assert_eq!(log.next(), Next::Done);
    }

    #[test]
    fn a_stopped_run_is_done() {
        let mut log = paused_with(PauseReason::Stalled, Some(Decision::Stop));
        log.records.push(Record::Stopped);
        assert_eq!(log.next(), Next::Done);
        let mut log = Log::new();
        log.records.push(Record::Stopped);
        assert_eq!(log.next(), Next::Done);
    }

    #[test]
    fn a_failed_run_is_done() {
        let mut log = Log::new();
        log.records.push(Record::Failed { error: "x".into() });
        assert_eq!(log.next(), Next::Done);
    }

    /// Tests pass, then `review_change` asks for changes, resolving every
    /// finding still open.
    fn rejected_round(log: &mut Log) {
        let (test, _) = log.run("test");
        log.tests(test, true);
        let (review, _) = log.run("review_change");
        log.request_changes_resolving(review, usize::MAX);
    }

    /// The coder's revision, which `next` must dispatch.
    fn revised(log: &mut Log) {
        let (attempt, _) = log.run("implement");
        log.change(attempt);
    }

    #[test]
    fn five_jumps_then_a_round_limit() {
        let mut log = Log::new();
        log.implemented();
        let review = log.idx("review_change");
        for jump in 1..=5 {
            rejected_round(&mut log);
            assert_eq!(log.state().jumps[&review], jump);
            revised(&mut log);
        }
        // The sixth jump would exceed max_rounds.
        rejected_round(&mut log);
        assert_eq!(log.state().jumps[&review], 6);
        let detail = log.paused(PauseReason::RoundLimit);
        assert!(
            detail.contains("review_change") && detail.contains("implement"),
            "{detail}"
        );
        log.pause(PauseReason::RoundLimit);
        log.paused(PauseReason::RoundLimit);

        // continue: one round past the limit.
        log.decide(Decision::Continue);
        revised(&mut log);
        rejected_round(&mut log);
        log.paused(PauseReason::RoundLimit);
        log.pause(PauseReason::RoundLimit);

        // rounds 8: the cap is 8 jumps.
        log.decide(Decision::Rounds { n: 8 });
        revised(&mut log);
        rejected_round(&mut log);
        assert_eq!(log.state().jumps[&review], 8);
        revised(&mut log);
        rejected_round(&mut log);
        log.paused(PauseReason::RoundLimit);
        assert_eq!(log.attempts_of("implement"), 9);
    }

    #[test]
    fn rounds_below_the_jumps_still_take_the_jump() {
        let mut log = Log::new();
        log.implemented();
        for _ in 0..5 {
            rejected_round(&mut log);
            revised(&mut log);
        }
        rejected_round(&mut log);
        log.pause(PauseReason::RoundLimit);
        log.decide(Decision::Rounds { n: 2 });
        // A second decision under the same pause changes nothing.
        log.decide(Decision::Continue);
        revised(&mut log);
        let review = log.idx("review_change");
        assert_eq!(log.state().round_caps[&review], 6);
        rejected_round(&mut log);
        log.paused(PauseReason::RoundLimit);
    }

    #[test]
    fn a_round_limit_is_per_edge() {
        let mut log = Log::new();
        log.implemented();
        for _ in 0..5 {
            rejected_round(&mut log);
            revised(&mut log);
        }
        // The tests edge has its own count: a failure is the first jump on it.
        let (test, _) = log.run("test");
        log.tests(test, false);
        revised(&mut log);
    }

    /// Two review rounds on the coder's change; the second resolves
    /// `resolved` of the first's findings and raises one more.
    fn second_rejection(resolved: usize) -> Log {
        let mut log = Log::new();
        log.implemented();
        let (test, _) = log.run("test");
        log.tests(test, true);
        let (review, _) = log.run("review_change");
        log.request_changes(review);
        // The first rejection had no earlier findings: no stall.
        revised(&mut log);
        let (test, _) = log.run("test");
        log.tests(test, true);
        let (review, _) = log.run("review_change");
        log.request_changes_resolving(review, resolved);
        log
    }

    #[test]
    fn a_reviewer_that_resolves_nothing_stalls() {
        // Round 2: F1 stays open, F2 is new.
        let mut log = second_rejection(0);
        let detail = log.paused(PauseReason::Stalled);
        assert!(detail.contains("review_change"), "{detail}");
        log.pause(PauseReason::Stalled);
        log.paused(PauseReason::Stalled);
        // continue takes the jump past the stall, and only that stall.
        log.decide(Decision::Continue);
        revised(&mut log);
        let (test, _) = log.run("test");
        log.tests(test, true);
        let (review, _) = log.run("review_change");
        log.request_changes(review);
        log.paused(PauseReason::Stalled);

        // Resolving one of them is progress.
        let mut log = second_rejection(1);
        revised(&mut log);
    }

    #[test]
    fn no_stall_without_previous_findings() {
        let mut log = Log::new();
        let (attempt, _) = log.run("research");
        log.produce(attempt);
        let (review, _) = log.run("review_research");
        log.request_changes(review);
        assert_eq!(log.next_step().as_deref(), Some("research"));

        // Two approvals, each with nothing earlier to account for.
        let mut log = Log::new();
        log.approved_plan();
        assert_eq!(log.next_step().as_deref(), Some("implement"));

        // A plan review's open findings are not the change review's.
        let mut log = Log::new();
        let (attempt, _) = log.run("research");
        log.produce(attempt);
        let (review, _) = log.run("review_research");
        log.approve(review);
        let (attempt, _) = log.run("plan");
        log.produce(attempt);
        let (review, _) = log.run("review_plan");
        log.request_changes(review);
        let (attempt, _) = log.run("plan");
        log.produce(attempt);
        let (review, _) = log.run("review_plan");
        log.approve(review);
        revised(&mut log);
        rejected_round(&mut log);
        assert_eq!(log.next_step().as_deref(), Some("implement"));
    }

    #[test]
    fn an_unchanged_tree_after_feedback_stalls() {
        let mut log = Log::new();
        let (_, _, k1) = log.implemented();
        let (test, _) = log.run("test");
        log.tests(test, false);
        let (attempt, dispatch) = log.run("implement");
        assert_eq!(dispatch.input_candidate, k1);
        assert!(!dispatch.history.is_empty());
        log.change_tree(attempt, "t1");
        let detail = log.paused(PauseReason::Stalled);
        assert!(detail.contains("implement"), "{detail}");
        log.pause(PauseReason::Stalled);
        log.decide(Decision::Continue);
        assert_eq!(log.next_step().as_deref(), Some("test"));

        // Without feedback, an unchanged tree is not a stall.
        let mut log = Log::new();
        log.approved_plan();
        let (attempt, dispatch) = log.run("implement");
        assert!(dispatch.history.is_empty());
        log.change_tree(attempt, "t0");
        assert_eq!(log.next_step().as_deref(), Some("test"));
    }

    /// Two failing test rounds: the tester leaves `first` then `second` as
    /// the tree, failing `failing` then `again`.
    fn failed_twice(first: &str, failing: Vec<usize>, second: &str, again: Vec<usize>) -> Log {
        let mut log = Log::new();
        log.implemented();
        let (test, _) = log.run("test");
        let candidate = log.propose_tree(Origin::Tests, first);
        log.start_evidence(test, candidate, 0);
        log.tests_failing(test, failing);
        revised(&mut log);
        let (test, _) = log.run("test");
        let candidate = log.propose_tree(Origin::Tests, second);
        log.start_evidence(test, candidate, 0);
        log.tests_failing(test, again);
        log
    }

    #[test]
    fn the_same_failure_twice_stalls() {
        let log = failed_twice("x", vec![0, 1], "x", vec![0, 1]);
        let detail = log.paused(PauseReason::Stalled);
        assert!(detail.contains("test"), "{detail}");
        // Different failing commands, or a different tree, is not a stall.
        let log = failed_twice("x", vec![0, 1], "x", vec![1]);
        assert_eq!(log.next_step().as_deref(), Some("implement"));
        let log = failed_twice("x", vec![0], "y", vec![0]);
        assert_eq!(log.next_step().as_deref(), Some("implement"));
    }

    #[test]
    fn invalid_never_counts() {
        let mut log = Log::new();
        log.implemented();
        let (test, _) = log.run("test");
        log.tests(test, true);
        let (review, _) = log.run("review_change");
        let invalid = || Failure::Invalid {
            detail: "no verdict block".into(),
        };
        log.fail(review, invalid());
        log.paused(PauseReason::InvalidOutput);
        log.pause(PauseReason::InvalidOutput);
        log.decide(Decision::Retry);
        let (review, _) = log.run("review_change");
        log.fail(review, invalid());
        let detail = log.paused(PauseReason::InvalidOutput);
        assert!(detail.contains("no verdict block"), "{detail}");
        assert!(log.state().jumps.is_empty());
    }

    #[test]
    fn transient_failures_retry_then_pause() {
        let mut log = Log::new();
        let (mut attempt, _) = log.run("research");
        for (count, at) in [(1, 60), (2, 300), (3, 900)] {
            log.fail(
                attempt,
                Failure::Transient {
                    detail: "rate limited".into(),
                },
            );
            log.retry(attempt, count, at);
            attempt = log.run_after("research", Some(at)).0;
        }
        log.fail(
            attempt,
            Failure::Transient {
                detail: "rate limited".into(),
            },
        );
        let detail = log.paused(PauseReason::TransientExhausted);
        assert!(detail.contains("rate limited"), "{detail}");
        log.pause(PauseReason::TransientExhausted);
        log.decide(Decision::Retry);
        let (attempt, _) = log.run("research");
        log.produce(attempt);

        // A finished attempt starts the count again; no retry record, no wait.
        let (review, _) = log.run("review_research");
        log.fail(
            review,
            Failure::Transient {
                detail: "overloaded".into(),
            },
        );
        log.run("review_research");
    }

    #[test]
    fn each_failure_pauses_and_retry_runs_the_step_again() {
        let failures = [
            (
                Failure::Invalid {
                    detail: "bad".into(),
                },
                PauseReason::InvalidOutput,
            ),
            (
                Failure::LocalFailure {
                    detail: "disk full".into(),
                },
                PauseReason::LocalFailure,
            ),
            (
                Failure::Unknown {
                    detail: "???".into(),
                },
                PauseReason::Unknown,
            ),
            (
                Failure::AccessViolation {
                    diff_stat: "1 file changed".into(),
                },
                PauseReason::AccessViolation,
            ),
        ];
        for (failure, reason) in failures {
            let mut log = Log::new();
            let (attempt, _) = log.run("research");
            log.produce(attempt);
            let (review, _) = log.run("review_research");
            log.fail(review, failure);
            log.paused(reason);
            log.pause(reason);
            log.paused(reason);
            log.decide(Decision::Retry);
            log.run("review_research");
        }
    }

    #[test]
    fn a_config_error_fails_the_run() {
        let mut log = Log::new();
        let (attempt, _) = log.run("research");
        log.fail(
            attempt,
            Failure::ConfigError {
                detail: "model not found".into(),
            },
        );
        let Next::Fail(error) = log.next() else {
            panic!("{:?}", log.next());
        };
        assert!(
            error.contains("research") && error.contains("model not found"),
            "{error}"
        );
    }

    #[test]
    fn an_interrupted_failure_runs_the_step_again() {
        let mut log = Log::new();
        let (attempt, _) = log.run("research");
        log.fail(
            attempt,
            Failure::Interrupted {
                signal: "SIGTERM".into(),
            },
        );
        log.run("research");
    }

    fn not_ready(log: &Log) {
        let state = log.state();
        assert!(
            !matches!(next(&log.def, &state), Next::ReadyToComplete { .. }),
            "{:?}",
            next(&log.def, &state)
        );
        assert!(
            !matches!(
                ready_to_complete(&log.def, &state),
                Next::ReadyToComplete { .. }
            ),
            "{:?}",
            ready_to_complete(&log.def, &state)
        );
    }

    #[test]
    fn ready_requires_every_part() {
        // An approval for K2 over evidence for K1.
        let mut log = Log::new();
        let (_, c1, _) = log.implemented();
        let (test, _) = log.run("test");
        let e1 = log.tests(test, true);
        let k2 = log.adopt_drift();
        log.forge("review_change", vec![c1, e1], Some(k2), |approval| {
            Accepted::ChangeApproved {
                approval,
                new_findings: vec![],
                previous: vec![],
            }
        });
        not_ready(&log);

        // An approval over failing evidence.
        let mut log = Log::new();
        let (_, c1, k1) = log.implemented();
        let (test, _) = log.run("test");
        let e1 = log.tests(test, false);
        log.forge("review_change", vec![c1, e1], Some(k1), |approval| {
            Accepted::ChangeApproved {
                approval,
                new_findings: vec![],
                previous: vec![],
            }
        });
        not_ready(&log);

        // A stale plan approval: a newer plan document exists.
        let mut log = Log::new();
        let (plan, ..) = ready(&mut log);
        let state = log.state();
        let document = state.artifacts[&plan].requires[0];
        let research = state.artifacts[&document].requires.clone();
        log.forge("plan", research, None, |document| {
            Accepted::DocumentProduced { document }
        });
        assert!(!log.current(plan));
        not_ready(&log);
        assert_eq!(log.next_step().as_deref(), Some("review_plan"));
    }

    #[test]
    fn currency_needs_the_latest_and_current_requirements() {
        let mut log = Log::new();
        assert!(!log.current(ArtifactId(99)), "an unknown artifact");
        let (attempt, _) = log.run("research");
        let d1 = log.produce(attempt);
        let (review, _) = log.run("review_research");
        let a1 = log.approve(review);
        assert!(log.current(a1));
        assert_eq!(log.idx("review_research"), log.state().artifacts[&a1].step);
        assert_eq!(
            log.def.steps[log.idx("review_research")].kind,
            Kind::Verdict
        );

        // A newer document of the same step stales D1 and, through it, A1,
        // even though A1 is still the review step's latest.
        log.records.push(Record::AttemptStarted {
            attempt: AttemptId(9),
            step: "research".into(),
            round: 2,
            input_candidate: CandidateId(0),
            requires: vec![],
            history: vec![d1],
        });
        let d2 = ArtifactId(u32::try_from(log.metas.len()).unwrap());
        log.metas.push(ArtifactMeta {
            id: d2,
            step: log.idx("research"),
            requires: vec![],
            candidate: None,
        });
        log.finish(
            AttemptId(9),
            Accepted::DocumentProduced { document: d2 },
            None,
        );
        assert!(log.current(d2));
        assert!(!log.current(d1));
        assert!(!log.current(a1));
        assert_eq!(log.next_step().as_deref(), Some("review_research"));
    }
}
