//! Antigravity changing its own mode. A mode other than Octet's that it
//! names while none of Octet's switches awaits a reply is its own change:
//! one looser than Octet's during a turn cancels the turn, and Octet's mode
//! is always set back, at once or when the turn ends. The header keeps
//! Octet's mode throughout.
use super::{
    AntigravityProtocol, NAMED_NO_MODE, Outstanding,
    launch::{mode_id, mode_of},
    reported_mode, reported_reason,
    tools::is_looser,
};
use crate::live::{DriverError, Event, Mode, acp, deadline_after, driver::seconds, protocol::Core};
use serde_json::Value;
use tokio::time::Instant;

/// What Octet does about Antigravity moving itself to a mode.
#[derive(Debug, PartialEq, Eq)]
enum Moved {
    /// The mode lets more run unasked than Octet's, during a turn: the turn
    /// is cancelled, for this reason, and the mode set back at once.
    Looser(String),
    /// Anything else: Octet says so and sets its mode back when the turn
    /// ends, or at once when idle.
    Other,
}

/// Octet's mode, to be set back after Antigravity moved itself.
#[derive(Default)]
pub(super) struct Follow {
    /// Antigravity moved to this mode, which Octet has reported; Octet's
    /// mode is set back once no turn runs and no switch awaits its reply.
    pub(super) due: Option<String>,
    /// The set-back was sent and awaits its reply.
    pub(super) sent: bool,
}

/// What Antigravity naming its mode `id` asks of Octet, whose mode is
/// `mode`, while a turn is `running` or not; `None` when `id` is Octet's.
/// `plan`, and a mode Octet does not know, are never looser.
fn moved(id: &str, mode: Mode, running: bool) -> Option<Moved> {
    if id == mode_id(mode) {
        return None;
    }
    let looser = mode_of(id, mode).is_some_and(|moved| is_looser(moved, mode));
    Some(if looser && running {
        let label = mode.label();
        Moved::Looser(format!(
            "Antigravity moved to its {id} mode while Octet shows {label}. Octet cancelled the turn and set {label} back"
        ))
    } else {
        Moved::Other
    })
}

/// What Octet says when Antigravity moved itself to its mode `id` while
/// Octet shows `mode`.
fn notice(id: &str, mode: Mode) -> String {
    format!(
        "Antigravity moved to its {id} mode. Octet sets {} back before the next prompt",
        mode.label()
    )
}

/// The error that stops the session when Antigravity would not go back to
/// mode `id`, for `reason`.
fn would_not_go_back(id: &str, reason: &str) -> DriverError {
    format!(
        "Antigravity would not go back to mode {id} ({reason}); \
         Octet stopped the session so Antigravity never acts outside the mode you chose"
    )
    .into()
}

impl AntigravityProtocol {
    /// Antigravity names its mode `id`. Once the session is ready, while none
    /// of Octet's switches (a set-back included) awaits a reply, a mode other
    /// than Octet's is Antigravity's own change (see `moved`): a
    /// looser one during a turn cancels the turn and is set back at once;
    /// anything else is reported once and set back when no turn runs. The
    /// header keeps Octet's mode.
    pub(super) async fn followed(&mut self, core: &mut Core, id: &str) -> Result<(), DriverError> {
        if !core.phase.is_ready() || self.switches.pending() || self.switches.unsettled().is_some()
        {
            return Ok(());
        }
        match moved(id, core.mode, core.phase.is_running()) {
            None => Ok(()),
            Some(Moved::Looser(reason)) => {
                if !self.cancel_turn(core, reason, false).await? {
                    core.emit(Event::Notice(notice(id, core.mode)))?;
                }
                self.follow.due = None;
                self.send_set_back(core).await
            }
            Some(Moved::Other) => {
                if self.follow.due.as_deref() != Some(id) {
                    core.emit(Event::Notice(notice(id, core.mode)))?;
                    self.follow.due = Some(id.to_owned());
                }
                self.send_due_set_back(core).await
            }
        }
    }

    /// Sends the set-back that is due, once no turn runs and none of Octet's
    /// switches awaits a reply; none is needed when Antigravity last named
    /// Octet's mode itself.
    pub(super) async fn send_due_set_back(&mut self, core: &Core) -> Result<(), DriverError> {
        if self.follow.due.is_none()
            || core.phase.is_running()
            || self.switches.pending()
            || self.switches.unsettled().is_some()
        {
            return Ok(());
        }
        self.follow.due = None;
        if self.mode.as_deref() == Some(mode_id(core.mode)) {
            return Ok(());
        }
        self.send_set_back(core).await
    }

    /// Sets Octet's mode back: a switch the driver holds the next prompt
    /// for, whose reply shows nothing (see `set_back_reply`).
    async fn send_set_back(&mut self, core: &Core) -> Result<(), DriverError> {
        self.set_option(
            core,
            Outstanding::SetBack(core.mode),
            "mode",
            mode_id(core.mode),
        )
        .await?;
        let id = self.requests.last_id();
        self.switches
            .sent(id, core.mode, deadline_after(core.limits.mode_confirm));
        self.follow.sent = true;
        Ok(())
    }

    /// The set-back's reply. Only one naming `target`'s mode confirms it,
    /// and shows nothing; an error, or a reply naming no mode or another
    /// one, stops the session: Antigravity may be acting outside the mode
    /// the user chose.
    pub(super) fn set_back_reply(&mut self, v: &Value, target: Mode) -> Result<(), DriverError> {
        self.switches.answered(v);
        self.follow.sent = false;
        let id = mode_id(target);
        let reason = if let Some(error) = v.get("error") {
            acp::error_text(error)
        } else {
            let options = &v["result"]["configOptions"];
            self.note_options(options);
            match reported_mode(options) {
                Value::Null => NAMED_NO_MODE.to_owned(),
                reported if reported.as_str() == Some(id) => return Ok(()),
                reported => reported_reason(reported),
            }
        };
        Err(would_not_go_back(id, &reason))
    }

    /// A set-back Antigravity has not confirmed in time stops the session,
    /// as a refusal does.
    pub(super) fn set_back_expired(&self, core: &Core) -> Result<(), DriverError> {
        let expired = self.follow.sent
            && self
                .switches
                .deadline()
                .is_some_and(|at| at <= Instant::now());
        if !expired {
            return Ok(());
        }
        Err(would_not_go_back(
            mode_id(core.mode),
            &format!(
                "it did not answer within {}",
                seconds(core.limits.mode_confirm)
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_looser_mode_during_a_turn_cancels_it() {
        let looser = |id: &str, mode: Mode| matches!(moved(id, mode, true), Some(Moved::Looser(_)));
        assert!(looser("accept-edits", Mode::Ask));
        assert!(looser("auto", Mode::Ask));
        assert!(looser("auto", Mode::AcceptEdits));
        for (id, mode) in [
            ("plan", Mode::Ask),
            ("default", Mode::AcceptEdits),
            ("accept-edits", Mode::Auto),
            ("auto", Mode::FullAccess),
            ("unknown", Mode::Ask),
        ] {
            assert_eq!(
                moved(id, mode, true),
                Some(Moved::Other),
                "{id} under {mode:?}"
            );
        }
        // Idle, nothing is cancelled; Octet's own mode is no move.
        assert_eq!(moved("auto", Mode::Ask, false), Some(Moved::Other));
        assert_eq!(moved("default", Mode::FullAccess, true), None);
        assert_eq!(
            moved("auto", Mode::Ask, true),
            Some(Moved::Looser(
                "Antigravity moved to its auto mode while Octet shows ask. Octet cancelled the turn and set ask back"
                    .to_owned()
            ))
        );
    }
}
