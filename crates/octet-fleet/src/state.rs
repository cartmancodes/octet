//! Replay: folds a run's log into the [`RunState`] the machine reads.
//!
//! `replay` is pure. It trusts nothing it cannot check against the workflow:
//! an unknown step, an artifact nobody produced, a candidate no record
//! proposed or a finding ID that is not the next one is a [`ReplayError`], not
//! a state.

use std::collections::{BTreeMap, BTreeSet};

use crate::record::{
    Accepted, ArtifactId, ArtifactMeta, AttemptId, CandidateId, Decision, Failure, Finding, Origin,
    PauseReason, Record, Status,
};
use crate::validate::{Refs, Resolved};

/// A candidate that joined the lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedCandidate {
    /// Its ID.
    pub id: CandidateId,
    /// Its tree object.
    pub tree: String,
    /// The candidate it derives from.
    pub parent: Option<CandidateId>,
    /// What made it.
    pub origin: Origin,
}

/// Where an attempt stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptState {
    /// Started and not finished or failed.
    Unfinished {
        /// An `attempt_session` exists: the child may have run.
        released: bool,
        /// An `attempt_interrupted` exists.
        interrupted: bool,
        /// The latest `evidence_started`: the candidate and the restart.
        evidence: Option<(CandidateId, u32)>,
    },
    /// Accepted.
    Finished,
    /// Failed.
    Failed(Failure),
}

/// What replay keeps of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptInfo {
    /// The step, as an index.
    pub step: usize,
    /// The round it was started in.
    pub round: u32,
    /// The candidate it started from.
    pub input_candidate: CandidateId,
    /// The artifacts it required.
    pub requires: Vec<ArtifactId>,
    /// The artifacts kept to explain it.
    pub history: Vec<ArtifactId>,
    /// The latest vendor session ID it logged.
    pub vendor_session: Option<String>,
    /// The candidate of its latest `evidence_started`: once set, the tester
    /// phase is committed, even if the attempt later fails.
    pub handoff: Option<CandidateId>,
    /// Where it stands.
    pub state: AttemptState,
}

/// Everything the machine decides from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunState {
    /// The lineage, in the order candidates were committed.
    pub candidates: Vec<CommittedCandidate>,
    /// The latest committed candidate.
    pub tip: CandidateId,
    /// Every attempt.
    pub attempts: BTreeMap<AttemptId, AttemptInfo>,
    /// The latest artifact of each step.
    pub latest: Vec<Option<ArtifactId>>,
    /// Whether each step's latest artifact was rejected.
    pub needs_revision: Vec<bool>,
    /// The step whose rejection set each step's `needs_revision` mark; `None`
    /// while unmarked.
    pub rejected_by: Vec<Option<usize>>,
    /// Evidence artifacts accepted as `tests_passed`.
    pub tests_passed: BTreeSet<ArtifactId>,
    /// Every finding of the run, with its current status.
    pub findings: BTreeMap<String, (Finding, Status)>,
    /// Backward jumps taken, by rejecting step.
    pub jumps: BTreeMap<usize, u32>,
    /// Jumps allowed past the cap, by rejecting step: one per `continue`
    /// taken at a round limit.
    pub extra_rounds: BTreeMap<usize, u32>,
    /// The cap a `rounds N` decision set, by rejecting step, replacing
    /// `max_rounds` for it.
    pub round_caps: BTreeMap<usize, u32>,
    /// Stalls a `continue` (or `rounds N`) went past: the stalled step (the
    /// rejector, or the change step) and its jumps (or accepted attempts) at
    /// the time.
    pub stall_waived: BTreeSet<(usize, u32)>,
    /// The latest `retry` record of each failed attempt: its count and when
    /// the retry is eligible, in seconds since the Unix epoch.
    pub retry: BTreeMap<AttemptId, (u32, u64)>,
    /// Every accepted outcome, with the candidate it named.
    pub outcomes: BTreeMap<AttemptId, (Accepted, Option<CandidateId>)>,
    /// The latest vendor session ID of each step.
    pub sessions: Vec<Option<String>>,
    /// Metadata of every artifact supplied to replay.
    pub artifacts: BTreeMap<ArtifactId, ArtifactMeta>,
    /// The pause the run is in, if any.
    pub pending_pause: Option<PauseReason>,
    /// The decisions logged since that pause.
    pub decisions_since_pause: Vec<Decision>,
    /// The PIDs a `release` decision stated are not the run's.
    pub released_pids: BTreeSet<u32>,
    /// The run completed.
    pub completed: bool,
    /// The user stopped the run.
    pub stopped: bool,
    /// The run failed, with its error.
    pub failed: Option<String>,
}

/// The log contradicts the workflow or itself.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("log record {index}: {reason}")]
pub struct ReplayError {
    /// The record's position in the log, from 0.
    pub index: usize,
    /// What is wrong.
    pub reason: String,
}

impl RunState {
    /// The next `count` finding IDs of `step_id`: `<step>/F<n>`, `n` counting
    /// from 1 across the whole run.
    #[must_use]
    pub fn next_finding_ids(&self, step_id: &str, count: usize) -> Vec<String> {
        let last = self
            .findings
            .keys()
            .filter_map(|id| finding_number(step_id, id))
            .max()
            .unwrap_or(0);
        (1..=count)
            .map(|offset| format!("{step_id}/F{}", u64::from(last) + offset as u64))
            .collect()
    }
}

/// `n` of a finding ID `<step_id>/F<n>`; `None` for any other ID.
fn finding_number(step_id: &str, id: &str) -> Option<u32> {
    let digits = id.strip_prefix(step_id)?.strip_prefix("/F")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Folds `records` into a [`RunState`].
///
/// The first record must be the base candidate, which commits itself. Every
/// other candidate is only a proposal until an `evidence_started`, an
/// `attempt_finished` that names it, or a `decision adopt` commits it. A
/// `changes_requested` or `tests_failed` marks the rejector's producer in
/// `needs_revision`, and the producer's next accepted artifact clears the
/// mark. A new attempt or evidence phase starting ends the pause the run was
/// in. An attempt that failed is never finished, except when it is the
/// latest attempt and a `Reconcile` pause logged after its failure adopted a
/// candidate: then a change step's `attempt_finished` naming that candidate
/// finishes it, and a tests step's `evidence_started` on it (restart 0)
/// re-opens it into its evidence phase.
///
/// # Errors
///
/// A [`ReplayError`] naming the first record that contradicts the workflow,
/// the artifacts or the records before it.
pub fn replay(
    def: &Resolved,
    records: &[Record],
    artifacts: &[ArtifactMeta],
) -> Result<RunState, ReplayError> {
    let fail = |index: usize, reason: String| ReplayError { index, reason };
    let base = match records.first() {
        Some(Record::Candidate {
            id,
            tree,
            parent: None,
            origin: Origin::Base,
        }) if *id == CandidateId(0) => CommittedCandidate {
            id: *id,
            tree: tree.clone(),
            parent: None,
            origin: Origin::Base,
        },
        _ => {
            return Err(fail(
                0,
                "the log must start with the base candidate (id 0, no parent)".into(),
            ));
        }
    };
    let steps = def.steps.len();
    let mut replayer = Replayer {
        def,
        proposals: BTreeMap::from([(base.id, base.clone())]),
        reconciling_failure: None,
        state: RunState {
            tip: base.id,
            candidates: vec![base],
            attempts: BTreeMap::new(),
            latest: vec![None; steps],
            needs_revision: vec![false; steps],
            rejected_by: vec![None; steps],
            tests_passed: BTreeSet::new(),
            findings: BTreeMap::new(),
            jumps: BTreeMap::new(),
            extra_rounds: BTreeMap::new(),
            round_caps: BTreeMap::new(),
            stall_waived: BTreeSet::new(),
            retry: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            sessions: vec![None; steps],
            artifacts: artifacts.iter().map(|m| (m.id, m.clone())).collect(),
            pending_pause: None,
            decisions_since_pause: Vec::new(),
            released_pids: BTreeSet::new(),
            completed: false,
            stopped: false,
            failed: None,
        },
    };
    for (index, record) in records.iter().enumerate().skip(1) {
        replayer
            .apply(record)
            .map_err(|reason| fail(index, reason))?;
    }
    Ok(replayer.state)
}

struct Replayer<'a> {
    def: &'a Resolved,
    /// Every candidate record seen, committed or not.
    proposals: BTreeMap<CandidateId, CommittedCandidate>,
    /// The failed attempt the pending `Reconcile` pause was logged for.
    reconciling_failure: Option<AttemptId>,
    state: RunState,
}

impl Replayer<'_> {
    fn apply(&mut self, record: &Record) -> Result<(), String> {
        match record {
            Record::AttemptStarted {
                attempt,
                step,
                round,
                input_candidate,
                requires,
                history,
            } => self.attempt_started(*attempt, step, *round, *input_candidate, requires, history),
            Record::AttemptSession { attempt, .. } => {
                if let AttemptState::Unfinished { released, .. } =
                    &mut self.attempt(*attempt)?.state
                {
                    *released = true;
                }
                Ok(())
            }
            Record::VendorSession { attempt, id } => {
                let info = self.attempt(*attempt)?;
                info.vendor_session = Some(id.clone());
                let step = info.step;
                self.state.sessions[step] = Some(id.clone());
                Ok(())
            }
            Record::Candidate {
                id,
                tree,
                parent,
                origin,
            } => {
                if *origin == Origin::Base {
                    return Err("a second base candidate".into());
                }
                let proposal = CommittedCandidate {
                    id: *id,
                    tree: tree.clone(),
                    parent: *parent,
                    origin: *origin,
                };
                if self.proposals.insert(*id, proposal).is_some() {
                    return Err(format!("candidate {} is proposed twice", id.0));
                }
                Ok(())
            }
            Record::EvidenceStarted {
                attempt,
                candidate,
                restart,
            } => self.evidence_started(*attempt, *candidate, *restart),
            Record::EvidenceCommand { attempt, .. } | Record::EvidenceResult { attempt, .. } => {
                self.attempt(*attempt).map(|_| ())
            }
            Record::Retry {
                attempt,
                count,
                eligible_at,
                ..
            } => {
                self.attempt(*attempt)?;
                self.state.retry.insert(*attempt, (*count, *eligible_at));
                Ok(())
            }
            Record::AttemptFinished {
                attempt,
                outcome,
                candidate,
            } => self.attempt_finished(*attempt, outcome, *candidate),
            Record::AttemptFailed { attempt, failure } => {
                self.open_attempt(*attempt)?;
                self.attempt(*attempt)?.state = AttemptState::Failed(failure.clone());
                Ok(())
            }
            Record::AttemptInterrupted { attempt, .. } => {
                if let AttemptState::Unfinished { interrupted, .. } =
                    &mut self.attempt(*attempt)?.state
                {
                    *interrupted = true;
                }
                Ok(())
            }
            Record::Paused { .. }
            | Record::Decision { .. }
            | Record::Completed { .. }
            | Record::Stopped
            | Record::Failed { .. } => self.run_level(record),
        }
    }

    /// Records about the run, not an attempt.
    fn run_level(&mut self, record: &Record) -> Result<(), String> {
        match record {
            Record::Paused { reason, .. } => {
                self.reconciling_failure = match (reason, self.state.attempts.iter().next_back()) {
                    (PauseReason::Reconcile, Some((id, info)))
                        if matches!(info.state, AttemptState::Failed(_)) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                };
                self.state.pending_pause = Some(*reason);
                self.state.decisions_since_pause.clear();
                Ok(())
            }
            Record::Decision { decision } => {
                match decision {
                    Decision::Adopt { candidate } => self.commit(*candidate)?,
                    Decision::Release { pids } => self.state.released_pids.extend(pids),
                    _ => {}
                }
                self.past_the_limit(decision);
                self.state.decisions_since_pause.push(decision.clone());
                Ok(())
            }
            Record::Completed { .. } => {
                self.state.completed = true;
                Ok(())
            }
            Record::Stopped => {
                self.state.stopped = true;
                Ok(())
            }
            Record::Failed { error } => {
                self.state.failed = Some(error.clone());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn attempt_started(
        &mut self,
        attempt: AttemptId,
        step: &str,
        round: u32,
        input_candidate: CandidateId,
        requires: &[ArtifactId],
        history: &[ArtifactId],
    ) -> Result<(), String> {
        if self.state.attempts.contains_key(&attempt) {
            return Err(format!("attempt {} starts twice", attempt.0));
        }
        let step = self.step_index(step)?;
        if !self
            .state
            .candidates
            .iter()
            .any(|c| c.id == input_candidate)
        {
            return Err(format!(
                "input candidate {} is not committed",
                input_candidate.0
            ));
        }
        for id in requires.iter().chain(history) {
            if !self.state.artifacts.contains_key(id) {
                return Err(format!("artifact {} is unknown", id.0));
            }
        }
        self.resume_from_pause();
        self.state.attempts.insert(
            attempt,
            AttemptInfo {
                step,
                round,
                input_candidate,
                requires: requires.to_vec(),
                history: history.to_vec(),
                vendor_session: None,
                handoff: None,
                state: AttemptState::Unfinished {
                    released: false,
                    interrupted: false,
                    evidence: None,
                },
            },
        );
        Ok(())
    }

    /// The tester-to-evidence handoff, or an evidence restart: commits the
    /// candidate and ends the pause.
    fn evidence_started(
        &mut self,
        attempt: AttemptId,
        candidate: CandidateId,
        restart: u32,
    ) -> Result<(), String> {
        let step = self.attempt(attempt)?.step;
        // A failed tests attempt whose tree was adopted re-opens into its
        // evidence phase on the adopted candidate.
        let reopen = restart == 0
            && matches!(self.def.steps[step].refs, Refs::Tests { .. })
            && self.adopted_failure(attempt, candidate);
        self.commit(candidate)?;
        self.resume_from_pause();
        let info = self.attempt(attempt)?;
        info.handoff = Some(candidate);
        if reopen {
            info.state = AttemptState::Unfinished {
                released: true,
                interrupted: false,
                evidence: None,
            };
        }
        if let AttemptState::Unfinished { evidence, .. } = &mut info.state {
            *evidence = Some((candidate, restart));
        }
        Ok(())
    }

    fn attempt_finished(
        &mut self,
        attempt: AttemptId,
        outcome: &Accepted,
        candidate: Option<CandidateId>,
    ) -> Result<(), String> {
        let adopted = matches!(outcome, Accepted::ChangeProduced { .. })
            && candidate.is_some_and(|k| self.adopted_failure(attempt, k));
        if !adopted {
            self.open_attempt(attempt)?;
        }
        if let Some(candidate) = candidate {
            self.commit(candidate)?;
        }
        self.accept(attempt, outcome)?;
        self.attempt(attempt)?.state = AttemptState::Finished;
        self.state
            .outcomes
            .insert(attempt, (outcome.clone(), candidate));
        Ok(())
    }

    /// Applies an accepted outcome to the artifacts, findings and marks.
    fn accept(&mut self, attempt: AttemptId, outcome: &Accepted) -> Result<(), String> {
        let step = self.attempt(attempt)?.step;
        let refs = self.def.steps[step].refs.clone();
        let step_id = self.def.steps[step].id.clone();
        let mismatch = |what: &str| format!("step {step_id} cannot produce {what}");
        match outcome {
            Accepted::DocumentProduced { document } => {
                if !matches!(refs, Refs::Document { .. }) {
                    return Err(mismatch("a document"));
                }
                self.produced(*document, step)?;
            }
            Accepted::ChangeProduced { change } => {
                if !matches!(refs, Refs::Change { .. }) {
                    return Err(mismatch("a change"));
                }
                self.produced(*change, step)?;
            }
            Accepted::DocumentApproved {
                approval,
                new_findings,
                previous,
            } => {
                if !matches!(refs, Refs::ReviewDocument { .. }) {
                    return Err(mismatch("a document approval"));
                }
                self.own_artifact(*approval, step)?;
                self.ledger(&step_id, new_findings, previous)?;
                self.state.latest[step] = Some(*approval);
            }
            Accepted::ChangeApproved {
                approval,
                new_findings,
                previous,
            } => {
                if !matches!(refs, Refs::ReviewChange { .. }) {
                    return Err(mismatch("a change approval"));
                }
                self.own_artifact(*approval, step)?;
                self.ledger(&step_id, new_findings, previous)?;
                self.state.latest[step] = Some(*approval);
            }
            Accepted::ChangesRequested {
                new_findings,
                previous,
            } => {
                if !matches!(
                    refs,
                    Refs::ReviewDocument { .. } | Refs::ReviewChange { .. }
                ) {
                    return Err(mismatch("a verdict"));
                }
                self.ledger(&step_id, new_findings, previous)?;
                self.reject(step);
            }
            Accepted::TestsPassed { evidence } | Accepted::TestsFailed { evidence, .. } => {
                if !matches!(refs, Refs::Tests { .. }) {
                    return Err(mismatch("test evidence"));
                }
                self.own_artifact(*evidence, step)?;
                self.state.latest[step] = Some(*evidence);
                if matches!(outcome, Accepted::TestsFailed { .. }) {
                    self.reject(step);
                } else {
                    self.state.tests_passed.insert(*evidence);
                }
            }
        }
        Ok(())
    }

    /// A producer's accepted artifact: it is the step's latest, and any
    /// rejection of the artifact before it no longer stands.
    fn produced(&mut self, artifact: ArtifactId, step: usize) -> Result<(), String> {
        self.own_artifact(artifact, step)?;
        self.state.latest[step] = Some(artifact);
        self.state.needs_revision[step] = false;
        self.state.rejected_by[step] = None;
        Ok(())
    }

    /// Marks `rejector`'s producer and counts the jump.
    fn reject(&mut self, rejector: usize) {
        let producer = self.def.producer_of(rejector);
        self.state.needs_revision[producer] = true;
        self.state.rejected_by[producer] = Some(rejector);
        *self.state.jumps.entry(rejector).or_insert(0) += 1;
    }

    /// Checks the verdict's new finding IDs and applies its account of the
    /// step's earlier findings.
    fn ledger(
        &mut self,
        step_id: &str,
        new_findings: &[Finding],
        previous: &[(String, Status)],
    ) -> Result<(), String> {
        let expected = self.state.next_finding_ids(step_id, new_findings.len());
        let given: Vec<&str> = new_findings.iter().map(|f| f.id.as_str()).collect();
        if given != expected {
            return Err(format!(
                "finding IDs [{}] must be [{}]",
                given.join(", "),
                expected.join(", ")
            ));
        }
        for (id, status) in previous {
            if finding_number(step_id, id).is_none() {
                return Err(format!("{id} is not a finding of {step_id}"));
            }
            let Some(entry) = self.state.findings.get_mut(id) else {
                return Err(format!("finding {id} does not exist"));
            };
            entry.1 = *status;
        }
        for finding in new_findings {
            self.state
                .findings
                .insert(finding.id.clone(), (finding.clone(), Status::Open));
        }
        Ok(())
    }

    /// An artifact the attempt's step produced.
    fn own_artifact(&self, id: ArtifactId, step: usize) -> Result<(), String> {
        match self.state.artifacts.get(&id) {
            None => Err(format!("artifact {} is unknown", id.0)),
            Some(meta) if meta.step != step => Err(format!(
                "artifact {} belongs to step {}, not {}",
                id.0, self.def.steps[meta.step].id, self.def.steps[step].id
            )),
            Some(_) => Ok(()),
        }
    }

    /// Joins `id` to the lineage and makes it the tip.
    fn commit(&mut self, id: CandidateId) -> Result<(), String> {
        if !self.state.candidates.iter().any(|c| c.id == id) {
            let Some(proposal) = self.proposals.get(&id) else {
                return Err(format!("candidate {} was never proposed", id.0));
            };
            self.state.candidates.push(proposal.clone());
        }
        self.state.tip = id;
        Ok(())
    }

    /// Applies a `continue` or `rounds N` taken at a round limit or a stall.
    /// A round decision is for the rejector whose jump the run waits to take;
    /// `rounds N` never sets a cap below the jumps already taken, so the
    /// pending jump is always taken. A stall decision waives exactly the stall
    /// the run paused on. Only the first such decision under a pause counts,
    /// so a decision logged twice does not grant two rounds.
    fn past_the_limit(&mut self, decision: &Decision) {
        let reason = self.state.pending_pause;
        let already = self
            .state
            .decisions_since_pause
            .iter()
            .any(|d| matches!(d, Decision::Continue | Decision::Rounds { .. }));
        if already || !matches!(reason, Some(PauseReason::RoundLimit | PauseReason::Stalled)) {
            return;
        }
        let rejector = crate::machine::revising_rejector(self.def, &self.state);
        match (reason, decision, rejector) {
            (Some(PauseReason::RoundLimit), Decision::Continue, Some(rejector)) => {
                let extra = self.state.extra_rounds.entry(rejector).or_insert(0);
                *extra = extra.saturating_add(1);
            }
            (_, Decision::Rounds { n }, Some(rejector)) => {
                let jumps = self.state.jumps.get(&rejector).copied().unwrap_or(0);
                self.state.round_caps.insert(rejector, (*n).max(jumps));
                self.state.extra_rounds.remove(&rejector);
            }
            _ => {}
        }
        if reason == Some(PauseReason::Stalled)
            && matches!(decision, Decision::Continue | Decision::Rounds { .. })
            && let Some(point) = crate::machine::stall_point(self.def, &self.state)
        {
            self.state.stall_waived.insert(point);
        }
    }

    /// `attempt` failed, is the latest attempt, and the pending `Reconcile`
    /// pause logged after its failure adopted `candidate`: its tree may
    /// become the attempt's outcome.
    fn adopted_failure(&self, attempt: AttemptId, candidate: CandidateId) -> bool {
        let latest = self.state.attempts.keys().next_back() == Some(&attempt);
        let failed = self
            .state
            .attempts
            .get(&attempt)
            .is_some_and(|info| matches!(info.state, AttemptState::Failed(_)));
        let adopted = self
            .state
            .decisions_since_pause
            .iter()
            .rev()
            .find_map(|d| match d {
                Decision::Adopt { candidate } => Some(*candidate),
                _ => None,
            });
        latest
            && failed
            && self.reconciling_failure == Some(attempt)
            && self.state.pending_pause == Some(PauseReason::Reconcile)
            && adopted == Some(candidate)
    }

    fn resume_from_pause(&mut self) {
        self.reconciling_failure = None;
        self.state.pending_pause = None;
        self.state.decisions_since_pause.clear();
    }

    fn step_index(&self, id: &str) -> Result<usize, String> {
        self.def
            .steps
            .iter()
            .position(|step| step.id == id)
            .ok_or_else(|| format!("unknown step {id}"))
    }

    fn attempt(&mut self, id: AttemptId) -> Result<&mut AttemptInfo, String> {
        self.state
            .attempts
            .get_mut(&id)
            .ok_or_else(|| format!("attempt {} never started", id.0))
    }

    /// An attempt that can still finish or fail.
    fn open_attempt(&mut self, id: AttemptId) -> Result<(), String> {
        match self.attempt(id)?.state {
            AttemptState::Unfinished { .. } => Ok(()),
            _ => Err(format!("attempt {} already ended", id.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::definition::parse;
    use crate::record::{Identity, Severity};
    use crate::validate::{Host, validate};

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

    fn def() -> Resolved {
        validate(&parse(SHIPPED).unwrap(), &FakeHost).unwrap()
    }

    fn idx(def: &Resolved, id: &str) -> usize {
        def.steps.iter().position(|s| s.id == id).unwrap()
    }

    fn art(def: &Resolved, id: u32, step: &str) -> ArtifactMeta {
        ArtifactMeta {
            id: ArtifactId(id),
            step: idx(def, step),
            requires: vec![],
            candidate: None,
        }
    }

    fn base() -> Record {
        Record::Candidate {
            id: CandidateId(0),
            tree: "t0".into(),
            parent: None,
            origin: Origin::Base,
        }
    }

    fn proposal(id: u32, parent: u32, origin: Origin) -> Record {
        Record::Candidate {
            id: CandidateId(id),
            tree: format!("t{id}"),
            parent: Some(CandidateId(parent)),
            origin,
        }
    }

    fn start(attempt: u32, step: &str) -> Record {
        Record::AttemptStarted {
            attempt: AttemptId(attempt),
            step: step.into(),
            round: 0,
            input_candidate: CandidateId(0),
            requires: vec![],
            history: vec![],
        }
    }

    fn finish(attempt: u32, outcome: Accepted) -> Record {
        Record::AttemptFinished {
            attempt: AttemptId(attempt),
            outcome,
            candidate: None,
        }
    }

    fn finding(id: &str, severity: Severity) -> Finding {
        Finding {
            id: id.into(),
            severity,
            summary: format!("problem {id}"),
            file: None,
        }
    }

    fn run(records: &[Record], artifacts: &[ArtifactMeta]) -> Result<RunState, ReplayError> {
        replay(&def(), records, artifacts)
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one of every record variant")]
    fn records_round_trip_as_snake_case_json() {
        let identity = Identity {
            pid: 7,
            start: "Mon".into(),
        };
        let findings = vec![finding("review_change/F1", Severity::Minor)];
        let previous = vec![("review_change/F1".to_string(), Status::Open)];
        let records = vec![
            start(1, "research"),
            Record::AttemptSession {
                attempt: AttemptId(1),
                journal: PathBuf::from("/tmp/j"),
                identities: vec![identity.clone()],
            },
            Record::VendorSession {
                attempt: AttemptId(1),
                id: "s".into(),
            },
            base(),
            Record::EvidenceStarted {
                attempt: AttemptId(1),
                candidate: CandidateId(2),
                restart: 1,
            },
            Record::EvidenceCommand {
                attempt: AttemptId(1),
                restart: 0,
                index: 0,
                command: "cargo test".into(),
                identity,
                pre_tree: "a".into(),
            },
            Record::EvidenceResult {
                attempt: AttemptId(1),
                restart: 0,
                index: 0,
                exit: Some(1),
                timed_out: false,
                duration_ms: 5,
                post_tree: "b".into(),
            },
            finish(
                1,
                Accepted::DocumentProduced {
                    document: ArtifactId(0),
                },
            ),
            finish(
                1,
                Accepted::DocumentApproved {
                    approval: ArtifactId(1),
                    new_findings: findings.clone(),
                    previous: previous.clone(),
                },
            ),
            finish(
                1,
                Accepted::ChangesRequested {
                    new_findings: findings.clone(),
                    previous: previous.clone(),
                },
            ),
            finish(
                1,
                Accepted::ChangeProduced {
                    change: ArtifactId(2),
                },
            ),
            finish(
                1,
                Accepted::TestsPassed {
                    evidence: ArtifactId(3),
                },
            ),
            finish(
                1,
                Accepted::TestsFailed {
                    evidence: ArtifactId(3),
                    failing: vec![0, 2],
                },
            ),
            Record::AttemptFinished {
                attempt: AttemptId(1),
                outcome: Accepted::ChangeApproved {
                    approval: ArtifactId(4),
                    new_findings: findings,
                    previous,
                },
                candidate: Some(CandidateId(3)),
            },
            Record::AttemptFailed {
                attempt: AttemptId(1),
                failure: Failure::AccessViolation {
                    diff_stat: "1 file".into(),
                },
            },
            Record::AttemptFailed {
                attempt: AttemptId(1),
                failure: Failure::Transient {
                    detail: "429".into(),
                },
            },
            Record::AttemptInterrupted {
                attempt: AttemptId(1),
                signal: "SIGINT".into(),
            },
            Record::Retry {
                attempt: AttemptId(1),
                class: "rate_limit".into(),
                count: 2,
                eligible_at: 99,
            },
            Record::Paused {
                reason: PauseReason::ProcessesUnverified,
                detail: "x".into(),
            },
            Record::Decision {
                decision: Decision::Keep,
            },
            Record::Decision {
                decision: Decision::Adopt {
                    candidate: CandidateId(4),
                },
            },
            Record::Decision {
                decision: Decision::Rounds { n: 2 },
            },
            Record::Decision {
                decision: Decision::Release { pids: vec![1, 2] },
            },
            Record::Decision {
                decision: Decision::Stop,
            },
            Record::Completed {
                candidate: CandidateId(3),
                tree: "t3".into(),
                evidence: ArtifactId(3),
                approval: ArtifactId(4),
                observed_tree: "t3".into(),
            },
            Record::Stopped,
            Record::Failed { error: "e".into() },
        ];
        for record in &records {
            let json = serde_json::to_string(record).unwrap();
            assert!(!json.contains('\n'), "{json}");
            let back: Record = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, record, "{json}");
        }
        let json = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(json["type"], "attempt_started");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["input_candidate"], 0);
        let json = serde_json::to_value(&records[3]).unwrap();
        assert_eq!(json["type"], "candidate");
        assert_eq!(json["origin"], "base");
        let json = serde_json::to_value(&records[7]).unwrap();
        assert_eq!(json["outcome"]["type"], "document_produced");
        assert_eq!(
            serde_json::to_value(Record::Stopped).unwrap(),
            serde_json::json!({"type": "stopped"})
        );
    }

    #[test]
    fn an_uncommitted_candidate_is_ignored() {
        let mut log = vec![base(), proposal(2, 0, Origin::Tests)];
        let state = run(&log, &[]).unwrap();
        assert_eq!(state.tip, CandidateId(0));
        assert_eq!(state.candidates.len(), 1);

        log.push(start(0, "test"));
        log.push(Record::EvidenceStarted {
            attempt: AttemptId(0),
            candidate: CandidateId(2),
            restart: 0,
        });
        let state = run(&log, &[]).unwrap();
        assert_eq!(state.tip, CandidateId(2));
        assert_eq!(state.candidates.len(), 2);
        assert_eq!(state.candidates[1].parent, Some(CandidateId(0)));
        assert_eq!(state.candidates[1].origin, Origin::Tests);

        let change = ArtifactMeta {
            candidate: Some(CandidateId(3)),
            ..art(&def(), 0, "implement")
        };
        log.push(proposal(3, 2, Origin::Change));
        log.push(start(1, "implement"));
        log.push(Record::AttemptFinished {
            attempt: AttemptId(1),
            outcome: Accepted::ChangeProduced {
                change: ArtifactId(0),
            },
            candidate: Some(CandidateId(3)),
        });
        let state = run(&log, &[change]).unwrap();
        assert_eq!(state.tip, CandidateId(3));

        log.push(proposal(4, 3, Origin::Drift));
        assert_eq!(
            run(&log, &[art(&def(), 0, "implement")]).unwrap().tip,
            CandidateId(3)
        );
        log.push(Record::Decision {
            decision: Decision::Adopt {
                candidate: CandidateId(4),
            },
        });
        let state = run(&log, &[art(&def(), 0, "implement")]).unwrap();
        assert_eq!(state.tip, CandidateId(4));
        assert_eq!(state.candidates.len(), 4);
        assert_eq!(
            state.decisions_since_pause,
            vec![Decision::Adopt {
                candidate: CandidateId(4)
            }]
        );
    }

    #[test]
    fn a_committing_record_naming_an_unproposed_candidate_is_an_error() {
        let log = vec![
            base(),
            Record::Decision {
                decision: Decision::Adopt {
                    candidate: CandidateId(9),
                },
            },
        ];
        let error = run(&log, &[]).unwrap_err();
        assert_eq!(error.index, 1);
    }

    #[test]
    fn octet_assigns_finding_ids_across_rounds() {
        let def = def();
        let metas = [
            art(&def, 0, "implement"),
            art(&def, 1, "test"),
            art(&def, 2, "implement"),
            art(&def, 3, "test"),
        ];
        let mut log = vec![base()];
        let reject =
            |log: &mut Vec<Record>, attempt: u32, count: usize, previous: Vec<(String, Status)>| {
                let state = replay(&def, log, &metas).unwrap();
                let ids = state.next_finding_ids("review_change", count);
                log.push(start(attempt, "review_change"));
                log.push(finish(
                    attempt,
                    Accepted::ChangesRequested {
                        new_findings: ids
                            .iter()
                            .map(|id| finding(id, Severity::Blocking))
                            .collect(),
                        previous,
                    },
                ));
                ids
            };
        let first = reject(&mut log, 0, 2, vec![]);
        assert_eq!(first, ["review_change/F1", "review_change/F2"]);
        let state = replay(&def, &log, &metas).unwrap();
        assert_eq!(state.findings.len(), 2);
        assert_eq!(state.findings["review_change/F1"].1, Status::Open);
        assert_eq!(
            state.next_finding_ids("review_change", 1),
            ["review_change/F3"]
        );
        // Another step counts on its own.
        assert_eq!(state.next_finding_ids("review_plan", 1), ["review_plan/F1"]);

        let second = reject(
            &mut log,
            1,
            1,
            vec![
                ("review_change/F1".into(), Status::Resolved),
                ("review_change/F2".into(), Status::Open),
            ],
        );
        assert_eq!(second, ["review_change/F3"]);
        let state = replay(&def, &log, &metas).unwrap();
        assert_eq!(state.findings["review_change/F1"].1, Status::Resolved);
        assert_eq!(state.findings["review_change/F2"].1, Status::Open);
        assert_eq!(state.findings["review_change/F3"].1, Status::Open);
        assert_eq!(state.jumps[&idx(&def, "review_change")], 2);

        // A reviewer cannot renumber: a wrong ID is a replay error.
        log.push(start(2, "review_change"));
        log.push(finish(
            2,
            Accepted::ChangesRequested {
                new_findings: vec![finding("review_change/F9", Severity::Minor)],
                previous: vec![],
            },
        ));
        let error = replay(&def, &log, &metas).unwrap_err();
        assert_eq!(error.index, log.len() - 1);
        assert!(error.reason.contains("review_change/F4"), "{error}");
    }

    #[test]
    fn a_rejection_marks_the_producer() {
        let def = def();
        let plan = idx(&def, "plan");
        let review_plan = idx(&def, "review_plan");
        let metas = [art(&def, 0, "plan"), art(&def, 1, "plan")];
        let mut log = vec![
            base(),
            start(0, "plan"),
            finish(
                0,
                Accepted::DocumentProduced {
                    document: ArtifactId(0),
                },
            ),
            start(1, "review_plan"),
            finish(
                1,
                Accepted::ChangesRequested {
                    new_findings: vec![finding("review_plan/F1", Severity::Blocking)],
                    previous: vec![],
                },
            ),
        ];
        let state = replay(&def, &log, &metas).unwrap();
        assert!(state.needs_revision[plan]);
        assert!(!state.needs_revision[review_plan]);
        assert_eq!(state.jumps[&review_plan], 1);
        // A rejection does not change the rejector's own latest, or the producer's.
        assert_eq!(state.latest[review_plan], None);
        assert_eq!(state.latest[plan], Some(ArtifactId(0)));

        log.push(start(2, "plan"));
        log.push(finish(
            2,
            Accepted::DocumentProduced {
                document: ArtifactId(1),
            },
        ));
        let state = replay(&def, &log, &metas).unwrap();
        assert!(!state.needs_revision[plan]);
        assert_eq!(state.latest[plan], Some(ArtifactId(1)));
        assert_eq!(state.jumps[&review_plan], 1);
    }

    #[test]
    fn failed_tests_mark_the_change_and_keep_their_evidence() {
        let def = def();
        let (implement, test) = (idx(&def, "implement"), idx(&def, "test"));
        let metas = [
            art(&def, 0, "implement"),
            art(&def, 1, "test"),
            art(&def, 2, "implement"),
        ];
        let mut log = vec![
            base(),
            start(0, "implement"),
            finish(
                0,
                Accepted::ChangeProduced {
                    change: ArtifactId(0),
                },
            ),
            start(1, "test"),
            finish(
                1,
                Accepted::TestsFailed {
                    evidence: ArtifactId(1),
                    failing: vec![0],
                },
            ),
        ];
        let state = replay(&def, &log, &metas).unwrap();
        assert!(state.needs_revision[implement]);
        assert_eq!(state.rejected_by[implement], Some(test));
        assert!(state.tests_passed.is_empty());
        assert_eq!(state.jumps[&test], 1);
        assert_eq!(state.latest[test], Some(ArtifactId(1)));
        assert_eq!(state.latest[implement], Some(ArtifactId(0)));

        log.push(start(2, "implement"));
        log.push(finish(
            2,
            Accepted::ChangeProduced {
                change: ArtifactId(2),
            },
        ));
        let state = replay(&def, &log, &metas).unwrap();
        assert!(!state.needs_revision[implement]);
        assert_eq!(state.rejected_by[implement], None);

        log.push(start(3, "test"));
        log.push(finish(
            3,
            Accepted::TestsPassed {
                evidence: ArtifactId(3),
            },
        ));
        let metas = [metas.as_slice(), &[art(&def, 3, "test")]].concat();
        let state = replay(&def, &log, &metas).unwrap();
        assert_eq!(state.tests_passed, BTreeSet::from([ArtifactId(3)]));
    }

    #[test]
    fn an_attempt_without_finished_is_unfinished() {
        let def = def();
        let research = idx(&def, "research");
        let mut log = vec![base(), start(0, "research")];
        let state = replay(&def, &log, &[]).unwrap();
        assert_eq!(
            state.attempts[&AttemptId(0)].state,
            AttemptState::Unfinished {
                released: false,
                interrupted: false,
                evidence: None
            }
        );
        assert_eq!(state.attempts[&AttemptId(0)].step, research);

        log.push(Record::AttemptSession {
            attempt: AttemptId(0),
            journal: PathBuf::from("/j"),
            identities: vec![],
        });
        log.push(Record::VendorSession {
            attempt: AttemptId(0),
            id: "v-1".into(),
        });
        log.push(Record::AttemptInterrupted {
            attempt: AttemptId(0),
            signal: "SIGINT".into(),
        });
        log.push(Record::Paused {
            reason: PauseReason::Interrupted,
            detail: String::new(),
        });
        let state = replay(&def, &log, &[]).unwrap();
        assert_eq!(
            state.attempts[&AttemptId(0)].state,
            AttemptState::Unfinished {
                released: true,
                interrupted: true,
                evidence: None
            }
        );
        assert_eq!(state.sessions[research].as_deref(), Some("v-1"));
        assert_eq!(
            state.attempts[&AttemptId(0)].vendor_session.as_deref(),
            Some("v-1")
        );
        assert_eq!(state.pending_pause, Some(PauseReason::Interrupted));
        assert_eq!(state.latest[research], None);
    }

    #[test]
    fn evidence_started_is_recorded_on_the_attempt() {
        let log = vec![
            base(),
            start(0, "test"),
            Record::EvidenceStarted {
                attempt: AttemptId(0),
                candidate: CandidateId(0),
                restart: 1,
            },
        ];
        let state = run(&log, &[]).unwrap();
        assert_eq!(
            state.attempts[&AttemptId(0)].state,
            AttemptState::Unfinished {
                released: false,
                interrupted: false,
                evidence: Some((CandidateId(0), 1))
            }
        );
    }

    #[test]
    fn a_failed_attempt_keeps_its_failure_and_pauses_track_decisions() {
        let log = vec![
            base(),
            start(0, "implement"),
            Record::AttemptFailed {
                attempt: AttemptId(0),
                failure: Failure::Unknown { detail: "?".into() },
            },
            Record::Paused {
                reason: PauseReason::Unknown,
                detail: "?".into(),
            },
            Record::Decision {
                decision: Decision::Retry,
            },
        ];
        let state = run(&log, &[]).unwrap();
        assert_eq!(
            state.attempts[&AttemptId(0)].state,
            AttemptState::Failed(Failure::Unknown { detail: "?".into() })
        );
        assert_eq!(state.pending_pause, Some(PauseReason::Unknown));
        assert_eq!(state.decisions_since_pause, vec![Decision::Retry]);
        assert!(state.released_pids.is_empty());

        // Released PIDs are kept across pauses.
        let mut released = log.clone();
        released.push(Record::Decision {
            decision: Decision::Release { pids: vec![7, 9] },
        });
        released.push(Record::Paused {
            reason: PauseReason::ProcessesUnverified,
            detail: String::new(),
        });
        released.push(Record::Decision {
            decision: Decision::Release { pids: vec![11] },
        });
        let state = run(&released, &[]).unwrap();
        assert_eq!(state.released_pids, BTreeSet::from([7, 9, 11]));

        // A later pause forgets the decisions made at the earlier one.
        let mut log = log;
        log.push(Record::Paused {
            reason: PauseReason::LocalFailure,
            detail: String::new(),
        });
        let state = run(&log, &[]).unwrap();
        assert!(state.decisions_since_pause.is_empty());

        // An attempt starting means the pause is over.
        log.push(start(1, "implement"));
        let state = run(&log, &[]).unwrap();
        assert_eq!(state.pending_pause, None);
    }

    #[test]
    fn a_failed_attempt_finishes_only_when_its_reconcile_adopted_it() {
        let def = def();
        let artifacts = [art(&def, 0, "implement")];
        let failed = vec![
            base(),
            start(0, "implement"),
            Record::AttemptFailed {
                attempt: AttemptId(0),
                failure: Failure::Transient {
                    detail: "overloaded".into(),
                },
            },
        ];
        let reconcile = Record::Paused {
            reason: PauseReason::Reconcile,
            detail: String::new(),
        };
        let adopt = |k: u32| Record::Decision {
            decision: Decision::Adopt {
                candidate: CandidateId(k),
            },
        };
        let finished = |k: u32| Record::AttemptFinished {
            attempt: AttemptId(0),
            outcome: Accepted::ChangeProduced {
                change: ArtifactId(0),
            },
            candidate: Some(CandidateId(k)),
        };
        let log = |tail: Vec<Record>| [failed.clone(), tail].concat();
        let refused = |tail: Vec<Record>| {
            let error = replay(&def, &log(tail), &artifacts).unwrap_err();
            assert!(error.reason.contains("already ended"), "{error}");
        };
        // No pause, a pause without adopt, another candidate adopted, or a
        // later attempt superseding it: still refused.
        refused(vec![proposal(1, 0, Origin::Change), finished(1)]);
        refused(vec![
            reconcile.clone(),
            proposal(1, 0, Origin::Change),
            finished(1),
        ]);
        refused(vec![
            reconcile.clone(),
            proposal(1, 0, Origin::Change),
            proposal(2, 0, Origin::Change),
            adopt(2),
            finished(1),
        ]);
        refused(vec![
            reconcile.clone(),
            proposal(1, 0, Origin::Change),
            adopt(1),
            start(1, "implement"),
            finished(1),
        ]);
        // A Reconcile after the failure, then adopt: the change is finished.
        let state = replay(
            &def,
            &log(vec![
                reconcile.clone(),
                proposal(1, 0, Origin::Change),
                adopt(1),
                finished(1),
            ]),
            &artifacts,
        )
        .unwrap();
        assert_eq!(state.attempts[&AttemptId(0)].state, AttemptState::Finished);
        assert_eq!(state.tip, CandidateId(1));
        assert_eq!(state.latest[idx(&def, "implement")], Some(ArtifactId(0)));
    }

    #[test]
    fn an_adopted_failed_tests_attempt_reopens_into_evidence() {
        let def = def();
        let reconcile = Record::Paused {
            reason: PauseReason::Reconcile,
            detail: String::new(),
        };
        let adopt = Record::Decision {
            decision: Decision::Adopt {
                candidate: CandidateId(1),
            },
        };
        let tests = |tail: Vec<Record>| {
            let mut log = vec![
                base(),
                start(0, "test"),
                Record::AttemptFailed {
                    attempt: AttemptId(0),
                    failure: Failure::Unknown { detail: "?".into() },
                },
            ];
            log.extend(tail);
            log.push(Record::EvidenceStarted {
                attempt: AttemptId(0),
                candidate: CandidateId(1),
                restart: 0,
            });
            replay(&def, &log, &[]).unwrap().attempts[&AttemptId(0)]
                .state
                .clone()
        };
        assert_eq!(
            tests(vec![
                reconcile.clone(),
                proposal(1, 0, Origin::Tests),
                adopt
            ]),
            AttemptState::Unfinished {
                released: true,
                interrupted: false,
                evidence: Some((CandidateId(1), 0)),
            }
        );
        assert!(matches!(
            tests(vec![reconcile, proposal(1, 0, Origin::Tests)]),
            AttemptState::Failed(_)
        ));
    }

    #[test]
    fn terminal_records_set_their_flags() {
        let mut log = vec![base()];
        log.push(Record::Completed {
            candidate: CandidateId(0),
            tree: "t0".into(),
            evidence: ArtifactId(0),
            approval: ArtifactId(1),
            observed_tree: "t0".into(),
        });
        let state = run(&log, &[]).unwrap();
        assert!(state.completed && !state.stopped && state.failed.is_none());
        let state = run(&[base(), Record::Stopped], &[]).unwrap();
        assert!(state.stopped && !state.completed);
        let state = run(
            &[
                base(),
                Record::Failed {
                    error: "boom".into(),
                },
            ],
            &[],
        )
        .unwrap();
        assert_eq!(state.failed.as_deref(), Some("boom"));
    }

    #[test]
    fn replay_refuses_logs_that_contradict_the_workflow() {
        let def = def();
        let metas = [art(&def, 0, "research")];
        let doc = || {
            finish(
                0,
                Accepted::DocumentProduced {
                    document: ArtifactId(0),
                },
            )
        };
        // No base candidate first.
        assert_eq!(replay(&def, &[], &[]).unwrap_err().index, 0);
        assert_eq!(
            replay(&def, &[start(0, "research")], &[])
                .unwrap_err()
                .index,
            0
        );
        // Unknown step.
        let error = replay(&def, &[base(), start(0, "nope")], &[]).unwrap_err();
        assert_eq!((error.index, error.reason.contains("nope")), (1, true));
        // Record for an attempt that never started.
        let error = replay(&def, &[base(), doc()], &metas).unwrap_err();
        assert_eq!(error.index, 1);
        // Duplicate attempt ID.
        let log = [base(), start(0, "research"), start(0, "research")];
        assert_eq!(replay(&def, &log, &[]).unwrap_err().index, 2);
        // Artifact nobody supplied.
        let log = [base(), start(0, "research"), doc()];
        assert_eq!(replay(&def, &log, &[]).unwrap_err().index, 2);
        // Artifact produced by another step.
        let log = [base(), start(0, "plan"), doc()];
        assert_eq!(replay(&def, &log, &metas).unwrap_err().index, 2);
        // Outcome of the wrong kind for the step.
        let log = [base(), start(0, "implement"), doc()];
        assert_eq!(replay(&def, &log, &metas).unwrap_err().index, 2);
        // Finished twice.
        let log = [base(), start(0, "research"), doc(), doc()];
        assert_eq!(replay(&def, &log, &metas).unwrap_err().index, 3);
        // A second base, or a duplicate candidate ID.
        assert_eq!(replay(&def, &[base(), base()], &[]).unwrap_err().index, 1);
        // `previous` naming a finding of another step.
        let log = [
            base(),
            start(0, "review_plan"),
            finish(
                0,
                Accepted::ChangesRequested {
                    new_findings: vec![],
                    previous: vec![("review_plan/F1".into(), Status::Resolved)],
                },
            ),
        ];
        assert_eq!(replay(&def, &log, &[]).unwrap_err().index, 2);
    }
}
