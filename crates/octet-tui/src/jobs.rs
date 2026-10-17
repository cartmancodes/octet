//! Work that reads the disk or runs programs, kept off the event loop so
//! vendor events and approvals keep flowing. One job runs at a time. A job
//! outlives a reconnect (it moves to the next interface); on quit only an
//! export is waited for, so it is never cut short. A job a phone asked for
//! (an `/export`) carries its [`Asker`], who is answered when it ends.
use crate::{app::App, remote};
use octet_core::{ExportError, RecentSession};
use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};
use tokio::task::JoinHandle;

/// How long quitting waits for an export.
pub(crate) const GRACE: Duration = Duration::from_secs(5);

/// What a finished job reports.
pub(crate) enum Done {
    Remote(remote::Checks),
    /// A pairing code asked of the hub; the secret is shown once and kept out of logs.
    Pair(remote::Pairing),
    Sessions(Vec<RecentSession>),
    Exported {
        path: PathBuf,
        result: Result<(), ExportError>,
    },
}

/// What a failed export says before the reason; the desk's job and a phone's `export` share it.
pub(crate) const EXPORT_FAILED: &str = "Export failed: ";

/// The words for a failed export.
pub(crate) fn export_failure(error: &octet_core::ExportError) -> String {
    format!("{EXPORT_FAILED}{error}")
}

/// A phone that asked for a job, to be answered when it ends: the hub connection it came on,
/// the hub's req, and the device's name for the desk's note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asker {
    pub(crate) peer: u64,
    pub(crate) req: u64,
    pub(crate) by: String,
}

/// A job a command asked for: its name for the status line, and its work.
pub(crate) struct Job {
    pub(crate) label: &'static str,
    /// Quitting waits for it (an export: cut short, it would leave a
    /// partial file); other jobs are abandoned.
    pub(crate) must_finish: bool,
    pub(crate) work: Pin<Box<dyn Future<Output = Done> + Send>>,
    /// The phone that asked for it, if one did.
    pub(crate) asker: Option<Asker>,
}

impl Job {
    /// The `/remote-control` checks.
    pub(crate) fn remote(directory: PathBuf) -> Self {
        Self {
            label: "Checking phone access…",
            must_finish: false,
            asker: None,
            work: Box::pin(async move { Done::Remote(remote::probe(&directory).await) }),
        }
    }
    /// A pairing secret from the hub, for `/remote-control pair`.
    pub(crate) fn pair(directory: PathBuf) -> Self {
        Self {
            label: "Asking the hub for a pairing code…",
            must_finish: false,
            asker: None,
            work: Box::pin(async move {
                let run = octet_core::remote::run::run_dir(&directory);
                Done::Pair(remote::pairing(&run, remote::CHECK_TIMEOUT).await)
            }),
        }
    }
    /// The journals beside this one, for `/sessions`.
    pub(crate) fn sessions(directory: PathBuf, workspace: PathBuf, limit: usize) -> Self {
        Self {
            label: "Reading recent sessions…",
            must_finish: false,
            asker: None,
            work: Box::pin(async move {
                Done::Sessions(octet_core::recent_sessions(&directory, &workspace, limit).await)
            }),
        }
    }
    /// A copy of the journal, for `/export`.
    pub(crate) fn export(journal: PathBuf, path: PathBuf) -> Self {
        Self {
            label: "Exporting the journal…",
            must_finish: true,
            asker: None,
            work: Box::pin(async move {
                let result = octet_core::export_journal(&journal, &path).await;
                Done::Exported { path, result }
            }),
        }
    }
}

/// The job running off the loop.
pub(crate) struct Running {
    pub(crate) label: &'static str,
    must_finish: bool,
    task: JoinHandle<Done>,
    asker: Option<Asker>,
}

impl Running {
    pub(crate) fn spawn(job: Job) -> Self {
        Self {
            label: job.label,
            must_finish: job.must_finish,
            task: tokio::spawn(job.work),
            asker: job.asker,
        }
    }
    /// The phone that asked for it, to be answered now that it ended.
    pub(crate) fn into_asker(self) -> Option<Asker> {
        self.asker
    }
    /// Waits for the job to end.
    pub(crate) async fn wait(&mut self) -> Ended {
        match (&mut self.task).await {
            Ok(done) => Ended::Done(done),
            Err(error) => Ended::Failed(error.to_string()),
        }
    }
}

/// How a job ended.
pub(crate) enum Ended {
    Done(Done),
    /// The task panicked or was cancelled.
    Failed(String),
    /// It outlived its session's grace period and was stopped.
    Stopped(&'static str),
}

/// Waits up to `grace` for `running` to end, stopping it after that.
pub(crate) async fn finish(running: Option<Running>, grace: Duration) -> Option<Ended> {
    let mut running = running?;
    Some(
        if let Ok(ended) = tokio::time::timeout(grace, running.wait()).await {
            ended
        } else {
            running.task.abort();
            Ended::Stopped(running.label)
        },
    )
}

/// Quitting: waits up to `GRACE` for a job that must finish (an export),
/// and stops any other at once. Nothing is left to show the result.
pub(crate) async fn on_quit(running: Option<Running>) {
    match running {
        Some(running) if running.must_finish => {
            let _ = finish(Some(running), GRACE).await;
        }
        Some(running) => running.task.abort(),
        None => {}
    }
}

/// Shows how a job ended.
pub(crate) fn apply(app: &mut App, ended: Ended) {
    let done = match ended {
        Ended::Done(done) => done,
        Ended::Failed(error) => return app.error(format!("A background task failed: {error}")),
        Ended::Stopped(label) => {
            return app.error(format!("{label} did not finish in time and was stopped"));
        }
    };
    match done {
        Done::Remote(mut checks) => {
            checks.bridge_off.clone_from(&app.remote_off);
            let report = remote::report(&checks);
            app.note(report.text.as_str());
            // The status line, one row high, gets the count of problems.
            app.hint(remote::summary(&report));
        }
        Done::Pair(pairing) => crate::commands::show_pairing(app, pairing),
        Done::Sessions(listed) => crate::commands::show_sessions(app, listed),
        Done::Exported { path, result } => match result {
            Ok(()) => app.note(format!("Exported journal to {}", path.display())),
            Err(error) => app.error(export_failure(&error)),
        },
    }
}
