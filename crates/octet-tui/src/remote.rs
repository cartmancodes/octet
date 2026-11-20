//! `/remote-control`: where the hub stands, and pairing a phone with the owner's yes at the desk.
//! Everything goes through the hub's control socket. Nothing here starts, installs or runs
//! anything, and nothing changes system settings: with no hub answering, it says to run
//! `octet pair` in a terminal, which does.
use crate::{
    app::{APPROVAL_ARM, App},
    pairing::{
        self, Control, HUB_SILENT, Heard, Outcome, Question, REQUEST_WITHIN, STATUS_WITHIN, Waited,
    },
};
use serde_json::json;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// Said when no hub answers `/remote-control pair`.
pub(crate) const NO_HUB: &str = "The hub isn't running. Run \"octet pair\" in a terminal first.";
/// Said when the link ran out, or was replaced, before a phone came.
const LINK_EXPIRED: &str =
    "The link expired before a phone used it; run /remote-control pair again";

/// Where the hub stands, as its `status` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HubState {
    /// No hub answers on its control socket.
    NotRunning,
    /// A hub answered.
    Running {
        /// Where it stands with the relay: `off`, `connecting` or `registered`.
        relay: String,
        /// How many phones are paired; `None` when it did not say.
        devices: Option<u64>,
    },
}

/// Asks the hub on `control` where it stands.
pub(crate) async fn status(control: &impl Control) -> HubState {
    let request = json!({"type": "status"});
    let Some(reply) = pairing::within(STATUS_WITHIN, control.call(&request)).await else {
        return HubState::NotRunning;
    };
    HubState::Running {
        relay: reply["relay"].as_str().unwrap_or_default().to_owned(),
        devices: reply["devices"].as_u64(),
    }
}

/// The report of plain `/remote-control`: whether the hub runs, whether the relay has it, and how
/// many phones are paired; then why this session's bridge is off, when it is.
pub(crate) fn report(state: &HubState, bridge_off: Option<&str>) -> String {
    let mut lines = match state {
        HubState::NotRunning => vec![
            "Hub: not running · run \"octet pair\" in a terminal to start it".to_owned(),
            "Relay: unknown".to_owned(),
            "Phones paired: unknown".to_owned(),
        ],
        HubState::Running { relay, devices } => {
            let relay = match relay.as_str() {
                "registered" => "has the hub",
                "connecting" => "connecting",
                "off" => "off · nothing is paired and no pairing is open",
                _ => "unknown",
            };
            let devices = devices.map_or_else(|| "unknown".to_owned(), |n| n.to_string());
            vec![
                "Hub: running".to_owned(),
                format!("Relay: {relay}"),
                format!("Phones paired: {devices}"),
            ]
        }
    };
    lines.extend(bridge_off.map(str::to_owned));
    lines.join("\n")
}

/// What opening a pairing came to.
pub(crate) enum Opened {
    /// No hub answers.
    NoHub,
    /// The hub gave no link, with why.
    Refused(String),
    /// The hub's link, and the wait for a phone, already running.
    Link {
        /// The pairing link, which carries the secret: it goes only into the transcript.
        link: String,
        /// The wait for a phone to use it.
        wait: PairWait,
    },
}

/// Opens a pairing on the hub at `control`: `NoHub` unless a hub answers `status`, then a link
/// from `pair_secret` and the wait for a phone, started.
pub(crate) async fn open<C>(control: C) -> Opened
where
    C: Control + Send + Sync + 'static,
{
    if !pairing::answers(&control).await {
        return Opened::NoHub;
    }
    let request = json!({"type": "pair_secret"});
    let Some(opened) = pairing::within(REQUEST_WITHIN, control.call(&request)).await else {
        return Opened::Refused(HUB_SILENT.to_owned());
    };
    let Some(link) = opened["link"].as_str() else {
        return Opened::Refused(pairing::no_link(&opened));
    };
    Opened::Link {
        link: link.to_owned(),
        wait: PairWait::spawn(control),
    }
}

/// What the wait for a phone tells the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Told {
    /// A phone is held: ask the owner.
    Asked(Question),
    /// The pairing ended, with what to say; nothing when there is nothing to say.
    Ended(String),
}

/// A pairing in progress, off the event loop: waiting for a phone, then for the owner's answer to
/// the question about it. Dropping it stops the wait; the hub refuses an unanswered phone on its
/// own after two minutes.
pub(crate) struct PairWait {
    /// What the wait tells.
    told: mpsc::Receiver<Told>,
    /// Takes the owner's answer, until it is given.
    answer: Option<oneshot::Sender<bool>>,
    /// The question shown, once a phone is held and until it is answered.
    pub(crate) question: Option<Question>,
    /// When the question was last drawn after being hidden; answer keys wait `APPROVAL_ARM` after
    /// that, so a key typed as it appears cannot answer it.
    shown: Option<std::time::Instant>,
    /// The wait itself.
    task: JoinHandle<()>,
}

impl PairWait {
    /// Starts waiting for a phone on the hub at `control`.
    pub(crate) fn spawn<C>(control: C) -> Self
    where
        C: Control + Send + Sync + 'static,
    {
        let (tell, told) = mpsc::channel(2);
        let (answer, answered) = oneshot::channel();
        let task = tokio::spawn(async move {
            let said = wait(&control, &tell, answered).await;
            let _ = tell.send(Told::Ended(said)).await;
        });
        Self {
            told,
            answer: Some(answer),
            question: None,
            shown: None,
            task,
        }
    }

    /// The next thing the wait tells; `None` once it stopped without saying how it ended.
    pub(crate) async fn next(&mut self) -> Option<Told> {
        self.told.recv().await
    }

    /// Gives the owner's answer to the open question, which closes it.
    pub(crate) fn answer(&mut self, yes: bool) {
        if self.question.take().is_some()
            && let Some(answer) = self.answer.take()
        {
            let _ = answer.send(yes);
        }
    }

    /// The question was drawn: its answer keys wait from now, unless it was on screen already.
    pub(crate) fn drawn(&mut self) {
        self.shown.get_or_insert_with(std::time::Instant::now);
    }

    /// Something covers the question: it must be seen again before a key answers it.
    pub(crate) fn hidden(&mut self) {
        self.shown = None;
    }

    /// The question has been on screen for `APPROVAL_ARM`.
    fn armed(&self) -> bool {
        self.shown
            .is_some_and(|shown| shown.elapsed() >= APPROVAL_ARM)
    }

    /// Ages the question past the arm delay, as if it had been on screen that long.
    #[cfg(test)]
    pub(crate) fn arm(&mut self) {
        self.shown = std::time::Instant::now().checked_sub(APPROVAL_ARM);
    }
}

impl Drop for PairWait {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Waits for a phone, tells the interface the question, hears the owner's answer (or two minutes
/// passing, or the phone leaving) and answers the hub; what to say about how it ended.
async fn wait(
    control: &impl Control,
    tell: &mpsc::Sender<Told>,
    answered: oneshot::Receiver<bool>,
) -> String {
    let Some(waited) = control.call(&json!({"type": "pair_wait"})).await else {
        return HUB_SILENT.to_owned();
    };
    let question = match Waited::read(&waited) {
        Waited::Asked(question) => question,
        Waited::Expired => return LINK_EXPIRED.to_owned(),
        Waited::Not(said) => return said,
    };
    let pending = question.pending;
    if tell.send(Told::Asked(question)).await.is_err() {
        // The interface is gone; the hub refuses the phone on its own clock.
        return String::new();
    }
    // An interface gone without answering said no.
    let answer = async { answered.await.unwrap_or(false) };
    match pairing::hear(control, pending, answer).await {
        Heard::Answer(yes) => match pairing::answer(control, pending, yes).await {
            Some(reply) => Outcome::read(&reply).said,
            None => HUB_SILENT.to_owned(),
        },
        heard @ (Heard::TimedOut | Heard::Gone) => {
            let timed_out = heard == Heard::TimedOut;
            pairing::let_go(control, pending, timed_out)
                .await
                .to_owned()
        }
    }
}

/// Shows what opening a pairing came to: the QR code and link (in the transcript only, never on
/// the status line), then the wait for a phone; or why there is none.
pub(crate) fn show(app: &mut App, opened: Opened) {
    match opened {
        Opened::NoHub => app.note(NO_HUB),
        Opened::Refused(why) => app.note(why),
        Opened::Link { link, wait } => {
            let room = crate::view::transcript_room(app.last_width, app.sidebar);
            app.note_apart(pairing::pair_lines(&link, room.map(usize::from)).join("\n"));
            app.hint("Waiting for a phone…");
            app.pairing = Some(wait);
        }
    }
}

/// Acts on what the pairing's wait told: shows the question, or says how the pairing ended.
pub(crate) fn apply(app: &mut App, told: Option<Told>) {
    match told {
        Some(Told::Asked(question)) => {
            let prompt = question.prompt();
            if let Some(wait) = app.pairing.as_mut() {
                wait.question = Some(question);
                wait.hidden();
                app.hint(prompt);
            }
        }
        Some(Told::Ended(said)) => {
            app.pairing = None;
            if !said.is_empty() {
                app.note(said);
            }
        }
        None => {
            app.pairing = None;
            app.error("The pairing stopped");
        }
    }
}

/// A key while the pairing question is open: `Y` says yes; `N`, Enter, Esc or Ctrl+C says no.
/// Other keys do nothing, and no key answers until the question has been seen.
pub(crate) fn key(app: &mut App, key: crossterm::event::KeyEvent) {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
    let yes = match key.code {
        _ if ctrl_c => false,
        KeyCode::Char('y' | 'Y') => true,
        KeyCode::Char('n' | 'N') | KeyCode::Enter | KeyCode::Esc => false,
        _ => return,
    };
    let Some(wait) = app.pairing.as_mut() else {
        return;
    };
    if wait.armed() {
        wait.answer(yes);
        app.hint("Answering the hub…");
    } else {
        // Typed as the question appeared: never an answer.
        app.hint("The question just opened; press Y or N again to answer");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing;

    #[test]
    fn the_report_is_hub_relay_and_phones() {
        let running = |relay: &str, devices| HubState::Running {
            relay: relay.into(),
            devices,
        };
        assert_eq!(
            report(&running("registered", Some(2)), None),
            "Hub: running\nRelay: has the hub\nPhones paired: 2"
        );
        assert_eq!(
            report(&running("connecting", Some(0)), None),
            "Hub: running\nRelay: connecting\nPhones paired: 0"
        );
        assert_eq!(
            report(&running("off", None), None),
            "Hub: running\nRelay: off · nothing is paired and no pairing is open\nPhones paired: unknown"
        );
        assert_eq!(
            report(&HubState::NotRunning, None),
            "Hub: not running · run \"octet pair\" in a terminal to start it\nRelay: unknown\nPhones paired: unknown"
        );
    }

    #[test]
    fn remote_control_says_why_this_sessions_bridge_is_off() {
        const OFF: &str = "Remote control is off: the socket path /x/run/1.sock is too long";
        let text = report(&HubState::NotRunning, Some(OFF));
        assert!(text.ends_with(&format!("\n{OFF}")), "{text}");
        assert_eq!(text.lines().count(), 4, "{text}");
    }

    #[test]
    fn the_qr_keeps_one_width_through_the_transcripts_wrapping() {
        let link = format!("octet://pair/{}", "A".repeat(140));
        let rows = pairing::qr_rows(&link).unwrap();
        let width = rows[0].chars().count();
        for room in [width, width + 40] {
            for row in &rows {
                let wrapped = crate::view::transcript::wrap(row, room);
                assert_eq!(wrapped, std::slice::from_ref(row), "room {room}");
            }
        }
    }

    #[tokio::test]
    async fn the_hub_status_comes_from_its_socket() {
        let dir = octet_testkit::TempDir::new("octet-rc-status");
        let socket = pairing::Socket::of(dir.path());
        assert_eq!(status(&socket).await, HubState::NotRunning, "no socket");
        let served = crate::test_support::serve_hub(
            &octet_core::remote::run::run_dir(dir.path()),
            r#"{"relay":"registered","devices":2}"#,
        );
        assert_eq!(
            status(&socket).await,
            HubState::Running {
                relay: "registered".into(),
                devices: Some(2)
            }
        );
        assert_eq!(served.await.unwrap().trim(), r#"{"type":"status"}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hub_that_never_answers_is_given_up_on() {
        let dir = octet_testkit::TempDir::new("octet-rc-mute");
        let run = octet_core::remote::run::run_dir(dir.path());
        std::fs::create_dir_all(&run).unwrap();
        let listener = tokio::net::UnixListener::bind(run.join("hub.sock")).unwrap();
        let _held = tokio::spawn(async move {
            let _stream = listener.accept().await;
            std::future::pending::<()>().await;
        });
        let started = tokio::time::Instant::now();
        assert!(matches!(
            open(pairing::Socket::of(dir.path())).await,
            Opened::NoHub
        ));
        let waited = started.elapsed();
        assert!(
            (pairing::STATUS_WITHIN..pairing::STATUS_WITHIN + std::time::Duration::from_millis(50))
                .contains(&waited),
            "{waited:?}"
        );
    }
}
