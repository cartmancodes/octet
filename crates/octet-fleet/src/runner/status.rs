//! `status`: what each run's log says, read without taking any lock.

use std::fs::{File, TryLockError};
use std::path::Path;

use super::RunnerError;
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
}

/// Summarises run `run`, or every run under `data_dir` (oldest first).
///
/// Read-only: no lock is taken and nothing is written. The log is replayed
/// as it is (a torn last line is skipped, not cut). A run that has not
/// ended is `running` while another process holds its lock, which a
/// momentary non-blocking shared lock attempt tells (released at once), and
/// otherwise `paused`: a pause, an interrupt or a crash, which `resume`
/// continues. A run whose log never got its first record (a start that
/// died) is `failed`.
///
/// # Errors
///
/// [`RunnerError::Store`] when a run cannot be read (for `run`, when there
/// is no such run); [`RunnerError::Replay`] when a log contradicts its
/// workflow.
pub async fn status(data_dir: &Path, run: Option<&str>) -> Result<Vec<RunSummary>, RunnerError> {
    let ids = match run {
        Some(run) => vec![run.to_owned()],
        None => store::list_runs(data_dir).await?,
    };
    let mut summaries = Vec::with_capacity(ids.len());
    for id in ids {
        summaries.push(summary(data_dir, &id).await?);
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
    } else if locked(&dir.root.join("lock")) {
        "running"
    } else {
        "paused"
    }
    .into();
    Ok(summary)
}

/// Whether another process holds the lock file `path` now. A missing or
/// unreadable file is not held.
fn locked(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    // Dropping the file releases a lock this took.
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}
