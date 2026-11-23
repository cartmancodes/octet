//! A scripted, in-memory run: the records the runner writes, without
//! sessions, snapshots or processes.
//!
//! [`run`] drives the machine from the base candidate to the end, writing
//! for every attempt the records the runner (Task 14) will write, in its
//! order, and keeping, for every prefix of the log, what a crash right after
//! it would observe and what the sim did next. Trees are invented strings
//! (`t0` for the base, `t<n>` for candidate `n`).
//!
//! A scripted vendor's edits land in the live tree with the record that
//! commits them (`attempt_finished` for a change, `evidence_started` for the
//! tester's edits), so a crash anywhere else finds the tree its attempt
//! started from. A crash between an edit and its commit is one of
//! [`crate::recover`]'s tree-changed rows, tested on its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::definition::{Kind, SessionPolicy, parse};
use crate::machine::{self, Dispatch, Next};
use crate::record::{
    Accepted, ArtifactId, ArtifactMeta, AttemptId, CandidateId, Finding, Identity, Origin, Record,
    Severity, Status,
};
use crate::recover::{Observation, ProcessCheck};
use crate::state::{RunState, replay};
use crate::validate::{Host, Refs, Resolved, validate};

/// The workflow the repository ships.
const SHIPPED: &str = include_str!("../../../docs/fleet/research-plan-build.yaml");

/// A host that knows every engine and effort.
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

/// The shipped workflow, resolved.
pub(crate) fn shipped() -> Resolved {
    validate(
        &parse(SHIPPED).expect("the shipped workflow parses"),
        &FakeHost,
    )
    .expect("the shipped workflow validates")
}

/// What an attempt does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A document or change step produces; a verdict approves, resolving
    /// its step's open findings; a tests step passes without editing.
    Accept,
    /// A verdict requests changes with one new blocking finding, resolving
    /// its step's earlier open findings (`resolve`) or none of them; a tests
    /// step fails command 0 without editing.
    Reject {
        /// Whether the earlier open findings are resolved.
        resolve: bool,
    },
    /// The tester adds `adds` (none: no edit), then the `failing` commands
    /// fail (none: the tests pass).
    Tests {
        /// The files the tester adds.
        adds: Vec<String>,
        /// The failing commands' indices.
        failing: Vec<usize>,
    },
}

/// Outcomes by `(step id, round)`; a pair not listed is [`Outcome::Accept`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Script(pub(crate) BTreeMap<(String, u32), Outcome>);

impl Script {
    /// Scripts `outcome` for `step`'s attempt in `round`.
    pub(crate) fn with(mut self, step: &str, round: u32, outcome: Outcome) -> Self {
        self.0.insert((step.to_owned(), round), outcome);
        self
    }

    fn outcome(&self, step: &str, round: u32) -> Outcome {
        self.0
            .get(&(step.to_owned(), round))
            .cloned()
            .unwrap_or(Outcome::Accept)
    }
}

/// A finished simulation.
///
/// `observations[i]` and `decisions[i]` belong to the prefix
/// `records[..i]`: what a crash right after those records would observe,
/// and the action the sim took next. Index 0, the empty log (which replay
/// refuses), repeats index 1.
#[derive(Debug, Clone)]
pub(crate) struct SimLog {
    /// The log.
    pub(crate) records: Vec<Record>,
    /// Every artifact's metadata.
    pub(crate) artifacts: Vec<ArtifactMeta>,
    /// What a crash after each prefix would observe.
    pub(crate) observations: Vec<Observation>,
    /// What the sim did after each prefix.
    pub(crate) decisions: Vec<Next>,
}

/// Runs `def` to its end under `script`: until the machine says the run is
/// done, or pauses, or fails (the last decision says which).
///
/// At every point between attempts the sim does what [`machine::next`] says.
/// Inside an attempt, what it does next is that attempt, continued: the
/// dispatch that started it, in the vendor session it is running in once
/// that session is logged and the step carries its work in its session (a
/// change step, or a step that continues sessions); and, once the evidence
/// phase began, the evidence on its candidate.
///
/// # Panics
///
/// If the log it writes does not replay, or the machine asks it to resume an
/// evidence phase it did not start (it never leaves one unfinished).
pub(crate) fn run(def: &Resolved, script: &Script) -> SimLog {
    let mut sim = Sim {
        def,
        log: SimLog {
            records: Vec::new(),
            artifacts: Vec::new(),
            observations: Vec::new(),
            decisions: Vec::new(),
        },
        tree: "t0".into(),
        go_created: false,
        next_candidate: 1,
        next_attempt: 0,
    };
    sim.push(Record::Candidate {
        id: CandidateId(0),
        tree: "t0".into(),
        parent: None,
        origin: Origin::Base,
    });
    loop {
        let next = machine::next(def, &sim.state());
        sim.decide(next.clone());
        match next {
            Next::Run(dispatch) => sim.attempt(dispatch, script),
            Next::ReadyToComplete {
                candidate,
                tree,
                evidence,
                approval,
            } => sim.push(Record::Completed {
                candidate,
                observed_tree: sim.tree.clone(),
                tree,
                evidence,
                approval,
            }),
            Next::Evidence { .. } | Next::FinishAdopted { .. } => {
                panic!("the sim never leaves an attempt unfinished: {next:?}")
            }
            Next::Pause(..) | Next::Fail(_) | Next::Done => break,
        }
    }
    let mut log = sim.log;
    log.observations.insert(0, log.observations[0].clone());
    log.decisions.insert(0, log.decisions[0].clone());
    assert_eq!(log.observations.len(), log.records.len() + 1);
    assert_eq!(log.decisions.len(), log.records.len() + 1);
    log
}

struct Sim<'a> {
    def: &'a Resolved,
    log: SimLog,
    /// The live tree.
    tree: String,
    /// The latest attempt's `go` file exists.
    go_created: bool,
    next_candidate: u32,
    next_attempt: u32,
}

impl Sim<'_> {
    fn state(&self) -> RunState {
        replay(self.def, &self.log.records, &self.log.artifacts).expect("the sim's log replays")
    }

    /// Appends `record` and what a crash right after it would observe.
    fn push(&mut self, record: Record) {
        self.log.records.push(record);
        self.log.observations.push(Observation {
            live_tree: self.tree.clone(),
            go_created: self.go_created,
            processes: ProcessCheck::Clear,
            commands: BTreeMap::new(),
        });
    }

    /// What the sim does after the records so far.
    fn decide(&mut self, next: Next) {
        self.log.decisions.push(next);
        assert_eq!(self.log.decisions.len(), self.log.records.len());
    }

    fn artifact(&mut self, dispatch: &Dispatch, candidate: Option<CandidateId>) -> ArtifactId {
        let id = ArtifactId(u32::try_from(self.log.artifacts.len()).expect("few artifacts"));
        self.log.artifacts.push(ArtifactMeta {
            id,
            step: dispatch.step,
            requires: dispatch.requires.clone(),
            candidate,
        });
        id
    }

    /// Proposes a candidate derived from `parent`, with a new tree.
    fn propose(&mut self, parent: CandidateId, origin: Origin) -> (CandidateId, String) {
        let id = CandidateId(self.next_candidate);
        self.next_candidate += 1;
        let tree = format!("t{}", id.0);
        self.push(Record::Candidate {
            id,
            tree: tree.clone(),
            parent: Some(parent),
            origin,
        });
        (id, tree)
    }

    /// One attempt, from `attempt_started` to `attempt_finished`.
    fn attempt(&mut self, dispatch: Dispatch, script: &Script) {
        let def = self.def;
        let step = &def.steps[dispatch.step];
        let attempt = AttemptId(self.next_attempt);
        self.next_attempt += 1;
        self.go_created = false;
        self.push(Record::AttemptStarted {
            attempt,
            step: step.id.clone(),
            round: dispatch.round,
            input_candidate: dispatch.input_candidate,
            requires: dispatch.requires.clone(),
            history: dispatch.history.clone(),
        });
        self.decide(Next::Run(dispatch.clone()));
        self.push(Record::AttemptSession {
            attempt,
            journal: PathBuf::from(format!("attempts/{}/journal.jsonl", attempt.0)),
            identities: vec![Identity {
                pid: 1000 + attempt.0,
                start: "start".into(),
            }],
        });
        // The gate is released right after the identities are durable.
        self.go_created = true;
        self.decide(Next::Run(dispatch.clone()));
        let session = format!("{}-session-{}", step.id, attempt.0);
        self.push(Record::VendorSession {
            attempt,
            id: session.clone(),
        });
        let carries_work = step.kind == Kind::Change || step.session == SessionPolicy::Continue;
        let dispatch = Dispatch {
            resume_session: if carries_work {
                Some(session)
            } else {
                dispatch.resume_session
            },
            ..dispatch
        };
        self.decide(Next::Run(dispatch.clone()));
        let outcome = script.outcome(&step.id, dispatch.round);
        match &step.refs {
            Refs::Document { .. } => {
                let document = self.artifact(&dispatch, None);
                self.finish(attempt, Accepted::DocumentProduced { document }, None);
            }
            Refs::ReviewDocument { .. } | Refs::ReviewChange { .. } => {
                self.verdict(attempt, &dispatch, &outcome);
            }
            Refs::Change { .. } => {
                let (candidate, tree) = self.propose(dispatch.input_candidate, Origin::Change);
                self.decide(Next::Run(dispatch.clone()));
                let change = self.artifact(&dispatch, Some(candidate));
                self.tree = tree;
                self.finish(
                    attempt,
                    Accepted::ChangeProduced { change },
                    Some(candidate),
                );
            }
            Refs::Tests { commands, .. } => {
                self.tests(attempt, &dispatch, commands, outcome);
            }
        }
    }

    fn finish(&mut self, attempt: AttemptId, outcome: Accepted, candidate: Option<CandidateId>) {
        self.push(Record::AttemptFinished {
            attempt,
            outcome,
            candidate,
        });
    }

    /// A verdict: approval or changes requested, with finding IDs from
    /// [`RunState::next_finding_ids`].
    fn verdict(&mut self, attempt: AttemptId, dispatch: &Dispatch, outcome: &Outcome) {
        let step = &self.def.steps[dispatch.step];
        let state = self.state();
        let open: Vec<String> = state
            .findings
            .iter()
            .filter(|(id, (_, status))| {
                id.starts_with(&format!("{}/", step.id)) && *status == Status::Open
            })
            .map(|(id, _)| id.clone())
            .collect();
        let previous = |status: Status| -> Vec<(String, Status)> {
            open.iter().map(|id| (id.clone(), status)).collect()
        };
        let outcome = match outcome {
            Outcome::Reject { resolve } => Accepted::ChangesRequested {
                new_findings: state
                    .next_finding_ids(&step.id, 1)
                    .into_iter()
                    .map(|id| Finding {
                        id,
                        severity: Severity::Blocking,
                        summary: "fix it".into(),
                        file: None,
                    })
                    .collect(),
                previous: previous(if *resolve {
                    Status::Resolved
                } else {
                    Status::Open
                }),
            },
            Outcome::Accept | Outcome::Tests { .. } => {
                if matches!(step.refs, Refs::ReviewChange { .. }) {
                    let approval = self.artifact(dispatch, Some(dispatch.input_candidate));
                    Accepted::ChangeApproved {
                        approval,
                        new_findings: vec![],
                        previous: previous(Status::Resolved),
                    }
                } else {
                    let approval = self.artifact(dispatch, None);
                    Accepted::DocumentApproved {
                        approval,
                        new_findings: vec![],
                        previous: previous(Status::Resolved),
                    }
                }
            }
        };
        self.finish(attempt, outcome, None);
    }

    /// The tester's turn, the handoff, the evidence commands and the result.
    fn tests(
        &mut self,
        attempt: AttemptId,
        dispatch: &Dispatch,
        commands: &[String],
        outcome: Outcome,
    ) {
        let (adds, failing) = match outcome {
            Outcome::Accept => (vec![], vec![]),
            Outcome::Reject { .. } => (vec![], vec![0]),
            Outcome::Tests { adds, failing } => (adds, failing),
        };
        // The tester's edits are a proposal until the handoff commits them.
        let candidate = if adds.is_empty() {
            dispatch.input_candidate
        } else {
            let (candidate, tree) = self.propose(dispatch.input_candidate, Origin::Tests);
            self.decide(Next::Run(dispatch.clone()));
            self.tree = tree;
            candidate
        };
        self.push(Record::EvidenceStarted {
            attempt,
            candidate,
            restart: 0,
        });
        let evidence_phase = Next::Evidence {
            attempt,
            candidate,
            restart: 0,
        };
        self.decide(evidence_phase.clone());
        for (index, command) in commands.iter().enumerate() {
            self.push(Record::EvidenceCommand {
                attempt,
                restart: 0,
                index,
                command: command.clone(),
                identity: Identity {
                    pid: 2000 + attempt.0,
                    start: "start".into(),
                },
                pre_tree: self.tree.clone(),
            });
            self.decide(evidence_phase.clone());
            self.push(Record::EvidenceResult {
                attempt,
                restart: 0,
                index,
                exit: Some(i32::from(failing.contains(&index))),
                timed_out: false,
                duration_ms: 10,
                post_tree: self.tree.clone(),
            });
            self.decide(evidence_phase.clone());
        }
        let evidence = self.artifact(dispatch, Some(candidate));
        let outcome = if failing.is_empty() {
            Accepted::TestsPassed { evidence }
        } else {
            Accepted::TestsFailed { evidence, failing }
        };
        self.finish(attempt, outcome, None);
    }
}
