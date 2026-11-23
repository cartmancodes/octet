//! The vendor's background tasks, followed so a front end that runs a turn
//! unattended does not take the turn as over while one still runs.
//!
//! Claude starts a turn of its own when a background task ends after its
//! turn did. A headless run, or a fleet attempt, must therefore not end
//! while a task runs, nor just after the last one ended: [`Background`]
//! says when it is safe to, with [`BACKGROUND_GRACE`] after the last task.
use crate::Event;
use std::time::Duration;
use tokio::time::{Instant, sleep_until};

/// How long a run waits, after the vendor's last background task ends
/// between turns, for the turn Claude then starts of its own.
pub const BACKGROUND_GRACE: Duration = Duration::from_secs(5);

/// The vendor's background tasks. Claude starts a turn of its own when one
/// ends after its turn did, so a headless run must not end while one runs,
/// nor just after the last one ended.
#[derive(Debug, Default)]
pub struct Background {
    /// Tasks running.
    tasks: usize,
    /// The last task ended between turns: until then a turn may follow.
    grace: Option<Instant>,
}
impl Background {
    /// Follows the session's events; `idle` when no turn runs.
    pub fn track(&mut self, event: &Event, idle: bool) {
        match event {
            Event::Background(tasks) => {
                if *tasks == 0 && self.tasks > 0 && idle {
                    self.grace = Some(Instant::now() + BACKGROUND_GRACE);
                }
                self.tasks = *tasks;
            }
            Event::Started => self.grace = None,
            _ => {}
        }
    }

    /// A task runs, or one ended so recently that a turn may still follow.
    pub fn pending(&self) -> bool {
        self.tasks > 0 || self.grace.is_some()
    }

    /// How many of the vendor's background tasks run.
    pub fn tasks(&self) -> usize {
        self.tasks
    }

    /// Ends the grace once it passes; never, when there is none.
    pub async fn grace_over(&mut self) {
        match self.grace {
            Some(at) => {
                sleep_until(at).await;
                self.grace = None;
            }
            None => std::future::pending().await,
        }
    }
}
