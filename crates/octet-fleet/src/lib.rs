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
pub mod snapshot;
pub mod state;
pub mod step;
pub mod store;
pub mod validate;

#[cfg(test)]
mod sim;

use record::{AttemptId, CandidateId, Origin, PauseReason};

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
    },
}
