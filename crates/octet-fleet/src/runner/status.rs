//! `status`: what each run's log says, read without taking any lock.

use std::path::Path;

use super::RunnerError;
use crate::ownership;
use crate::record::{Record, Status};
use crate::state::replay;
use crate::store;

/// One run, as its log has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    /// The run's ID.
    pub id: String,
    /// The workflow's name.
    pub workflow: String,
    /// `running`, `paused`, `completed`, `stopped` or `failed`.
    pub state: String,
    /// The latest attempt's step, if any ran.
    pub step: Option<String>,
    /// The latest attempt's round (0 before any).
    pub round: u32,
    /// The findings still open, of every step.
    pub open_findings: usize,
    /// The git refs the run made (`refs/octet-fleet/<run>/<seq>`), one per
    /// candidate.
    pub refs: Vec<String>,
    /// Why the run cannot be read (its state is then `unreadable`): a
    /// half-created run, or a damaged file.
    pub error: Option<String>,
}

/// Summarises run `run`, or every run under `data_dir` (oldest first).
///
/// Read-only: no lock is taken and nothing is written. The log is replayed
/// as it is (a torn last line is skipped, not cut). A run that has not
/// ended is `running` while the process its lock file names
/// ([`ownership::run_holder`]) is alive, and otherwise `paused`: a pause,
/// an interrupt or a crash, which `resume` continues. A run whose log never got its first record (a start that
/// died) is `failed`.
///
/// Listing every run, a run that cannot be read (a half-created one, a
/// damaged file) is listed `unreadable`, with its error, rather than failing
/// the listing.
///
/// # Errors
///
/// For `run` only: [`RunnerError::Store`] when it cannot be read (or there
/// is no such run); [`RunnerError::Replay`] when its log contradicts its
/// workflow.
pub async fn status(data_dir: &Path, run: Option<&str>) -> Result<Vec<RunSummary>, RunnerError> {
    let ids = match run {
        Some(run) => vec![run.to_owned()],
        None => store::list_runs(data_dir).await?,
    };
    let mut summaries = Vec::with_capacity(ids.len());
    for id in ids {
        match summary(data_dir, &id).await {
            Ok(summary) => summaries.push(summary),
            // One damaged run must not hide every other.
            Err(error) if run.is_none() => summaries.push(RunSummary {
                id,
                workflow: String::new(),
                state: "unreadable".into(),
                step: None,
                round: 0,
                open_findings: 0,
                refs: Vec::new(),
                error: Some(error.to_string()),
            }),
            Err(error) => return Err(error),
        }
    }
    Ok(summaries)
}

async fn summary(data_dir: &Path, id: &str) -> Result<RunSummary, RunnerError> {
    let (dir, file) = store::open(data_dir, id).await?;
    let records = store::read_log(&dir).await?;
    let refs = records
        .iter()
        .filter_map(|record| match record {
            Record::Candidate { id: candidate, .. } => {
                Some(format!("refs/octet-fleet/{id}/{}", candidate.0))
            }
            _ => None,
        })
        .collect();
    let mut summary = RunSummary {
        id: id.to_owned(),
        workflow: file.workflow.name.clone(),
        state: "failed".into(),
        step: None,
        round: 0,
        open_findings: 0,
        refs,
        error: None,
    };
    if records.is_empty() {
        return Ok(summary);
    }
    let metas = store::read_artifact_metas(&dir).await?;
    let state = replay(&file.workflow, &records, &metas)?;
    if let Some(info) = state.attempts.values().next_back() {
        summary.step = Some(file.workflow.steps[info.step].id.clone());
        summary.round = info.round;
    }
    summary.open_findings = state
        .findings
        .values()
        .filter(|(_, status)| *status == Status::Open)
        .count();
    summary.state = if state.completed {
        "completed"
    } else if state.stopped {
        "stopped"
    } else if state.failed.is_some() {
        "failed"
    } else if ownership::run_holder(&dir).await.is_some() {
        "running"
    } else {
        "paused"
    }
    .into();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use octet_testkit::TempDir;

    use super::*;

    #[tokio::test]
    async fn a_half_created_run_is_flagged_not_fatal() {
        let data = TempDir::new("octet-fleet-status");
        let run = data.path().join("fleet").join("f0000000001-00ab");
        std::fs::create_dir_all(&run).unwrap();
        // A start that died while writing its run file.
        std::fs::write(run.join("run.json"), "{\"work").unwrap();
        let runs = status(data.path(), None).await.expect("lists the runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "f0000000001-00ab");
        assert_eq!(runs[0].state, "unreadable");
        assert!(runs[0].error.is_some(), "{:?}", runs[0]);
        // Asked for by name, the run's own error is the answer.
        assert!(status(data.path(), Some("f0000000001-00ab")).await.is_err());
    }
}
