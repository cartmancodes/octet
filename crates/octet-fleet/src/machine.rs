//! The machine: a pure function of the replayed log that decides what runs
//! next.
//!
//! [`next`] reads a [`RunState`] and nothing else: no I/O, no clock. It runs
//! the first step, in order, whose latest artifact is missing, not current
//! or needs revision, and reports the run ready to complete once every step's
//! artifact is current and the final approval binds passing evidence to the
//! tip. Retry timing, rounds, stalls and the decisions taken at a pause build
//! on this (`not_before` is where retry state will go).

use crate::definition::SessionPolicy;
use crate::record::{ArtifactId, AttemptId, CandidateId, Decision, PauseReason};
use crate::state::{AttemptState, RunState};
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
    /// The run completed, stopped or failed.
    Done,
}

/// Decides what runs next from the replayed log.
///
/// In order: a run that ended is done; a pause waits unless its latest
/// decision resumes the run (keep, adopt, continue, rounds, retry);
/// a tests attempt whose evidence phase began resumes it; otherwise the first
/// step whose latest artifact is missing, not current or needs revision runs;
/// with every step settled, the run is ready to complete if the final
/// approval binds passing evidence to the tip.
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
    if let Some(evidence) = evidence_in_progress(def, state) {
        return evidence;
    }
    for step in 0..def.steps.len() {
        let settled = state.latest[step].is_some_and(|artifact| is_current(def, state, artifact))
            && !state.needs_revision[step];
        if !settled {
            return Next::Run(dispatch(def, state, step));
        }
    }
    ready_to_complete(def, state)
}

/// Whether a decision lets the run go on from its pause. `wait`, `kill` and
/// `release` leave it waiting on its processes, and `stop` ends it (its
/// `stopped` record may not be durable yet); the others resume.
fn ends_the_wait(decision: &Decision) -> bool {
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

/// The latest unfinished tests attempt whose evidence phase began.
fn evidence_in_progress(def: &Resolved, state: &RunState) -> Option<Next> {
    state.attempts.iter().rev().find_map(|(attempt, info)| {
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
    })
}

fn dispatch(def: &Resolved, state: &RunState, step: usize) -> Dispatch {
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
        not_before: None,
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

/// Every step is settled: ready when the final approval is for the tip and
/// requires passing evidence for the tip.
fn ready_to_complete(def: &Resolved, state: &RunState) -> Next {
    let unsatisfied = || {
        Next::Pause(
            PauseReason::Unknown,
            "every step is settled but the logged artifacts do not satisfy the completion invariant"
                .into(),
        )
    };
    let Some((last, step)) = def.steps.iter().enumerate().next_back() else {
        return unsatisfied();
    };
    let Refs::ReviewChange {
        evidence: tests_step,
        ..
    } = step.refs
    else {
        return unsatisfied();
    };
    let Some(approval) = state.latest[last] else {
        return unsatisfied();
    };
    let for_tip = |artifact: ArtifactId| {
        state
            .artifacts
            .get(&artifact)
            .is_some_and(|meta| meta.candidate == Some(state.tip))
    };
    let evidence = state.artifacts.get(&approval).and_then(|meta| {
        meta.requires.iter().copied().find(|required| {
            state
                .artifacts
                .get(required)
                .is_some_and(|meta| meta.step == tests_step)
        })
    });
    let tree = state
        .candidates
        .iter()
        .find(|candidate| candidate.id == state.tip)
        .map(|candidate| candidate.tree.clone());
    match (evidence, tree) {
        (Some(evidence), Some(tree))
            if for_tip(approval) && for_tip(evidence) && state.tests_passed.contains(&evidence) =>
        {
            Next::ReadyToComplete {
                candidate: state.tip,
                tree,
                evidence,
                approval,
            }
        }
        _ => unsatisfied(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::definition::{Kind, parse};
    use crate::record::{
        Accepted, ArtifactMeta, Decision, Finding, Origin, Record, Severity, Status,
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
            let next = self.next();
            let Next::Run(dispatch) = next else {
                panic!("expected Run({step}), got {next:?}");
            };
            assert_eq!(self.def.steps[dispatch.step].id, step, "{dispatch:?}");
            assert_eq!(dispatch.not_before, None);
            let attempt = AttemptId(u32::try_from(self.dispatched.len()).unwrap());
            self.records.push(Record::AttemptStarted {
                attempt,
                step: step.into(),
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
            let id = CandidateId(self.next_candidate);
            self.next_candidate += 1;
            self.records.push(Record::Candidate {
                id,
                tree: format!("t{}", id.0),
                parent: Some(self.state().tip),
                origin,
            });
            id
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
            let candidate = self.propose(Origin::Change);
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
            let step = self.def.steps[self.dispatched[&attempt].step].id.clone();
            let state = self.state();
            let previous = state
                .findings
                .iter()
                .filter(|(id, (_, status))| {
                    id.starts_with(&format!("{step}/")) && *status == Status::Open
                })
                .map(|(id, _)| (id.clone(), Status::Open))
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
        /// edit) and passes or fails.
        fn tests(&mut self, attempt: AttemptId, pass: bool) -> ArtifactId {
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
            let outcome = if pass {
                Accepted::TestsPassed { evidence }
            } else {
                Accepted::TestsFailed {
                    evidence,
                    failing: vec![0],
                }
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

    #[test]
    fn a_finished_run_is_done() {
        for end in [Record::Stopped, Record::Failed { error: "x".into() }] {
            let mut log = Log::new();
            log.records.push(end);
            assert_eq!(log.next(), Next::Done);
        }
        let mut log = Log::new();
        let (_, _, _) = log.implemented();
        let (test, _) = log.run("test");
        let evidence = log.tests(test, true);
        let (review, _) = log.run("review_change");
        let approval = log.approve(review);
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
