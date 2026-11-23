//! Fleet workflows: a YAML-defined sequence of vendor sessions (research,
//! review, implement, test) that Octet drives under approval gates.
#![forbid(unsafe_code)]

pub mod classify;
pub mod contract;
pub mod definition;
pub mod evidence;
pub mod gate;
pub mod machine;
pub mod ownership;
pub mod record;
pub mod recover;
pub mod runner;
pub mod snapshot;
pub mod state;
pub mod step;
pub mod store;
pub mod validate;

#[cfg(test)]
mod sim;

use record::{AttemptId, CandidateId, Origin, PauseReason};

pub use runner::{
    Clock, RunEnd, RunSummary, RunnerError, RunnerIo, Start, resume, start, status, stop,
};
pub use step::Approver;

/// The run's signals: the name of the signal (`SIGINT`, `SIGTERM`) that asked
/// the run to stop, once one has.
pub type Signals = tokio::sync::watch::Receiver<Option<&'static str>>;

/// The name of the signal `signals` holds now, if any.
pub(crate) fn signal_now(signals: Option<&Signals>) -> Option<&'static str> {
    signals.and_then(|receiver| *receiver.borrow())
}

/// Resolves with the name of the signal `signals` holds, as soon as it holds
/// one; never, without a receiver or once its sender is gone. Cancel-safe.
pub(crate) async fn signalled(signals: &mut Option<Signals>) -> &'static str {
    if let Some(receiver) = signals {
        loop {
            if let Some(signal) = *receiver.borrow_and_update() {
                return signal;
            }
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
    std::future::pending().await
}

/// What a run reports to its front end, in order, as it happens. The log is
/// the record of a run; these are for showing it.
#[derive(Debug, Clone)]
pub enum FleetEvent {
    /// The run began (or resumed).
    RunStarted {
        /// The run's ID.
        run: String,
    },
    /// An attempt of a step began.
    AttemptStarted {
        /// The attempt.
        attempt: AttemptId,
        /// The step's ID.
        step: String,
        /// The step's round.
        round: u32,
        /// The engine the step runs on.
        engine: String,
        /// The model it asks for, if the profile names one.
        model: Option<String>,
    },
    /// An event of an attempt's session, already in its journal.
    Session {
        /// The attempt whose session it is.
        attempt: AttemptId,
        /// The event.
        event: octet_core::Event,
    },
    /// A candidate was recorded.
    Candidate {
        /// The candidate.
        id: CandidateId,
        /// What made it.
        origin: Origin,
    },
    /// An evidence command is about to run.
    EvidenceCommand {
        /// Its position in the tests step's list.
        index: usize,
        /// The command line.
        command: String,
    },
    /// An attempt ended.
    AttemptFinished {
        /// The attempt.
        attempt: AttemptId,
        /// How it ended, as the log records it.
        outcome: String,
    },
    /// A transient failure: the attempt is retried.
    Retrying {
        /// The attempt that failed.
        attempt: AttemptId,
        /// When the retry may start, in seconds since the Unix epoch.
        at: u64,
    },
    /// The run paused.
    Paused {
        /// Why.
        reason: PauseReason,
        /// What the user needs to know to decide.
        detail: String,
    },
    /// The run waits for the user's decision.
    NeedsDecision {
        /// Why.
        reason: PauseReason,
        /// The decisions open to the user.
        options: Vec<&'static str>,
    },
    /// The run completed.
    Completed {
        /// The final tree object.
        tree: String,
        /// A command that shows the run's change.
        diff_command: String,
        /// The findings still open (minor ones: a blocking finding cannot
        /// be open at an approval), in ID order.
        open_findings: Vec<record::Finding>,
    },
}
