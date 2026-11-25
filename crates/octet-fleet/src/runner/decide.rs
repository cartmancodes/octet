//! Taking the user's decision at a pause (spec, "Recovery and decisions").
//!
//! The leftover processes are observed before anything is logged: while a
//! verified process of the run is alive, or an unverified one is not
//! released, the decision the run waits for is about them, whatever pause
//! is pending, so a `keep` or an `adopt` is refused rather than logged (a
//! later processes pause would otherwise clear it).

use super::{Driver, Flow, RunEnd, RunnerError, View};
use crate::definition::Kind;
use crate::ownership;
use crate::record::{CandidateId, Decision, Origin, PauseReason, Record};
use crate::recover::{self, Observation, ProcessCheck};
use crate::state::RunState;

impl Driver {
    /// Logs `decision` and what it needs first (an adopted candidate), or
    /// stops the run, or refuses it.
    pub(super) async fn decide(
        &mut self,
        view: &View,
        obs: &Observation,
        decision: Decision,
    ) -> Result<Flow, RunnerError> {
        if decision == Decision::Stop {
            return self.stop_run(view, obs).await.map(Some);
        }
        let released = |pid: &u32| view.state.released_pids.contains(pid);
        let reason = match &obs.processes {
            ProcessCheck::Verified(pids) if !pids.is_empty() => Some(PauseReason::ProcessesRunning),
            ProcessCheck::Unverified(pids) if pids.iter().any(|pid| !released(pid)) => {
                Some(PauseReason::ProcessesUnverified)
            }
            _ => view.state.pending_pause,
        };
        let Some(reason) = reason else {
            return Err(RunnerError::Refused(format!(
                "run {} is not paused; it needs no decision",
                self.id
            )));
        };
        if retries_a_decided_pause(&view.state, reason, &decision) {
            // A local failure stopped the run after this pause's decision:
            // `retry` lets that decision stand, and logs nothing.
            return Ok(None);
        }
        if !recover::allows(reason, &decision) {
            return Err(RunnerError::Refused(format!(
                "the run is paused ({reason:?}); its decisions are {}",
                recover::options(reason).join(", ")
            )));
        }
        let decision = match decision {
            Decision::Kill => {
                if let ProcessCheck::Verified(pids) = &obs.processes {
                    self.append(&Record::Decision {
                        decision: Decision::Kill,
                    })
                    .await?;
                    ownership::kill_verified(pids).await;
                }
                return Ok(None);
            }
            Decision::Release { pids: named } => {
                // The PIDs named that are unverified and not yet released;
                // none named releases every one listed.
                let pids = match &obs.processes {
                    ProcessCheck::Unverified(pids) => pids
                        .iter()
                        .copied()
                        .filter(|pid| !released(pid))
                        .filter(|pid| named.is_empty() || named.contains(pid))
                        .collect(),
                    _ => Vec::new(),
                };
                Decision::Release { pids }
            }
            Decision::Adopt { .. } => {
                let parent = adopted_parent(view, reason);
                let origin = adopted_origin(self, view, reason);
                let candidate = self.propose(view, &obs.live_tree, parent, origin).await?;
                Decision::Adopt { candidate }
            }
            other => other,
        };
        self.append(&Record::Decision { decision }).await?;
        Ok(None)
    }

    /// `stop`: refused while a verified process is alive or an unverified
    /// one is not released; otherwise `stopped`, and the run no longer owns
    /// the work tree. The tree is left as it is; the latest attempt's
    /// scratch clone, if one is left, is removed (best effort: the run has
    /// stopped either way).
    async fn stop_run(&mut self, view: &View, obs: &Observation) -> Result<RunEnd, RunnerError> {
        match &obs.processes {
            ProcessCheck::Verified(pids) if !pids.is_empty() => {
                return Err(RunnerError::Refused(format!(
                    "processes of run {} are still running ({pids:?}); kill them or wait first",
                    self.id
                )));
            }
            ProcessCheck::Unverified(pids)
                if pids
                    .iter()
                    .any(|pid| !view.state.released_pids.contains(pid)) =>
            {
                return Err(RunnerError::Refused(format!(
                    "processes that may be run {}'s are alive ({pids:?}); release them first",
                    self.id
                )));
            }
            _ => {}
        }
        self.append(&Record::Decision {
            decision: Decision::Stop,
        })
        .await?;
        self.append(&Record::Stopped).await?;
        self.disown().await?;
        // A read-only attempt left unfinished keeps its scratch clone (a
        // full checkout) until the next attempt; a stopped run has none.
        if let Some(&latest) = view.state.attempts.keys().next_back() {
            let _ = tokio::fs::remove_dir_all(self.attempt_dir(latest).join("tree")).await;
        }
        Ok(RunEnd::Stopped)
    }
}

/// `decision` is a `retry` the pending pause `reason` does not take, and that
/// pause has its decision already: a local failure (not logged over it)
/// stopped the run, and the front end asked to `retry`.
fn retries_a_decided_pause(state: &RunState, reason: PauseReason, decision: &Decision) -> bool {
    *decision == Decision::Retry
        && state.pending_pause == Some(reason)
        && !recover::allows(reason, decision)
        && super::decided(state)
}

/// The candidate adopted drift derives from: the tip; for a `Reconcile`,
/// the unfinished attempt's input.
fn adopted_parent(view: &View, reason: PauseReason) -> CandidateId {
    match (reason, view.state.attempts.values().next_back()) {
        (PauseReason::Reconcile, Some(info)) => info.input_candidate,
        _ => view.state.tip,
    }
}

/// What made an adopted tree: drift, or the reconciled attempt's step.
fn adopted_origin(driver: &Driver, view: &View, reason: PauseReason) -> Origin {
    match (reason, view.state.attempts.values().next_back()) {
        (PauseReason::Reconcile, Some(info)) => match driver.def.steps[info.step].kind {
            Kind::Tests => Origin::Tests,
            Kind::Change | Kind::Document | Kind::Verdict => Origin::Change,
        },
        _ => Origin::Drift,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::replay;

    #[test]
    fn retry_lets_a_decided_pause_stand_and_nothing_else() {
        let base = Record::Candidate {
            id: CandidateId(0),
            tree: "t0".into(),
            parent: None,
            origin: Origin::Base,
        };
        let mut state = replay(&crate::sim::shipped(), &[base], &[]).unwrap();
        state.pending_pause = Some(PauseReason::Reconcile);
        // Undecided: `retry` is not one of its decisions.
        assert!(!retries_a_decided_pause(
            &state,
            PauseReason::Reconcile,
            &Decision::Retry
        ));
        state.decisions_since_pause = vec![Decision::Keep];
        assert!(retries_a_decided_pause(
            &state,
            PauseReason::Reconcile,
            &Decision::Retry
        ));
        assert!(!retries_a_decided_pause(
            &state,
            PauseReason::Reconcile,
            &Decision::Keep
        ));
        // A pause that takes `retry` logs it as usual.
        state.pending_pause = Some(PauseReason::UnstableTests);
        state.decisions_since_pause = vec![Decision::Retry];
        assert!(!retries_a_decided_pause(
            &state,
            PauseReason::UnstableTests,
            &Decision::Retry
        ));
    }
}
