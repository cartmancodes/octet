//! Antigravity changing its own mode. A mode other than Octet's that it
//! names while none of Octet's switches awaits a reply is its own change:
//! one looser than Octet's during a turn cancels the turn, and Octet's mode
//! is always set back, at once or when the turn ends. The header keeps
//! Octet's mode throughout.
use super::{
    launch::{mode_id, mode_of},
    tools::is_looser,
};
use crate::live::{DriverError, Mode};

/// What Octet does about Antigravity moving itself to a mode.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Moved {
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
pub(super) fn moved(id: &str, mode: Mode, running: bool) -> Option<Moved> {
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
pub(super) fn notice(id: &str, mode: Mode) -> String {
    format!(
        "Antigravity moved to its {id} mode. Octet sets {} back before the next prompt",
        mode.label()
    )
}

/// The error that stops the session when Antigravity would not go back to
/// mode `id`, for `reason`.
pub(super) fn would_not_go_back(id: &str, reason: &str) -> DriverError {
    format!(
        "Antigravity would not go back to mode {id} ({reason}); \
         Octet stopped the session so Antigravity never acts outside the mode you chose"
    )
    .into()
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
