//! Recovery: what a driver does with the log and the workspace it finds.
//!
//! [`recover`] is pure. The runner calls it before every action, with what it
//! has just observed: the live tree, whether the latest attempt's `go` file
//! exists, and the leftover processes of unfinished attempts. It applies the
//! spec's "State found" table to the latest attempt and the decision table
//! to a pause the user has decided on, and otherwise falls through to
//! [`machine::next`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::definition::Kind;
use crate::machine::{self, Dispatch, Next};
use crate::record::{AttemptId, Decision, Failure, PauseReason};
use crate::state::{AttemptInfo, AttemptState, RunState};
use crate::validate::Resolved;

/// What the runner observed before deciding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The live work tree's tree object, from a fresh snapshot.
    pub live_tree: String,
    /// The latest attempt's `<attempt>/go` file exists: its vendor may have
    /// run.
    pub go_created: bool,
    /// The leftover processes of unfinished attempts.
    pub processes: ProcessCheck,
    /// The command line of each unverified process, for the pause that lists
    /// them; a PID missing here is listed without one.
    pub commands: BTreeMap<u32, String>,
}

/// The leftover processes found (spec, "Leftover processes").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessCheck {
    /// None alive.
    Clear,
    /// These live processes are verified as the run's.
    Verified(Vec<u32>),
    /// These live processes are in a registered group but could be anyone's.
    Unverified(Vec<u32>),
}

/// Decides what the runner does next, given the replayed log and what it
/// observed.
///
/// In order: a run that ended is done; live verified processes pause
/// (`ProcessesRunning`), and so do unverified ones not yet released
/// (`ProcessesUnverified`); a pause the user decided on takes the decision's
/// next action (a `stop`, or no resuming decision, keeps waiting); a pause
/// for processes or an interrupt needs no decision: the observation answers
/// it, and the run goes on as if it had not paused. Then the latest attempt
/// decides (only it can be live; a later `attempt_started` supersedes every
/// earlier unfinished attempt):
///
/// - unfinished and read-only: a new attempt of the same step;
/// - unfinished, its evidence phase begun: the evidence on that phase's
///   candidate from command zero when the tree is the candidate's, `Drift`
///   otherwise;
/// - unfinished, never released (no `attempt_session`, or no `go`): a new
///   attempt when the tree is the attempt's input tree, `Drift` otherwise;
/// - unfinished and released: a new attempt (a change step's continuing its
///   vendor session) when the tree is the input tree, `Reconcile` otherwise;
/// - a write attempt that failed (not a config error) and left the tree
///   changed: `Reconcile`; but a tests attempt that failed after its
///   handoff compares with its evidence candidate, and another tree is
///   `Drift`;
/// - anything else (finished, or failed with the tree unchanged): `Drift`
///   when the tree is not the tip's, otherwise [`machine::next`].
///
/// A read-only attempt never reconciles: its clone is discarded; a change to
/// the live tree beside it is drift. "A new attempt" is the dispatch
/// [`machine::next`] builds for the step, in the unfinished attempt's round.
///
/// `state` must be [`crate::state::replay`]'s output for this `def`.
#[must_use]
pub fn recover(def: &Resolved, state: &RunState, obs: &Observation) -> Next {
    if state.completed || state.stopped || state.failed.is_some() {
        return machine::next(def, state);
    }
    if let Some(pause) = leftover_processes(state, &obs.processes, &obs.commands) {
        return pause;
    }
    let decision = state.decisions_since_pause.last();
    match state.pending_pause {
        Some(_) if decision == Some(&Decision::Stop) => return machine::next(def, state),
        None => {}
        Some(
            PauseReason::Interrupted
            | PauseReason::ProcessesRunning
            | PauseReason::ProcessesUnverified,
        ) => {
            // The observation just made answers the pause: it is over.
            let lifted = RunState {
                pending_pause: None,
                decisions_since_pause: Vec::new(),
                ..state.clone()
            };
            return found(def, &lifted, obs);
        }
        Some(reason) => {
            let Some(decision) = decision.filter(|d| machine::ends_the_wait(d)) else {
                return machine::next(def, state);
            };
            if let Some(next) = decided(def, state, obs, reason, decision) {
                return next;
            }
        }
    }
    found(def, state, obs)
}

/// Verified processes alive, or unverified ones the user has not released;
/// each is listed with its command (spec, "Leftover processes").
fn leftover_processes(
    state: &RunState,
    check: &ProcessCheck,
    commands: &BTreeMap<u32, String>,
) -> Option<Next> {
    match check {
        ProcessCheck::Clear => None,
        ProcessCheck::Verified(pids) => (!pids.is_empty()).then(|| {
            let mut detail = format!("processes of the run are still running: {pids:?}");
            list_commands(&mut detail, pids, commands);
            Next::Pause(PauseReason::ProcessesRunning, detail)
        }),
        ProcessCheck::Unverified(pids) => {
            let left: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| !state.released_pids.contains(pid))
                .collect();
            (!left.is_empty()).then(|| {
                let mut detail = format!("processes that may be the run's are alive: {left:?}");
                list_commands(&mut detail, &left, commands);
                Next::Pause(PauseReason::ProcessesUnverified, detail)
            })
        }
    }
}

/// Each of `pids` on a line of its own, with its command, so the user
/// knows what a `kill` or a `release` is about.
fn list_commands(detail: &mut String, pids: &[u32], commands: &BTreeMap<u32, String>) {
    for pid in pids {
        let command = commands
            .get(pid)
            .map_or("(command unknown)", String::as_str);
        let _ = write!(detail, "\n  {pid}: {command}");
    }
}

/// The next action of a resuming decision at a pause, when it is not
/// [`found`]'s: `keep` and `adopt` at a `Reconcile`, `retry` at
/// `UnstableTests`, and `adopt` at `Drift`.
fn decided(
    def: &Resolved,
    state: &RunState,
    obs: &Observation,
    reason: PauseReason,
    decision: &Decision,
) -> Option<Next> {
    if let (PauseReason::Drift, Decision::Adopt { .. }) = (reason, decision) {
        // The decision committed the adopted candidate; it is the tip.
        return Some(durable(def, state, obs));
    }
    let (&attempt, info) = state.attempts.iter().next_back()?;
    let kind = def.steps[info.step].kind;
    match (reason, decision, &info.state) {
        (
            PauseReason::Reconcile,
            Decision::Keep,
            AttemptState::Unfinished { .. } | AttemptState::Failed(_),
        ) => {
            let mut dispatch = continued(def, state, info);
            // A kept transient failure still waits out its backoff.
            if let AttemptState::Failed(Failure::Transient { eligible_at, .. }) = info.state {
                dispatch.not_before = machine::retry_eligible(state, attempt, eligible_at);
            }
            Some(Next::Run(dispatch))
        }
        // An unfinished attempt, or one that failed and left the tree
        // changed: replay lets the adopting record finish (or re-open) it.
        (
            PauseReason::Reconcile,
            Decision::Adopt { candidate },
            AttemptState::Unfinished { .. } | AttemptState::Failed(_),
        ) => match kind {
            Kind::Change => Some(Next::FinishAdopted {
                attempt,
                candidate: *candidate,
            }),
            Kind::Tests => Some(Next::Evidence {
                attempt,
                candidate: *candidate,
                restart: 0,
            }),
            Kind::Document | Kind::Verdict => None,
        },
        (
            PauseReason::UnstableTests,
            Decision::Retry,
            AttemptState::Unfinished {
                evidence: Some(_), ..
            },
        ) => Some(Next::Evidence {
            attempt,
            candidate: state.tip,
            restart: 0,
        }),
        _ => None,
    }
}

/// The "State found" table, on the latest attempt.
fn found(def: &Resolved, state: &RunState, obs: &Observation) -> Next {
    let Some((&attempt, info)) = state.attempts.iter().next_back() else {
        return durable(def, state, obs);
    };
    let step = &def.steps[info.step];
    let input_tree = machine::tree_of(state, info.input_candidate);
    let changed = input_tree != Some(obs.live_tree.as_str());
    match &info.state {
        AttemptState::Finished => durable(def, state, obs),
        AttemptState::Failed(failure) => {
            if let Some(candidate) = info.handoff {
                // After the handoff the tester phase is committed: the tree
                // is compared with the evidence candidate's, and another
                // tree is drift.
                return if machine::tree_of(state, candidate) == Some(obs.live_tree.as_str()) {
                    durable(def, state, obs)
                } else {
                    drift_pause(
                        obs,
                        machine::tree_of(state, candidate),
                        &format!("attempt {}'s evidence", attempt.0),
                    )
                };
            }
            let reconcile = !step.read_only() && !matches!(failure, Failure::ConfigError { .. });
            if reconcile && changed {
                reconcile_pause(def, attempt, info)
            } else {
                durable(def, state, obs)
            }
        }
        AttemptState::Unfinished {
            released, evidence, ..
        } => {
            if step.read_only() {
                return drift(state, obs)
                    .unwrap_or_else(|| Next::Run(machine::again(def, state, info)));
            }
            if let Some((candidate, restart)) = *evidence {
                return if machine::tree_of(state, candidate) == Some(obs.live_tree.as_str()) {
                    Next::Evidence {
                        attempt,
                        candidate,
                        restart,
                    }
                } else {
                    drift_pause(
                        obs,
                        machine::tree_of(state, candidate),
                        &format!("attempt {}'s evidence", attempt.0),
                    )
                };
            }
            if !(*released && obs.go_created) {
                return if changed {
                    drift_pause(
                        obs,
                        input_tree,
                        &format!("attempt {}, which never ran", attempt.0),
                    )
                } else {
                    Next::Run(machine::again(def, state, info))
                };
            }
            if changed {
                reconcile_pause(def, attempt, info)
            } else if step.kind == Kind::Change {
                Next::Run(continued(def, state, info))
            } else {
                Next::Run(machine::again(def, state, info))
            }
        }
    }
}

/// The decisions open at a pause, as the user names them (spec, "Recovery
/// and decisions"). `stop` ends every pause; an interrupt needs no decision.
#[must_use]
pub fn options(reason: PauseReason) -> Vec<&'static str> {
    let own: &[&str] = match reason {
        PauseReason::Reconcile => &["keep", "adopt"],
        PauseReason::RoundLimit | PauseReason::Stalled => &["continue", "rounds"],
        PauseReason::Drift => &["adopt"],
        PauseReason::ProcessesRunning => &["kill", "wait"],
        PauseReason::ProcessesUnverified => &["release", "wait"],
        PauseReason::Interrupted => &[],
        PauseReason::UnstableTests
        | PauseReason::AccessViolation
        | PauseReason::InvalidOutput
        | PauseReason::TransientExhausted
        | PauseReason::LocalFailure
        | PauseReason::Unknown => &["retry"],
    };
    own.iter().copied().chain(["stop"]).collect()
}

/// Whether `decision` is one of [`options`]`(reason)`.
#[must_use]
pub fn allows(reason: PauseReason, decision: &Decision) -> bool {
    let name = match decision {
        Decision::Keep => "keep",
        Decision::Adopt { .. } => "adopt",
        Decision::Continue => "continue",
        Decision::Rounds { .. } => "rounds",
        Decision::Retry => "retry",
        Decision::Kill => "kill",
        Decision::Wait => "wait",
        Decision::Release { .. } => "release",
        Decision::Stop => "stop",
    };
    options(reason).contains(&name)
}

/// A new attempt of `info`'s step that continues its vendor session: the
/// one it logged, else the one it was dispatched to continue, else fresh.
fn continued(def: &Resolved, state: &RunState, info: &AttemptInfo) -> Dispatch {
    let again = machine::again(def, state, info);
    Dispatch {
        resume_session: info.vendor_session.clone().or(again.resume_session.clone()),
        ..again
    }
}

/// Nothing is unfinished: `Drift` when the live tree is not the tip's,
/// otherwise [`machine::next`].
fn durable(def: &Resolved, state: &RunState, obs: &Observation) -> Next {
    drift(state, obs).unwrap_or_else(|| machine::next(def, state))
}

/// `Drift` when the live tree is not the tip's.
fn drift(state: &RunState, obs: &Observation) -> Option<Next> {
    let tip = machine::tree_of(state, state.tip);
    (tip != Some(obs.live_tree.as_str()))
        .then(|| drift_pause(obs, tip, &format!("candidate {}", state.tip.0)))
}

/// A `Drift` pause: the live tree is not `expected`, the tree of `what`.
fn drift_pause(obs: &Observation, expected: Option<&str>, what: &str) -> Next {
    Next::Pause(
        PauseReason::Drift,
        drift_detail(expected.unwrap_or("(unknown)"), &obs.live_tree, what),
    )
}

/// The start of a drift detail's last line, before the command that shows
/// the whole change.
const WHOLE_CHANGE: &str = "the whole change: git diff ";

/// A `Drift` pause's detail: both trees, and on its last line the command
/// that shows the whole change. The runner adds the diffstat before that
/// line; [`drift_trees`] reads the trees back.
#[must_use]
pub fn drift_detail(expected: &str, live: &str, what: &str) -> String {
    format!(
        "the live tree {live} is not the tree {expected} of {what}\n{WHOLE_CHANGE}{expected} {live}"
    )
}

/// The recorded and the live tree a [`drift_detail`] names.
#[must_use]
pub fn drift_trees(detail: &str) -> Option<(&str, &str)> {
    let command = detail.lines().last()?.strip_prefix(WHOLE_CHANGE)?;
    let (expected, live) = command.split_once(' ')?;
    let hex = |tree: &str| !tree.is_empty() && tree.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(expected) && hex(live)).then_some((expected, live))
}

fn reconcile_pause(def: &Resolved, attempt: AttemptId, info: &AttemptInfo) -> Next {
    Next::Pause(
        PauseReason::Reconcile,
        format!(
            "attempt {} of {} left the tree changed",
            attempt.0, def.steps[info.step].id
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Accepted, ArtifactId, ArtifactMeta, CandidateId, Origin, Record};
    use crate::sim::{self, Outcome, Script, SimLog};
    use crate::state::replay;

    /// Research rejected once, the tester adds `tests/new_test.rs`,
    /// everything else approves.
    fn milestone() -> Script {
        Script::default()
            .with("review_research", 1, Outcome::Reject { resolve: true })
            .with(
                "test",
                1,
                Outcome::Tests {
                    adds: vec!["tests/new_test.rs".into()],
                    failing: vec![],
                },
            )
    }

    /// A recovery case: a log and the artifacts it may name.
    #[derive(Clone)]
    struct Case {
        def: Resolved,
        records: Vec<Record>,
        artifacts: Vec<ArtifactMeta>,
    }

    impl Case {
        fn state(&self) -> RunState {
            replay(&self.def, &self.records, &self.artifacts).unwrap()
        }

        fn recover(&self, obs: &Observation) -> Next {
            recover(&self.def, &self.state(), obs)
        }

        fn push(&mut self, record: Record) -> &mut Self {
            self.records.push(record);
            self
        }

        fn pause(&mut self, reason: PauseReason) -> &mut Self {
            self.push(Record::Paused {
                reason,
                detail: String::new(),
            })
        }

        fn decide(&mut self, decision: Decision) -> &mut Self {
            self.push(Record::Decision { decision })
        }

        /// Proposes a candidate with `tree`, derived from the tip.
        fn propose(&mut self, tree: &str, origin: Origin) -> CandidateId {
            let id = CandidateId(
                1 + self
                    .records
                    .iter()
                    .filter_map(|r| match r {
                        Record::Candidate { id, .. } => Some(id.0),
                        _ => None,
                    })
                    .max()
                    .unwrap(),
            );
            let parent = Some(self.state().tip);
            self.push(Record::Candidate {
                id,
                tree: tree.into(),
                parent,
                origin,
            });
            id
        }

        /// The latest attempt.
        fn attempt(&self) -> AttemptId {
            *self.state().attempts.keys().next_back().unwrap()
        }
    }

    /// The milestone trace: its log and, for each prefix, what the sim did.
    fn trace() -> (Resolved, SimLog) {
        let def = sim::shipped();
        let log = sim::run(&def, &milestone());
        (def, log)
    }

    /// The milestone trace's records through the first record after `step`'s
    /// attempt in `round` starts that `matches`, with the dispatch that
    /// started the attempt.
    fn through(step: &str, round: u32, matches: impl Fn(&Record) -> bool) -> (Case, Dispatch) {
        let (def, log) = trace();
        let start = log
            .records
            .iter()
            .position(|r| {
                matches!(r, Record::AttemptStarted { step: s, round: n, .. } if s == step && *n == round)
            })
            .unwrap();
        let end = start + log.records[start..].iter().position(matches).unwrap();
        let Next::Run(dispatch) = log.decisions[start].clone() else {
            panic!("{:?}", log.decisions[start]);
        };
        let case = Case {
            def,
            records: log.records[..=end].to_vec(),
            artifacts: log.artifacts,
        };
        (case, dispatch)
    }

    fn started(r: &Record) -> bool {
        matches!(r, Record::AttemptStarted { .. })
    }

    fn session(r: &Record) -> bool {
        matches!(r, Record::AttemptSession { .. })
    }

    fn vendor(r: &Record) -> bool {
        matches!(r, Record::VendorSession { .. })
    }

    fn obs(tree: &str, go_created: bool) -> Observation {
        Observation {
            live_tree: tree.into(),
            go_created,
            processes: ProcessCheck::Clear,
            commands: BTreeMap::new(),
        }
    }

    fn assert_pause(next: &Next, reason: PauseReason) {
        assert!(
            matches!(next, Next::Pause(r, _) if *r == reason),
            "expected Pause({reason:?}), got {next:?}"
        );
    }

    /// The vendor session the sim logs for `attempt` of `step`.
    fn session_of(case: &Case, step: &str) -> String {
        format!("{step}-session-{}", case.attempt().0)
    }

    #[test]
    fn a_drift_detail_names_both_trees_and_the_command() {
        let detail = drift_detail("abc123", "def456", "candidate 3");
        assert_eq!(
            detail,
            "the live tree def456 is not the tree abc123 of candidate 3\n\
             the whole change: git diff abc123 def456"
        );
        assert_eq!(drift_trees(&detail), Some(("abc123", "def456")));
        assert_eq!(drift_trees("the live tree changed"), None);
        assert_eq!(drift_trees("x\nthe whole change: git diff a; b"), None);
    }

    #[test]
    fn read_only_unfinished_is_a_new_attempt() {
        for matches in [started, session, vendor] {
            let (case, dispatch) = through("review_research", 2, matches);
            assert_eq!(dispatch.round, 2);
            for go in [false, true] {
                assert_eq!(case.recover(&obs("t0", go)), Next::Run(dispatch.clone()));
            }
            // The clone is discarded; a change to the live tree is drift.
            assert_pause(&case.recover(&obs("t-edit", true)), PauseReason::Drift);
        }
    }

    #[test]
    fn never_released_and_unchanged_is_a_new_attempt() {
        let (case, dispatch) = through("implement", 1, started);
        assert_eq!(case.recover(&obs("t0", false)), Next::Run(dispatch.clone()));
        // An attempt_session without `go`: the vendor never ran either.
        let (case, _) = through("implement", 1, session);
        assert_eq!(case.recover(&obs("t0", false)), Next::Run(dispatch.clone()));
        // A tester too.
        let (case, dispatch) = through("test", 1, started);
        assert_eq!(case.recover(&obs("t1", false)), Next::Run(dispatch));
    }

    #[test]
    fn never_released_and_changed_is_drift() {
        let (case, _) = through("implement", 1, started);
        assert_pause(&case.recover(&obs("t-edit", false)), PauseReason::Drift);
        let (case, _) = through("implement", 1, session);
        assert_pause(&case.recover(&obs("t-edit", false)), PauseReason::Drift);
        let (case, _) = through("test", 1, started);
        assert_pause(&case.recover(&obs("t-edit", false)), PauseReason::Drift);
    }

    #[test]
    fn released_change_unchanged_continues_the_session() {
        // Released, no vendor session logged: the dispatch as it was.
        let (case, dispatch) = through("implement", 1, session);
        assert_eq!(dispatch.resume_session, None);
        assert_eq!(case.recover(&obs("t0", true)), Next::Run(dispatch.clone()));
        // A logged session is continued, in the same step and round.
        let (case, _) = through("implement", 1, vendor);
        let expected = Dispatch {
            resume_session: Some(session_of(&case, "implement")),
            ..dispatch.clone()
        };
        assert_eq!(case.recover(&obs("t0", true)), Next::Run(expected.clone()));
        // Even when the step starts fresh sessions.
        let mut case = case;
        let implement = case
            .def
            .steps
            .iter()
            .position(|s| s.id == "implement")
            .unwrap();
        case.def.steps[implement].session = crate::definition::SessionPolicy::Fresh;
        assert_eq!(case.recover(&obs("t0", true)), Next::Run(expected));
    }

    #[test]
    fn released_change_changed_reconciles() {
        let (case, _) = through("implement", 1, vendor);
        assert_pause(&case.recover(&obs("t-edit", true)), PauseReason::Reconcile);
        // The change's candidate proposed, not yet committed: still unfinished.
        let (case, _) = through("implement", 1, |r| matches!(r, Record::Candidate { .. }));
        assert_pause(&case.recover(&obs("t1", true)), PauseReason::Reconcile);
        // A transient failure that left the tree changed reconciles too; with
        // the tree unchanged it is retried.
        let mut failed = through("implement", 1, vendor).0;
        let attempt = failed.attempt();
        failed.push(Record::AttemptFailed {
            attempt,
            failure: Failure::Transient {
                detail: "overloaded".into(),
                eligible_at: None,
            },
        });
        assert_pause(
            &failed.recover(&obs("t-edit", true)),
            PauseReason::Reconcile,
        );
        let Next::Run(retry) = failed.recover(&obs("t0", true)) else {
            panic!("{:?}", failed.recover(&obs("t0", true)));
        };
        assert_eq!(failed.def.steps[retry.step].id, "implement");
        // Local and unknown failures, and interrupted ones, too.
        for failure in [
            Failure::LocalFailure {
                detail: "disk full".into(),
            },
            Failure::Unknown { detail: "?".into() },
            Failure::Interrupted {
                signal: "SIGTERM".into(),
            },
        ] {
            let mut failed = through("implement", 1, vendor).0;
            failed.push(Record::AttemptFailed { attempt, failure });
            assert_pause(
                &failed.recover(&obs("t-edit", true)),
                PauseReason::Reconcile,
            );
        }
        // A read-only attempt never reconciles: the change is drift.
        let mut failed = through("review_research", 1, vendor).0;
        let attempt = failed.attempt();
        failed.push(Record::AttemptFailed {
            attempt,
            failure: Failure::Transient {
                detail: "overloaded".into(),
                eligible_at: None,
            },
        });
        assert_pause(&failed.recover(&obs("t-edit", true)), PauseReason::Drift);
    }

    #[test]
    fn tester_before_handoff_unchanged_reruns_the_tester() {
        // Released and before the handoff, the tester's edits proposed or
        // not: a new tester attempt, same step and round.
        for matches in [session, vendor, |r: &Record| {
            matches!(r, Record::Candidate { .. })
        }] {
            let (case, dispatch) = through("test", 1, matches);
            let Next::Run(again) = case.recover(&obs("t1", true)) else {
                panic!("{:?}", case.recover(&obs("t1", true)));
            };
            assert_eq!(again.step, dispatch.step);
            assert_eq!(again.round, dispatch.round);
            assert_eq!(again.input_candidate, CandidateId(1));
            assert_eq!(again.requires, dispatch.requires);
        }
    }

    #[test]
    fn tester_before_handoff_changed_reconciles() {
        for matches in [vendor, |r: &Record| matches!(r, Record::Candidate { .. })] {
            let (case, _) = through("test", 1, matches);
            assert_pause(&case.recover(&obs("t2", true)), PauseReason::Reconcile);
        }
    }

    #[test]
    fn after_handoff_runs_evidence_from_zero() {
        let k2 = CandidateId(2);
        for matches in [
            |r: &Record| matches!(r, Record::EvidenceStarted { .. }),
            |r: &Record| matches!(r, Record::EvidenceCommand { .. }),
            |r: &Record| matches!(r, Record::EvidenceResult { .. }),
        ] {
            let (case, _) = through("test", 1, matches);
            let attempt = case.attempt();
            assert_eq!(
                case.recover(&obs("t2", true)),
                Next::Evidence {
                    attempt,
                    candidate: k2,
                    restart: 0,
                }
            );
        }
        // After a restart: its candidate, the same restart count.
        let (mut case, _) = through("test", 1, |r| matches!(r, Record::EvidenceResult { .. }));
        let attempt = case.attempt();
        let k3 = case.propose("t3", Origin::Evidence);
        case.push(Record::EvidenceStarted {
            attempt,
            candidate: k3,
            restart: 1,
        });
        assert_eq!(
            case.recover(&obs("t3", true)),
            Next::Evidence {
                attempt,
                candidate: k3,
                restart: 1,
            }
        );
    }

    #[test]
    fn after_handoff_with_another_tree_is_drift() {
        let (case, _) = through("test", 1, |r| matches!(r, Record::EvidenceCommand { .. }));
        // The tree before the tester's edits, or any other.
        for tree in ["t1", "t-edit"] {
            assert_pause(&case.recover(&obs(tree, true)), PauseReason::Drift);
        }
    }

    #[test]
    fn a_tests_attempt_failing_after_the_handoff_compares_with_its_candidate() {
        let (mut case, _) = through("test", 1, |r| matches!(r, Record::EvidenceCommand { .. }));
        let attempt = case.attempt();
        case.push(Record::AttemptFailed {
            attempt,
            failure: Failure::LocalFailure {
                detail: "snapshot failed".into(),
            },
        });
        // The tester's edits are committed (K2): nothing to reconcile.
        let state = case.state();
        assert_eq!(
            case.recover(&obs("t2", true)),
            machine::next(&case.def, &state)
        );
        assert_pause(&case.recover(&obs("t2", true)), PauseReason::LocalFailure);
        // Another tree after the handoff is drift, not partial tester work.
        for tree in ["t1", "t-edit"] {
            assert_pause(&case.recover(&obs(tree, true)), PauseReason::Drift);
        }
    }

    /// One case per row of the "State found" table, with its observation.
    fn rows() -> Vec<(Case, Observation)> {
        let evidence = |r: &Record| matches!(r, Record::EvidenceCommand { .. });
        vec![
            (through("review_research", 2, vendor).0, obs("t0", true)),
            (through("implement", 1, started).0, obs("t0", false)),
            (through("implement", 1, started).0, obs("t-edit", false)),
            (through("implement", 1, vendor).0, obs("t0", true)),
            (through("implement", 1, vendor).0, obs("t-edit", true)),
            (through("test", 1, vendor).0, obs("t1", true)),
            (through("test", 1, vendor).0, obs("t2", true)),
            (through("test", 1, evidence).0, obs("t2", true)),
            (through("test", 1, evidence).0, obs("t-edit", true)),
            (
                through("review_change", 1, |r| {
                    matches!(r, Record::AttemptFinished { .. })
                })
                .0,
                obs("t2", true),
            ),
        ]
    }

    #[test]
    fn processes_come_first() {
        for (mut case, clear) in rows() {
            let row = case.recover(&clear);
            let with = |processes| Observation {
                processes,
                ..clear.clone()
            };
            let running = with(ProcessCheck::Verified(vec![42]));
            let unverified = with(ProcessCheck::Unverified(vec![43, 44]));
            assert_pause(&case.recover(&running), PauseReason::ProcessesRunning);
            assert_pause(&case.recover(&unverified), PauseReason::ProcessesUnverified);
            // Verified processes win over unverified ones, and over a decision.
            case.pause(PauseReason::ProcessesRunning)
                .decide(Decision::Wait);
            assert_pause(&case.recover(&running), PauseReason::ProcessesRunning);
            // `kill` or `wait`, then the check runs again: clear, the row.
            assert_eq!(case.recover(&clear), row);
            case.decide(Decision::Kill);
            assert_eq!(case.recover(&clear), row);
            // Released PIDs are ignored from then on; the others still pause.
            case.pause(PauseReason::ProcessesUnverified);
            case.decide(Decision::Release { pids: vec![43] });
            assert_pause(&case.recover(&unverified), PauseReason::ProcessesUnverified);
            case.decide(Decision::Release { pids: vec![44] });
            assert_eq!(case.recover(&unverified), row);
        }
    }

    #[test]
    fn an_unverified_pause_lists_each_process_with_its_command() {
        let (mut case, clear) = rows().remove(0);
        let obs = Observation {
            processes: ProcessCheck::Unverified(vec![43, 44]),
            commands: BTreeMap::from([(43, "sleep 60".to_owned()), (44, "make test".to_owned())]),
            ..clear
        };
        let Next::Pause(PauseReason::ProcessesUnverified, detail) = case.recover(&obs) else {
            panic!("not an unverified pause");
        };
        assert!(detail.contains("[43, 44]"), "{detail}");
        assert!(detail.contains("43: sleep 60"), "{detail}");
        assert!(detail.contains("44: make test"), "{detail}");
        // A released process is not listed.
        case.pause(PauseReason::ProcessesUnverified);
        case.decide(Decision::Release { pids: vec![43] });
        let Next::Pause(_, detail) = case.recover(&obs) else {
            panic!("not a pause");
        };
        assert!(!detail.contains("sleep 60"), "{detail}");
        assert!(detail.contains("44: make test"), "{detail}");
    }

    #[test]
    fn a_running_pause_lists_each_process_with_its_command() {
        let (_, clear) = rows().remove(0);
        let obs = Observation {
            processes: ProcessCheck::Verified(vec![43, 44]),
            commands: BTreeMap::from([(43, "sleep 60".to_owned())]),
            ..clear
        };
        let (case, _) = rows().remove(0);
        let Next::Pause(PauseReason::ProcessesRunning, detail) = case.recover(&obs) else {
            panic!("not a running pause");
        };
        assert!(detail.contains("[43, 44]"), "{detail}");
        assert!(detail.contains("\n  43: sleep 60"), "{detail}");
        assert!(detail.contains("\n  44: (command unknown)"), "{detail}");
    }

    #[test]
    fn an_interrupt_needs_no_decision() {
        for (mut case, obs) in rows() {
            let row = case.recover(&obs);
            let state = case.state();
            let attempt = case.attempt();
            // Between attempts, a clean interrupt logs only the pause.
            if matches!(
                state.attempts[&attempt].state,
                AttemptState::Unfinished { .. }
            ) {
                case.push(Record::AttemptInterrupted {
                    attempt,
                    signal: "SIGINT".into(),
                });
            }
            case.pause(PauseReason::Interrupted);
            assert_eq!(case.recover(&obs), row, "{:?}", case.records.last());
        }
    }

    #[test]
    fn adopting_a_failed_attempt_finishes_it() {
        // A change attempt that failed with the tree changed: Reconcile,
        // adopt, the adopted candidate becomes its change, then the tests
        // run on it.
        let (mut case, dispatch) = through("implement", 1, vendor);
        let attempt = case.attempt();
        case.push(Record::AttemptFailed {
            attempt,
            failure: Failure::Transient {
                detail: "overloaded".into(),
                eligible_at: None,
            },
        });
        let edited = obs("t-edit", true);
        assert_pause(&case.recover(&edited), PauseReason::Reconcile);
        case.pause(PauseReason::Reconcile);
        let k = case.propose("t-edit", Origin::Change);
        case.decide(Decision::Adopt { candidate: k });
        assert_eq!(
            case.recover(&edited),
            Next::FinishAdopted {
                attempt,
                candidate: k,
            }
        );
        let change = ArtifactId(u32::try_from(case.artifacts.len()).unwrap());
        case.artifacts.push(ArtifactMeta {
            id: change,
            step: dispatch.step,
            requires: dispatch.requires.clone(),
            candidate: Some(k),
        });
        case.push(Record::AttemptFinished {
            attempt,
            outcome: Accepted::ChangeProduced { change },
            candidate: Some(k),
        });
        let Next::Run(tests) = case.recover(&edited) else {
            panic!("{:?}", case.recover(&edited));
        };
        assert_eq!(case.def.steps[tests.step].id, "test");
        assert_eq!(tests.input_candidate, k);
        assert_eq!(tests.requires, [change]);

        // A tests attempt that failed with the tree changed: adopt runs the
        // evidence on the adopted candidate, which re-opens the attempt.
        let (mut case, _) = through("test", 1, vendor);
        let attempt = case.attempt();
        case.push(Record::AttemptFailed {
            attempt,
            failure: Failure::LocalFailure {
                detail: "disk full".into(),
            },
        });
        assert_pause(&case.recover(&edited), PauseReason::Reconcile);
        case.pause(PauseReason::Reconcile);
        let k = case.propose("t-edit", Origin::Tests);
        case.decide(Decision::Adopt { candidate: k });
        let evidence = Next::Evidence {
            attempt,
            candidate: k,
            restart: 0,
        };
        assert_eq!(case.recover(&edited), evidence);
        case.push(Record::EvidenceStarted {
            attempt,
            candidate: k,
            restart: 0,
        });
        assert_eq!(case.recover(&edited), evidence);
    }

    #[test]
    fn decisions_have_exact_next_actions() {
        reconcile_decisions();
        round_and_evidence_decisions();
        drift_decisions();
        failure_decisions();
    }

    /// The `Reconcile` rows, on a change step and on a tests step.
    fn reconcile_decisions() {
        // Reconcile (change step): keep continues the same vendor session.
        let (mut case, dispatch) = through("implement", 1, vendor);
        let attempt = case.attempt();
        let session = session_of(&case, "implement");
        case.pause(PauseReason::Reconcile);
        let edited = obs("t-edit", true);
        assert_pause(&case.recover(&edited), PauseReason::Reconcile);
        let mut keep = case.clone();
        keep.decide(Decision::Keep);
        assert_eq!(
            keep.recover(&edited),
            Next::Run(Dispatch {
                resume_session: Some(session.clone()),
                ..dispatch.clone()
            })
        );
        // `stop` keeps the pause until `stopped` is durable.
        let mut stop = case.clone();
        stop.decide(Decision::Stop);
        assert_pause(&stop.recover(&edited), PauseReason::Reconcile);
        stop.push(Record::Stopped);
        assert_eq!(stop.recover(&edited), Next::Done);

        // Reconcile (change step): adopt makes the tree the change's
        // candidate; once finished, next is the tests.
        let k = case.propose("t-edit", Origin::Change);
        case.decide(Decision::Adopt { candidate: k });
        assert_eq!(
            case.recover(&edited),
            Next::FinishAdopted {
                attempt,
                candidate: k,
            }
        );
        let change = ArtifactId(u32::try_from(case.artifacts.len()).unwrap());
        case.artifacts.push(ArtifactMeta {
            id: change,
            step: dispatch.step,
            requires: dispatch.requires.clone(),
            candidate: Some(k),
        });
        case.push(Record::AttemptFinished {
            attempt,
            outcome: Accepted::ChangeProduced { change },
            candidate: Some(k),
        });
        let Next::Run(tests) = case.recover(&edited) else {
            panic!("{:?}", case.recover(&edited));
        };
        assert_eq!(case.def.steps[tests.step].id, "test");
        assert_eq!(tests.input_candidate, k);
        assert_eq!(tests.requires, [change]);

        // Reconcile (tests step): keep continues the tester; adopt runs the
        // evidence on the tester's edits.
        let (mut case, dispatch) = through("test", 1, vendor);
        let attempt = case.attempt();
        let session = session_of(&case, "test");
        case.pause(PauseReason::Reconcile);
        let mut keep = case.clone();
        keep.decide(Decision::Keep);
        assert_eq!(
            keep.recover(&edited),
            Next::Run(Dispatch {
                resume_session: Some(session),
                ..dispatch
            })
        );
        let k = case.propose("t-edit", Origin::Tests);
        case.decide(Decision::Adopt { candidate: k });
        let evidence = Next::Evidence {
            attempt,
            candidate: k,
            restart: 0,
        };
        assert_eq!(case.recover(&edited), evidence);
        case.push(Record::EvidenceStarted {
            attempt,
            candidate: k,
            restart: 0,
        });
        assert_eq!(case.recover(&edited), evidence);
    }

    #[test]
    fn keeping_a_transient_failure_waits_out_its_backoff() {
        let (mut case, dispatch) = through("implement", 1, vendor);
        let attempt = case.attempt();
        let session = session_of(&case, "implement");
        case.push(Record::AttemptFailed {
            attempt,
            failure: Failure::Transient {
                detail: "overloaded".into(),
                eligible_at: None,
            },
        });
        case.push(Record::Retry {
            attempt,
            class: "transient".into(),
            count: 1,
            eligible_at: 4_000,
        });
        let edited = obs("t-edit", true);
        assert_pause(&case.recover(&edited), PauseReason::Reconcile);
        case.pause(PauseReason::Reconcile);
        case.decide(Decision::Keep);
        assert_eq!(
            case.recover(&edited),
            Next::Run(Dispatch {
                resume_session: Some(session),
                not_before: Some(4_000),
                ..dispatch
            })
        );
    }

    #[test]
    fn every_pause_offers_stop_and_its_own_decisions() {
        assert_eq!(options(PauseReason::Reconcile), ["keep", "adopt", "stop"]);
        assert_eq!(options(PauseReason::Interrupted), ["stop"]);
        assert!(allows(
            PauseReason::Drift,
            &Decision::Adopt {
                candidate: CandidateId(1)
            }
        ));
        assert!(!allows(PauseReason::Drift, &Decision::Keep));
        assert!(allows(PauseReason::RoundLimit, &Decision::Rounds { n: 9 }));
        assert!(allows(
            PauseReason::ProcessesUnverified,
            &Decision::Release { pids: vec![] }
        ));
        assert!(!allows(
            PauseReason::ProcessesRunning,
            &Decision::Release { pids: vec![] }
        ));
        assert!(allows(PauseReason::Unknown, &Decision::Stop));
    }

    /// The `RoundLimit`, `Stalled` and `UnstableTests` rows.
    fn round_and_evidence_decisions() {
        // RoundLimit: continue or `rounds N` takes the pending jump.
        let def = sim::shipped();
        let mut script = Script::default();
        for round in 1..=6 {
            script = script.with("review_research", round, Outcome::Reject { resolve: true });
        }
        let log = sim::run(&def, &script);
        assert_pause(log.decisions.last().unwrap(), PauseReason::RoundLimit);
        for decision in [Decision::Continue, Decision::Rounds { n: 7 }] {
            let mut case = Case {
                def: def.clone(),
                records: log.records.clone(),
                artifacts: log.artifacts.clone(),
            };
            case.pause(PauseReason::RoundLimit).decide(decision);
            let Next::Run(revision) = case.recover(&obs("t0", false)) else {
                panic!("{:?}", case.recover(&obs("t0", false)));
            };
            assert_eq!(def.steps[revision.step].id, "research");
            assert_eq!(revision.round, 7);
        }

        // Stalled: continue goes past the stall.
        let script = Script::default()
            .with("review_research", 1, Outcome::Reject { resolve: true })
            .with("review_research", 2, Outcome::Reject { resolve: false });
        let log = sim::run(&def, &script);
        assert_pause(log.decisions.last().unwrap(), PauseReason::Stalled);
        let mut case = Case {
            def: def.clone(),
            records: log.records.clone(),
            artifacts: log.artifacts.clone(),
        };
        case.pause(PauseReason::Stalled).decide(Decision::Continue);
        let Next::Run(revision) = case.recover(&obs("t0", false)) else {
            panic!("{:?}", case.recover(&obs("t0", false)));
        };
        assert_eq!(def.steps[revision.step].id, "research");

        // UnstableTests: retry runs the evidence afresh on the tip, from
        // restart 0.
        let (mut case, _) = through("test", 1, |r| matches!(r, Record::EvidenceResult { .. }));
        let attempt = case.attempt();
        for restart in 1..=2 {
            let k = case.propose(&format!("t-unstable-{restart}"), Origin::Evidence);
            case.push(Record::EvidenceStarted {
                attempt,
                candidate: k,
                restart,
            });
        }
        let tip = case.state().tip;
        case.pause(PauseReason::UnstableTests);
        assert_pause(
            &case.recover(&obs("t-unstable-3", true)),
            PauseReason::UnstableTests,
        );
        case.decide(Decision::Retry);
        assert_eq!(
            case.recover(&obs("t-unstable-3", true)),
            Next::Evidence {
                attempt,
                candidate: tip,
                restart: 0,
            }
        );
    }

    /// The `Drift` row.
    fn drift_decisions() {
        // Drift after the final approval: adopt makes a derived candidate;
        // the tests and the review run again, the coder does not.
        let (mut case, _) = through("review_change", 1, |r| {
            matches!(r, Record::AttemptFinished { .. })
        });
        assert!(matches!(
            case.recover(&obs("t2", true)),
            Next::ReadyToComplete { .. }
        ));
        let drifted = obs("t-drift", true);
        assert_pause(&case.recover(&drifted), PauseReason::Drift);
        case.pause(PauseReason::Drift);
        assert_pause(&case.recover(&drifted), PauseReason::Drift);
        let k = case.propose("t-drift", Origin::Drift);
        case.decide(Decision::Adopt { candidate: k });
        let Next::Run(tests) = case.recover(&drifted) else {
            panic!("{:?}", case.recover(&drifted));
        };
        assert_eq!(case.def.steps[tests.step].id, "test");
        assert_eq!(tests.input_candidate, k);
        // Drift on a write attempt that never ran: adopt, then the attempt
        // again from the adopted candidate.
        let (mut case, dispatch) = through("implement", 1, started);
        let edited = obs("t-edit", false);
        assert_pause(&case.recover(&edited), PauseReason::Drift);
        case.pause(PauseReason::Drift);
        let k = case.propose("t-edit", Origin::Drift);
        case.decide(Decision::Adopt { candidate: k });
        assert_eq!(
            case.recover(&edited),
            Next::Run(Dispatch {
                input_candidate: k,
                ..dispatch
            })
        );
    }

    /// The failure rows and `interrupted`.
    fn failure_decisions() {
        // AccessViolation and InvalidOutput: retry is a new attempt of the
        // same step.
        for (failure, reason) in [
            (
                Failure::AccessViolation {
                    diff_stat: "1 file changed".into(),
                },
                PauseReason::AccessViolation,
            ),
            (
                Failure::Invalid {
                    detail: "no verdict".into(),
                },
                PauseReason::InvalidOutput,
            ),
        ] {
            let (mut case, dispatch) = through("review_research", 1, vendor);
            let attempt = case.attempt();
            case.push(Record::AttemptFailed { attempt, failure });
            assert_pause(&case.recover(&obs("t0", true)), reason);
            case.pause(reason);
            assert_pause(&case.recover(&obs("t0", true)), reason);
            case.decide(Decision::Retry);
            assert_eq!(case.recover(&obs("t0", true)), Next::Run(dispatch));
        }

        // Transient exhausted, LocalFailure, Unknown: retry is a new attempt
        // of the same step; Reconcile first if the tree changed.
        for (failure, reason) in [
            (
                Failure::LocalFailure {
                    detail: "disk full".into(),
                },
                PauseReason::LocalFailure,
            ),
            (
                Failure::Unknown { detail: "?".into() },
                PauseReason::Unknown,
            ),
        ] {
            let (mut case, _) = through("implement", 1, vendor);
            let attempt = case.attempt();
            case.push(Record::AttemptFailed { attempt, failure });
            assert_pause(&case.recover(&obs("t0", true)), reason);
            case.pause(reason).decide(Decision::Retry);
            let Next::Run(retry) = case.recover(&obs("t0", true)) else {
                panic!("{:?}", case.recover(&obs("t0", true)));
            };
            assert_eq!(case.def.steps[retry.step].id, "implement");
            assert_pause(&case.recover(&obs("t-edit", true)), PauseReason::Reconcile);
        }
        let (mut case, _) = through("implement", 1, vendor);
        for _ in 0..=case.def.limits.transient_retries {
            let attempt = case.attempt();
            case.push(Record::AttemptFailed {
                attempt,
                failure: Failure::Transient {
                    detail: "overloaded".into(),
                    eligible_at: None,
                },
            });
            let Next::Run(retry) = case.recover(&obs("t0", true)) else {
                break;
            };
            case.push(Record::AttemptStarted {
                attempt: AttemptId(attempt.0 + 1),
                step: "implement".into(),
                round: retry.round,
                input_candidate: retry.input_candidate,
                requires: retry.requires,
                history: retry.history,
            });
        }
        assert_pause(
            &case.recover(&obs("t0", true)),
            PauseReason::TransientExhausted,
        );
        case.pause(PauseReason::TransientExhausted)
            .decide(Decision::Retry);
        let Next::Run(retry) = case.recover(&obs("t0", true)) else {
            panic!("{:?}", case.recover(&obs("t0", true)));
        };
        assert_eq!(case.def.steps[retry.step].id, "implement");

        // Interrupted: no decision; the "State found" table decides.
        let (mut case, dispatch) = through("review_research", 2, vendor);
        let attempt = case.attempt();
        case.push(Record::AttemptInterrupted {
            attempt,
            signal: "SIGTERM".into(),
        })
        .pause(PauseReason::Interrupted);
        assert_eq!(case.recover(&obs("t0", true)), Next::Run(dispatch));
    }

    #[test]
    fn milestone_one_trace() {
        let (def, log) = trace();
        let started = |step: &str| {
            log.records
                .iter()
                .filter(|r| matches!(r, Record::AttemptStarted { step: s, .. } if s == step))
                .count()
        };
        assert_eq!(started("research"), 2);
        assert_eq!(started("review_research"), 2);
        assert_eq!(started("implement"), 1, "implement is dispatched once");

        // The final approval names the passing evidence and K2, the tester's
        // candidate.
        let approval = log
            .records
            .iter()
            .rev()
            .find_map(|r| match r {
                Record::AttemptFinished {
                    outcome: Accepted::ChangeApproved { approval, .. },
                    ..
                } => Some(*approval),
                _ => None,
            })
            .unwrap();
        let passed = log
            .records
            .iter()
            .rev()
            .find_map(|r| match r {
                Record::AttemptFinished {
                    outcome: Accepted::TestsPassed { evidence },
                    ..
                } => Some(*evidence),
                _ => None,
            })
            .unwrap();
        let k2 = CandidateId(2);
        let meta = |id: ArtifactId| log.artifacts.iter().find(|m| m.id == id).unwrap();
        assert!(meta(approval).requires.contains(&passed));
        assert_eq!(meta(approval).candidate, Some(k2));
        assert_eq!(meta(passed).candidate, Some(k2));
        assert!(log.records.iter().any(|r| matches!(
            r,
            Record::Candidate { id, origin: Origin::Tests, .. } if *id == k2
        )));
        assert!(matches!(
            log.records.last(),
            Some(Record::Completed { candidate, evidence, approval: a, .. })
                if *candidate == k2 && *evidence == passed && *a == approval
        ));
        assert_eq!(log.decisions.last(), Some(&Next::Done));

        // Every durable prefix: recovery does what the uninterrupted run did.
        for i in 1..=log.records.len() {
            let state = replay(&def, &log.records[..i], &log.artifacts).unwrap();
            assert_eq!(
                recover(&def, &state, &log.observations[i]),
                log.decisions[i],
                "after record {i}: {:?}",
                log.records[i - 1]
            );
        }
    }
}
