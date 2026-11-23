//! A tests attempt's evidence phase: [`crate::evidence::run`] pass after
//! pass until a pass completes on an unchanged tree, the tree will not hold
//! still, or a signal comes.

use std::fmt::Write as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{Driver, Flow, RunnerError, View};
use crate::contract::{CommandSummary, EvidenceSummary};
use crate::evidence::{self, CommandResult, EvidenceError, EvidenceOutcome, EvidenceRun};
use crate::machine;
use crate::record::{Accepted, AttemptId, CandidateId, Failure, Origin, PauseReason, Record};
use crate::state::AttemptState;
use crate::validate::Refs;

/// An evidence artifact's body (`artifacts/<id>.evidence`): every command
/// of the pass that completed on the candidate's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct EvidenceBody {
    /// The candidate the commands ran on.
    pub(super) candidate: CandidateId,
    /// Its tree: every observation's.
    pub(super) tree: String,
    /// One entry per command, in order.
    pub(super) results: Vec<CommandBody>,
}

/// One command of an [`EvidenceBody`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CommandBody {
    command: String,
    exit: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    pre_tree: String,
    post_tree: String,
    tail: String,
}

impl From<&CommandResult> for CommandBody {
    fn from(result: &CommandResult) -> Self {
        Self {
            command: result.command.clone(),
            exit: result.exit,
            timed_out: result.timed_out,
            duration_ms: result.duration_ms,
            pre_tree: result.pre_tree.clone(),
            post_tree: result.post_tree.clone(),
            tail: result.tail.clone(),
        }
    }
}

impl CommandBody {
    fn passed(&self) -> bool {
        self.exit == Some(0) && !self.timed_out
    }
}

impl EvidenceBody {
    /// The evidence as a change reviewer reads it.
    pub(super) fn summary(&self) -> EvidenceSummary {
        EvidenceSummary {
            candidate: self.candidate,
            tree: self.tree.clone(),
            results: self
                .results
                .iter()
                .map(|result| CommandSummary {
                    command: result.command.clone(),
                    exit: result.exit,
                    timed_out: result.timed_out,
                    pre_tree: result.pre_tree.clone(),
                    post_tree: result.post_tree.clone(),
                    tail: result.tail.clone(),
                })
                .collect(),
        }
    }

    /// The failing commands and their output, for the change step that
    /// revises after them.
    pub(super) fn failing_text(&self) -> String {
        let mut text = format!(
            "The tests failed on candidate {} (tree {}):",
            self.candidate.0, self.tree
        );
        for result in self.results.iter().filter(|result| !result.passed()) {
            let ended = if result.timed_out {
                "timed out".to_owned()
            } else {
                result
                    .exit
                    .map_or_else(|| "was killed".to_owned(), |code| format!("exited {code}"))
            };
            let _ = write!(
                text,
                "\n\n`{}` {ended}. The end of its output:\n````\n{}\n````",
                result.command,
                result.tail.trim_end()
            );
        }
        text
    }
}

impl Driver {
    /// A pass that completed: its evidence artifact, then the attempt's
    /// outcome, passed when every command exited 0 in time.
    async fn record_evidence(
        &mut self,
        attempt: AttemptId,
        step: usize,
        requires: Vec<crate::record::ArtifactId>,
        body: EvidenceBody,
    ) -> Result<Flow, RunnerError> {
        let failing: Vec<usize> = body
            .results
            .iter()
            .enumerate()
            .filter(|(_, result)| !result.passed())
            .map(|(index, _)| index)
            .collect();
        let json = serde_json::to_vec_pretty(&body)
            .map_err(|error| RunnerError::Store(crate::store::StoreError::Json(error)))?;
        let evidence = self
            .write_meta(step, requires, Some(body.candidate), &json, "evidence")
            .await?;
        let outcome = if failing.is_empty() {
            Accepted::TestsPassed { evidence }
        } else {
            Accepted::TestsFailed { evidence, failing }
        };
        self.finish(attempt, outcome, None).await
    }

    /// Runs the evidence of tests attempt `attempt` on `candidate`, from
    /// command zero, at `restart`. A phase the log has not begun on that
    /// candidate and restart (an adopted tester tree, a retry after
    /// unstable tests) is begun first with `evidence_started`.
    pub(super) async fn evidence(
        &mut self,
        view: &View,
        attempt: AttemptId,
        candidate: CandidateId,
        restart: u32,
    ) -> Result<Flow, RunnerError> {
        let info = &view.state.attempts[&attempt];
        let begun = matches!(
            info.state,
            AttemptState::Unfinished { evidence: Some(phase), .. } if phase == (candidate, restart)
        );
        let (step, requires) = (info.step, info.requires.clone());
        let Refs::Tests {
            commands,
            timeout_secs,
            ..
        } = self.def.steps[step].refs.clone()
        else {
            let detail = format!("attempt {} is not a tests attempt", attempt.0);
            return self.fail(attempt, Failure::LocalFailure { detail }).await;
        };
        if !begun {
            self.append(&Record::EvidenceStarted {
                attempt,
                candidate,
                restart,
            })
            .await?;
        }
        // The runner makes `<attempt>/evidence`, and the directories above it.
        let attempt_dir = self.attempt_dir(attempt);
        let (mut candidate, mut restart) = (candidate, restart);
        let Some(mut tree) = machine::tree_of(&view.state, candidate).map(str::to_owned) else {
            let detail = format!("candidate {} has no tree", candidate.0);
            return self.fail(attempt, Failure::LocalFailure { detail }).await;
        };
        loop {
            let pass = evidence::run(EvidenceRun {
                repo: &self.repo,
                log: &mut self.log,
                attempt,
                candidate,
                tree: tree.clone(),
                restart,
                commands: &commands,
                timeout: Duration::from_secs(timeout_secs),
                marker: format!("{}/{}", self.id, attempt.0),
                pause_hook: evidence::pause_hook_from(std::env::var_os(evidence::TEST_PAUSE_ENV)),
                attempt_dir: attempt_dir.clone(),
                interrupt: Some(self.io.signals.clone()),
                events: Some(self.io.events.clone()),
            })
            .await;
            self.own(attempt).await?;
            let outcome = match pass {
                Ok(outcome) => outcome,
                Err(EvidenceError::Store(error)) => return Err(error.into()),
                Err(error) => {
                    let detail = format!("the evidence runner failed: {error}");
                    return self.fail(attempt, Failure::LocalFailure { detail }).await;
                }
            };
            match outcome {
                EvidenceOutcome::Complete { results, .. } => {
                    let body = EvidenceBody {
                        candidate,
                        tree,
                        results: results.iter().map(CommandBody::from).collect(),
                    };
                    let flow = self.record_evidence(attempt, step, requires, body).await?;
                    self.settle(attempt).await?;
                    return Ok(flow);
                }
                EvidenceOutcome::Changed {
                    tree: observed,
                    restart: next,
                } => {
                    let view = self.load().await?;
                    let derived = self
                        .propose(&view, &observed, candidate, Origin::Evidence)
                        .await?;
                    self.append(&Record::EvidenceStarted {
                        attempt,
                        candidate: derived,
                        restart: next,
                    })
                    .await?;
                    (candidate, tree, restart) = (derived, observed, next);
                }
                EvidenceOutcome::Unstable => {
                    let detail = format!(
                        "the tests changed the tree on every one of {} passes",
                        restart + 1
                    );
                    return self
                        .pause(PauseReason::UnstableTests, detail)
                        .await
                        .map(Some);
                }
                EvidenceOutcome::Interrupted(signal) => {
                    return self.interrupted(Some(attempt), signal).await.map(Some);
                }
            }
        }
    }
}
