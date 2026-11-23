//! One attempt: its workspace, its prompt, its turn, and what is made of
//! the reply (an artifact and `attempt_finished`, a failure, or the tester's
//! handoff to the evidence runner).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::evidence::EvidenceBody;
use super::{Driver, Flow, RunEnd, RunnerError, View};
use crate::FleetEvent;
use crate::classify::FailureClass;
use crate::contract::{self, HeaderIds, Patch, PromptInputs};
use crate::definition::Kind;
use crate::machine::{self, Dispatch};
use crate::record::{
    Accepted, ArtifactId, ArtifactMeta, AttemptId, CandidateId, Failure, Finding, Origin,
    PauseReason, Record, Status,
};
use crate::snapshot::{self, Repo};
use crate::state::{AttemptState, RunState};
use crate::step::{self, AttemptPlan, FollowUp, TurnEnd};
use crate::store;
use crate::validate::{Refs, Step};

/// The patch file's name, in the attempt directory or the reviewer's clone.
const PATCH_FILE: &str = "octet-fleet.patch";

/// A read-only attempt's scratch clone.
struct Scratch {
    /// The clone's top level, `<attempt>/tree`.
    top: PathBuf,
    /// The vendor's working directory in it.
    cwd: PathBuf,
    /// The scratch index its snapshots use.
    index: PathBuf,
}

impl Driver {
    /// Starts and carries out one attempt of `dispatch`. A write attempt
    /// claims the work tree first and updates its claim when it ends.
    pub(super) async fn attempt(
        &mut self,
        view: &View,
        dispatch: Dispatch,
        kept: bool,
    ) -> Result<Flow, RunnerError> {
        let def = Arc::clone(&self.def);
        let step = &def.steps[dispatch.step];
        let attempt = view
            .state
            .attempts
            .keys()
            .next_back()
            .map_or(AttemptId(0), |last| AttemptId(last.0 + 1));
        // The attempt before this one, left unfinished or failed, may have
        // left its scratch clone (an accepted one's is already gone): a new
        // attempt discards it (spec, "State found": "discard its clone").
        // Each attempt discards its predecessor's, so none is left behind.
        // One that cannot be removed is a local failure: the run pauses
        // before the new attempt starts.
        if let Some((&previous, info)) = view.state.attempts.iter().next_back()
            && info.state != AttemptState::Finished
        {
            let clone = self.attempt_dir(previous).join("tree");
            match tokio::fs::remove_dir_all(&clone).await {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    let detail = format!("cannot discard the clone {}: {error}", clone.display());
                    return self
                        .pause(PauseReason::LocalFailure, detail)
                        .await
                        .map(Some);
                }
                _ => {}
            }
        }
        if !step.read_only() {
            self.own(attempt).await?;
        }
        self.append(&Record::AttemptStarted {
            attempt,
            step: step.id.clone(),
            round: dispatch.round,
            input_candidate: dispatch.input_candidate,
            requires: dispatch.requires.clone(),
            history: dispatch.history.clone(),
        })
        .await?;
        self.emit(FleetEvent::AttemptStarted {
            attempt,
            step: step.id.clone(),
            round: dispatch.round,
            engine: step.profile.engine.clone(),
            model: step.profile.model.clone(),
        })
        .await;
        let flow = self.turn(view, step, &dispatch, attempt, kept).await;
        if !step.read_only() {
            self.settle(attempt).await?;
        }
        flow
    }

    /// The attempt's turn, from its workspace to its outcome.
    async fn turn(
        &mut self,
        view: &View,
        step: &Step,
        dispatch: &Dispatch,
        attempt: AttemptId,
        kept: bool,
    ) -> Result<Flow, RunnerError> {
        let attempt_dir = self.attempt_dir(attempt);
        if let Err(error) = octet_store::create_private_dir(&attempt_dir).await {
            let detail = format!("cannot create {}: {error}", attempt_dir.display());
            return self.fail(attempt, Failure::LocalFailure { detail }).await;
        }
        let scratch = if step.read_only() {
            match self.scratch(dispatch, &attempt_dir).await {
                Ok(scratch) => Some(scratch),
                Err(detail) => return self.fail(attempt, Failure::LocalFailure { detail }).await,
            }
        } else {
            None
        };
        let cwd = scratch.as_ref().map_or_else(
            || self.repo.top.join(&self.repo.cwd_rel),
            |scratch| scratch.cwd.clone(),
        );
        // A reviewer reads the patch file in its own clone; a write step in
        // its attempt directory.
        let patch_dir = scratch
            .as_ref()
            .map_or_else(|| attempt_dir.clone(), |scratch| scratch.cwd.clone());
        let open = open_findings(&view.state, &step.id);
        let prompt = match self
            .prompt(view, step, dispatch, attempt, kept, &open, &patch_dir)
            .await
        {
            Ok(prompt) => prompt,
            Err(detail) => return self.fail(attempt, Failure::LocalFailure { detail }).await,
        };
        let before = match &scratch {
            Some(scratch) => match snapshot::tree_of(&scratch.cwd, &scratch.index).await {
                Ok(tree) => Some(tree),
                Err(error) => {
                    let detail = format!("cannot observe the scratch clone: {error}");
                    return self.fail(attempt, Failure::LocalFailure { detail }).await;
                }
            },
            None => None,
        };
        let open_refs: Vec<&Finding> = open.iter().collect();
        let verdict_ok = |text: &str| contract::parse_verdict(text, &open_refs).is_ok();
        let document_ok = |text: &str| !text.trim().is_empty();
        let follow_up = match step.kind {
            Kind::Verdict => Some(FollowUp {
                accept: &verdict_ok,
                prompt: contract::CORRECTIVE.to_owned(),
            }),
            Kind::Document => Some(FollowUp {
                accept: &document_ok,
                prompt: contract::DOCUMENT_CORRECTIVE.to_owned(),
            }),
            Kind::Change | Kind::Tests => None,
        };
        let plan = AttemptPlan {
            step,
            dispatch,
            attempt,
            prompt,
            cwd,
            attempt_dir,
            marker: self.marker(attempt),
            data_dir: self.data_dir.clone(),
            interrupt: Some(self.io.signals.clone()),
            follow_up,
            pause_hook: crate::evidence::pause_hook_from(std::env::var_os(
                crate::evidence::TEST_PAUSE_ENV,
            )),
        };
        let end = step::run_attempt(
            plan,
            &mut self.log,
            self.io.approver.as_mut(),
            &self.io.events,
        )
        .await?;
        if let (Some(scratch), Some(before)) = (&scratch, &before)
            && end != TurnEnd::Interrupted
        {
            match access_violation(scratch, before).await {
                Ok(None) => {}
                Ok(Some(diff_stat)) => {
                    return self
                        .fail(attempt, Failure::AccessViolation { diff_stat })
                        .await;
                }
                Err(detail) => return self.fail(attempt, Failure::LocalFailure { detail }).await,
            }
        }
        match end {
            TurnEnd::Interrupted => {
                let signal = self.signal();
                self.interrupted(Some(attempt), signal).await.map(Some)
            }
            TurnEnd::Failed(class, detail) => self.failed(dispatch, attempt, class, detail).await,
            TurnEnd::Replied { text } => {
                self.accept(view, step, dispatch, attempt, &text, &open)
                    .await
            }
        }
    }

    /// A fresh scratch clone of the dispatch's candidate at
    /// `<attempt>/tree`. An error is a local failure, as text.
    async fn scratch(&self, dispatch: &Dispatch, attempt_dir: &Path) -> Result<Scratch, String> {
        let top = attempt_dir.join("tree");
        match tokio::fs::remove_dir_all(&top).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("cannot clear {}: {error}", top.display()));
            }
            _ => {}
        }
        let commit = snapshot::kept(&self.repo, &self.id, dispatch.input_candidate.0)
            .await
            .map_err(|error| {
                format!(
                    "cannot find candidate {}: {error}",
                    dispatch.input_candidate.0
                )
            })?;
        let cwd = snapshot::scratch_clone(&self.repo, &commit, &top)
            .await
            .map_err(|error| format!("cannot make the scratch clone: {error}"))?;
        Ok(Scratch {
            top,
            cwd,
            index: attempt_dir.join("clone-index.tmp"),
        })
    }

    /// The attempt's prompt: the contract's, with the patch inline when it
    /// fits and in a file in `patch_dir` when it does not. An error (a
    /// missing input, a prompt over the limit) is a local failure, as text;
    /// nothing is ever cut.
    #[expect(
        clippy::too_many_arguments,
        reason = "the prompt's inputs, each from a different source"
    )]
    async fn prompt(
        &self,
        view: &View,
        step: &Step,
        dispatch: &Dispatch,
        attempt: AttemptId,
        kept: bool,
        open: &[Finding],
        patch_dir: &Path,
    ) -> Result<String, String> {
        let state = &view.state;
        let tree = machine::tree_of(state, dispatch.input_candidate)
            .ok_or_else(|| format!("candidate {} has no tree", dispatch.input_candidate.0))?;
        let ids = HeaderIds {
            attempt,
            candidate: dispatch.input_candidate,
            tree: tree.to_owned(),
        };
        let mut documents = Vec::new();
        for artifact in machine::documents_for(&self.def, state, dispatch)? {
            let text = self.text_of(artifact).await?;
            let step = state.artifacts[&artifact].step;
            documents.push((self.def.steps[step].id.clone(), text));
        }
        let evidence = match step.refs {
            Refs::ReviewChange { evidence, .. } => {
                let artifact = dispatch
                    .requires
                    .iter()
                    .copied()
                    .find(|id| state.artifacts.get(id).is_some_and(|m| m.step == evidence))
                    .ok_or_else(|| format!("{} has no evidence to read", step.id))?;
                Some(self.evidence_of(artifact).await?.summary())
            }
            _ => None,
        };
        let history = self.history(state, dispatch, kept).await?;
        let inputs = PromptInputs {
            task: &self.file.task,
            documents: documents
                .iter()
                .map(|(id, text)| (id.as_str(), text.as_str()))
                .collect(),
            patch: None,
            evidence: evidence.as_ref(),
            open_findings: open.iter().collect(),
            history,
        };
        let needs_patch = matches!(step.refs, Refs::Tests { .. } | Refs::ReviewChange { .. });
        let prompt = if needs_patch {
            let diff = |stat| snapshot::diff(&self.repo, &self.file.base_tree, tree, stat);
            let full = diff(false)
                .await
                .map_err(|error| format!("cannot make the patch: {error}"))?;
            if contract::fits_inline(step, dispatch, &ids, &inputs, &full) {
                let inputs = PromptInputs {
                    patch: Some(Patch::Inline(&full)),
                    ..inputs
                };
                contract::prompt(step, dispatch, &ids, &inputs)
            } else {
                let stat = diff(true)
                    .await
                    .map_err(|error| format!("cannot make the patch's stat: {error}"))?;
                let path = patch_dir.join(PATCH_FILE);
                tokio::fs::write(&path, &full)
                    .await
                    .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
                let inputs = PromptInputs {
                    patch: Some(Patch::File {
                        stat: &stat,
                        path: &path,
                    }),
                    ..inputs
                };
                contract::prompt(step, dispatch, &ids, &inputs)
            }
        } else {
            contract::prompt(step, dispatch, &ids, &inputs)
        };
        if prompt.len() > octet_core::PROMPT_LIMIT {
            return Err(format!(
                "the prompt for {} is {} bytes, over the {} byte limit; nothing was sent",
                step.id,
                prompt.len(),
                octet_core::PROMPT_LIMIT
            ));
        }
        Ok(prompt)
    }

    /// The attempt's history, rendered: that it continues an interrupted
    /// attempt, the rejection it revises and its open findings, and the
    /// failing evidence.
    async fn history(
        &self,
        state: &RunState,
        dispatch: &Dispatch,
        kept: bool,
    ) -> Result<Vec<String>, String> {
        let mut history = Vec::new();
        if kept {
            history.push(
                "This attempt continues one that was interrupted. Its partial changes are in \
                 place in the work tree; finish the work from there."
                    .to_owned(),
            );
        }
        if state.needs_revision[dispatch.step]
            && let Some(rejector) = state.rejected_by[dispatch.step]
        {
            let rejector = &self.def.steps[rejector];
            let findings = open_findings(state, &rejector.id);
            let mut text = format!("{} sent the previous version back.", rejector.id);
            if !findings.is_empty() {
                text.push_str(" Its open findings:");
            }
            for finding in findings {
                let file = finding
                    .file
                    .as_deref()
                    .map_or_else(String::new, |file| format!(" ({file})"));
                let _ = write!(
                    text,
                    "\n- {} [{:?}] {}{file}",
                    finding.id, finding.severity, finding.summary
                );
            }
            history.push(text);
        }
        for &artifact in &dispatch.history {
            let tests = state
                .artifacts
                .get(&artifact)
                .is_some_and(|meta| matches!(self.def.steps[meta.step].refs, Refs::Tests { .. }));
            if tests {
                history.push(self.evidence_of(artifact).await?.failing_text());
            }
        }
        Ok(history)
    }

    /// The text of a document artifact.
    async fn text_of(&self, artifact: ArtifactId) -> Result<String, String> {
        let bytes = store::read_artifact_body(&self.dir, artifact, "md")
            .await
            .map_err(|error| format!("cannot read artifact {}: {error}", artifact.0))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// An evidence artifact's record.
    pub(super) async fn evidence_of(&self, artifact: ArtifactId) -> Result<EvidenceBody, String> {
        let bytes = store::read_artifact_body(&self.dir, artifact, "evidence")
            .await
            .map_err(|error| format!("cannot read evidence {}: {error}", artifact.0))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("evidence {} is unreadable: {error}", artifact.0))
    }

    /// A failed turn: the failure; for a transient one with retries left,
    /// when the retry may start, in the failure itself and in a `retry`
    /// record after it. Whether the tree let the attempt be retried is
    /// recovery's call (a changed tree reconciles; a `keep` then still
    /// waits out the backoff).
    async fn failed(
        &mut self,
        dispatch: &Dispatch,
        attempt: AttemptId,
        class: FailureClass,
        detail: String,
    ) -> Result<Flow, RunnerError> {
        let failure = match class {
            FailureClass::Transient { reset_at } => {
                // This attempt is still unfinished, so it is not in the streak yet.
                let view = self.load().await?;
                let count = machine::transient_streak(&view.state, dispatch.step).saturating_add(1);
                let eligible_at = (count <= self.def.limits.transient_retries)
                    .then(|| machine::retry_at(self.io.clock.now(), count, reset_at));
                self.fail(
                    attempt,
                    Failure::Transient {
                        detail,
                        eligible_at,
                    },
                )
                .await?;
                if let Some(eligible_at) = eligible_at {
                    self.append(&Record::Retry {
                        attempt,
                        class: "transient".into(),
                        count,
                        eligible_at,
                    })
                    .await?;
                }
                return Ok(None);
            }
            FailureClass::ConfigError => Failure::ConfigError { detail },
            FailureClass::LocalFailure => Failure::LocalFailure { detail },
            FailureClass::Unknown => Failure::Unknown { detail },
        };
        self.fail(attempt, failure).await
    }

    /// Logs `failure` for `attempt`.
    pub(super) async fn fail(
        &mut self,
        attempt: AttemptId,
        failure: Failure,
    ) -> Result<Flow, RunnerError> {
        let outcome = format!("failed: {}", tag(&failure));
        self.append(&Record::AttemptFailed { attempt, failure })
            .await?;
        self.emit(FleetEvent::AttemptFinished { attempt, outcome })
            .await;
        Ok(None)
    }

    /// Makes the reply the attempt's outcome, as the step's kind says.
    async fn accept(
        &mut self,
        view: &View,
        step: &Step,
        dispatch: &Dispatch,
        attempt: AttemptId,
        text: &str,
        open: &[Finding],
    ) -> Result<Flow, RunnerError> {
        match step.refs {
            Refs::Change { .. } => return self.accept_change(view, dispatch, attempt, text).await,
            Refs::Tests { .. } => return self.handoff(view, dispatch, attempt).await,
            Refs::Document { .. } | Refs::ReviewDocument { .. } | Refs::ReviewChange { .. } => {}
        }
        // A read-only step's output stands only while the live tree is the
        // tip's: on drift it is dropped, and recovery pauses on the drift.
        match self.live_tree().await {
            Ok(live) if Some(live.as_str()) == machine::tree_of(&view.state, view.state.tip) => {}
            Ok(_) => return Ok(None),
            Err(error) => {
                let detail = format!("cannot observe the work tree: {error}");
                return self.fail(attempt, Failure::LocalFailure { detail }).await;
            }
        }
        let outcome = if let Refs::Document { .. } = step.refs {
            if text.trim().is_empty() {
                let detail = "the reply is empty, after the corrective prompt too".to_owned();
                return self.fail(attempt, Failure::Invalid { detail }).await;
            }
            let document = self
                .write_artifact(dispatch, None, text.as_bytes(), "md")
                .await?;
            Accepted::DocumentProduced { document }
        } else {
            let open_refs: Vec<&Finding> = open.iter().collect();
            let verdict = match contract::parse_verdict(text, &open_refs) {
                Ok(verdict) => verdict,
                Err(detail) => return self.fail(attempt, Failure::Invalid { detail }).await,
            };
            let ids = view
                .state
                .next_finding_ids(&step.id, verdict.new_findings.len());
            let new_findings: Vec<Finding> = ids
                .into_iter()
                .zip(verdict.new_findings)
                .map(|(id, finding)| Finding {
                    id,
                    severity: finding.severity,
                    summary: finding.summary,
                    file: finding.file,
                })
                .collect();
            let previous = verdict.previous;
            let change_review = matches!(step.refs, Refs::ReviewChange { .. });
            if !verdict.approved {
                // Kept beside the attempt for the user; a rejection is not an artifact.
                let _ = tokio::fs::write(self.attempt_dir(attempt).join("reply.md"), text).await;
                Accepted::ChangesRequested {
                    new_findings,
                    previous,
                }
            } else if change_review {
                let candidate = Some(dispatch.input_candidate);
                let approval = self
                    .write_artifact(dispatch, candidate, text.as_bytes(), "md")
                    .await?;
                Accepted::ChangeApproved {
                    approval,
                    new_findings,
                    previous,
                }
            } else {
                let approval = self
                    .write_artifact(dispatch, None, text.as_bytes(), "md")
                    .await?;
                Accepted::DocumentApproved {
                    approval,
                    new_findings,
                    previous,
                }
            }
        };
        self.finish(attempt, outcome, None).await
    }

    /// A change step's reply: the live tree is its candidate.
    async fn accept_change(
        &mut self,
        view: &View,
        dispatch: &Dispatch,
        attempt: AttemptId,
        text: &str,
    ) -> Result<Flow, RunnerError> {
        let candidate = match self
            .derive(view, dispatch.input_candidate, Origin::Change, true)
            .await
        {
            Ok(candidate) => candidate,
            Err(detail) => return self.fail(attempt, Failure::LocalFailure { detail }).await,
        };
        let change = self
            .write_artifact(dispatch, Some(candidate), text.as_bytes(), "md")
            .await?;
        self.finish(
            attempt,
            Accepted::ChangeProduced { change },
            Some(candidate),
        )
        .await
    }

    /// The tester's turn is done: its edits, if any, are a derived
    /// candidate, and `evidence_started` commits it and begins the evidence.
    async fn handoff(
        &mut self,
        view: &View,
        dispatch: &Dispatch,
        attempt: AttemptId,
    ) -> Result<Flow, RunnerError> {
        let candidate = match self
            .derive(view, dispatch.input_candidate, Origin::Tests, false)
            .await
        {
            Ok(candidate) => candidate,
            Err(detail) => return self.fail(attempt, Failure::LocalFailure { detail }).await,
        };
        self.append(&Record::EvidenceStarted {
            attempt,
            candidate,
            restart: 0,
        })
        .await?;
        let view = self.load().await?;
        self.evidence(&view, attempt, candidate, 0).await
    }

    /// The live tree as a candidate derived from `parent`: a new one, or,
    /// unless `always`, `parent` itself when the tree is `parent`'s. An error
    /// is a local failure, as text.
    async fn derive(
        &mut self,
        view: &View,
        parent: CandidateId,
        origin: Origin,
        always: bool,
    ) -> Result<CandidateId, String> {
        let live = self
            .live_tree()
            .await
            .map_err(|error| format!("cannot observe the work tree: {error}"))?;
        if !always && machine::tree_of(&view.state, parent) == Some(live.as_str()) {
            return Ok(parent);
        }
        match self.propose(view, &live, parent, origin).await {
            Ok(candidate) => Ok(candidate),
            Err(error) => Err(format!("cannot record the candidate: {error}")),
        }
    }

    /// A change attempt left unfinished, or failed, whose tree a
    /// `Reconcile` pause adopted: the adopted candidate is its change.
    pub(super) async fn finish_adopted(
        &mut self,
        view: &View,
        attempt: AttemptId,
        candidate: CandidateId,
    ) -> Result<Flow, RunnerError> {
        let info = &view.state.attempts[&attempt];
        let body = b"The user adopted the work tree as this change.";
        let change = self
            .write_meta(
                info.step,
                info.requires.clone(),
                Some(candidate),
                body,
                "md",
            )
            .await?;
        let flow = self
            .finish(
                attempt,
                Accepted::ChangeProduced { change },
                Some(candidate),
            )
            .await;
        self.settle(attempt).await?;
        flow
    }

    /// Writes the dispatch's step's artifact.
    async fn write_artifact(
        &self,
        dispatch: &Dispatch,
        candidate: Option<CandidateId>,
        body: &[u8],
        ext: &str,
    ) -> Result<ArtifactId, RunnerError> {
        self.write_meta(
            dispatch.step,
            dispatch.requires.clone(),
            candidate,
            body,
            ext,
        )
        .await
    }

    /// Writes an artifact with a new ID, synced before anything names it.
    pub(super) async fn write_meta(
        &self,
        step: usize,
        requires: Vec<ArtifactId>,
        candidate: Option<CandidateId>,
        body: &[u8],
        ext: &str,
    ) -> Result<ArtifactId, RunnerError> {
        let metas = store::read_artifact_metas(&self.dir).await?;
        let id = ArtifactId(metas.last().map_or(0, |meta| meta.id.0 + 1));
        let meta = ArtifactMeta {
            id,
            step,
            requires,
            candidate,
        };
        store::write_artifact(&self.dir, &meta, body, ext).await?;
        Ok(id)
    }

    /// Logs the attempt's outcome and discards its scratch clone.
    pub(super) async fn finish(
        &mut self,
        attempt: AttemptId,
        outcome: Accepted,
        candidate: Option<CandidateId>,
    ) -> Result<Flow, RunnerError> {
        let name = tag(&outcome);
        self.append(&Record::AttemptFinished {
            attempt,
            outcome,
            candidate,
        })
        .await?;
        let _ = tokio::fs::remove_dir_all(self.attempt_dir(attempt).join("tree")).await;
        self.emit(FleetEvent::AttemptFinished {
            attempt,
            outcome: name,
        })
        .await;
        Ok(None)
    }

    /// The final observation: the live tree is the approved one, and the
    /// run completes; or it is not, and the run pauses on the drift.
    pub(super) async fn complete(
        &mut self,
        candidate: CandidateId,
        tree: String,
        evidence: ArtifactId,
        approval: ArtifactId,
    ) -> Result<Flow, RunnerError> {
        let observed = match self.live_tree().await {
            Ok(observed) => observed,
            Err(error) => {
                let detail = format!("cannot observe the work tree: {error}");
                return self
                    .pause(PauseReason::LocalFailure, detail)
                    .await
                    .map(Some);
            }
        };
        if observed != tree {
            let detail = format!(
                "the live tree {observed} is not the approved tree {tree} of candidate {}",
                candidate.0
            );
            return self.pause(PauseReason::Drift, detail).await.map(Some);
        }
        self.append(&Record::Completed {
            candidate,
            tree: tree.clone(),
            evidence,
            approval,
            observed_tree: observed,
        })
        .await?;
        self.disown().await?;
        let diff_command = format!("git diff {} {tree}", self.file.base_tree);
        self.emit(FleetEvent::Completed { tree, diff_command })
            .await;
        Ok(Some(RunEnd::Completed))
    }
}

/// The open findings of step `step_id`, in ID order.
fn open_findings(state: &RunState, step_id: &str) -> Vec<Finding> {
    let prefix = format!("{step_id}/");
    state
        .findings
        .iter()
        .filter(|(id, (_, status))| id.starts_with(&prefix) && *status == Status::Open)
        .map(|(_, (finding, _))| finding.clone())
        .collect()
}

/// A record's `type` tag, as the log spells it.
fn tag(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The `--stat` of what the vendor changed in its clone, if anything. An
/// error is a local failure, as text.
async fn access_violation(scratch: &Scratch, before: &str) -> Result<Option<String>, String> {
    let after = snapshot::tree_of(&scratch.cwd, &scratch.index)
        .await
        .map_err(|error| format!("cannot observe the scratch clone: {error}"))?;
    if after == before {
        return Ok(None);
    }
    let clone = Repo {
        top: scratch.top.clone(),
        cwd_rel: PathBuf::new(),
        git_dir: scratch.top.join(".git"),
    };
    let stat = snapshot::diff(&clone, before, &after, true)
        .await
        .unwrap_or_else(|error| format!("(no diffstat: {error})"));
    Ok(Some(if stat.trim().is_empty() {
        format!("the tree changed from {before} to {after}")
    } else {
        stat
    }))
}
