//! `octet pair`: one command from nothing to a paired phone. It starts the hub if none answers
//! (installing the per-user service, as `octet hub install` does), opens a pairing, shows its QR
//! code and link, waits for a phone, and asks the owner at the Mac whether to pair the phone that
//! came, by the six-digit code both show. Exit code 0 when a phone was paired, 1 otherwise.
//!
//! The hub never says when a held phone leaves, so while the question is open the command asks
//! `pair_wait` again about once a second (one request at a time: the hub keeps one waiter): the
//! same pairing means the phone is still there. Each `pair_wait` names the attempt `pair_secret`
//! gave, so a pairing opened elsewhere meanwhile ends this one as expired.
use crate::args::CliError;
use octet_hub::Store;
use octet_tui::pairing::{
    Control, HUB_SILENT, Heard, Outcome, REQUEST_WITHIN, Socket, Waited, answer, answers, hear,
    let_go, no_link, opened_link, pair_lines, wait_request, within,
};
use serde_json::json;
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt as _, BufReader},
    net::UnixStream,
    time::{sleep, timeout},
};

/// How long a hub that was just started has to answer on its control socket.
const START_WITHIN: Duration = Duration::from_secs(15);
/// How often the control socket is tried while the hub starts.
const START_POLL: Duration = Duration::from_millis(100);

/// Said when `tmux` is not on the `PATH`.
pub(crate) const NO_TMUX: &str =
    "tmux isn't installed: the phone can watch and answer, but can't start sessions";
/// Said when the pairing link ran out, or was replaced, before a phone came.
pub(crate) const LINK_EXPIRED: &str =
    "The link expired before a phone used it; run octet pair again";

/// What `octet pair` takes from the world around it, which tests replace.
#[derive(Debug)]
pub(crate) struct World<C, S> {
    /// The hub's control socket.
    pub(crate) control: C,
    /// Starts the hub when none answers; the command installs the per-user service.
    pub(crate) start: S,
    /// Whether `tmux` is there for the hub to start sessions with.
    pub(crate) tmux: bool,
    /// The terminal's width in columns, when it is known.
    pub(crate) width: Option<usize>,
}

/// Pairs a phone with the hub whose files are in `directory`, asking the owner on `input` and
/// saying how it goes on `output`; exit code 0 when a phone was paired, 1 otherwise.
///
/// # Errors
///
/// The service cannot be installed, the hub does not answer within 15 seconds of it, the hub
/// stops answering, or `output` cannot be written.
pub(crate) async fn pair(
    directory: PathBuf,
    input: impl AsyncBufRead + Unpin,
    output: impl Write,
) -> Result<i32, CliError> {
    let installing = directory.clone();
    let world = World {
        control: Socket::of(&directory),
        start: move || async move { crate::hub::install(installing, false).await.map(|_| ()) },
        tmux: tmux_on_path(),
        width: terminal_width(),
    };
    pair_with(&directory, input, output, world).await
}

/// [`pair`] in `world`.
///
/// # Errors
///
/// The same as [`pair`].
pub(crate) async fn pair_with<C, S, F>(
    directory: &Path,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl Write,
    world: World<C, S>,
) -> Result<i32, CliError>
where
    C: Control,
    S: FnOnce() -> F,
    F: Future<Output = Result<(), CliError>>,
{
    let World {
        control,
        start,
        tmux,
        width,
    } = world;
    let out = &mut output;
    if !answers(&control).await {
        say(out, "Starting the hub")?;
        start().await?;
        if !started(&control).await {
            return Err(failed(format!(
                "The hub didn't start within {} seconds",
                START_WITHIN.as_secs()
            )));
        }
    }
    let opened = within(
        REQUEST_WITHIN,
        control.call(&json!({"type": "pair_secret"})),
    )
    .await
    .ok_or_else(no_answer)?;
    let Some((link, attempt)) = opened_link(&opened) else {
        say(out, &no_link(&opened))?;
        return Ok(1);
    };
    show(out, link, width)?;
    let moved = elsewhere(directory, link).await;
    if moved > 0 {
        say(
            out,
            &format!("{moved} phones are paired through another relay and must pair again"),
        )?;
    }
    if !tmux {
        say(out, NO_TMUX)?;
    }
    say(out, "Waiting for a phone…")?;
    let waited = control
        .call(&wait_request(attempt))
        .await
        .ok_or_else(no_answer)?;
    let question = match Waited::read(&waited) {
        Waited::Asked(question) => question,
        Waited::Expired => {
            say(out, LINK_EXPIRED)?;
            return Ok(1);
        }
        Waited::Not(said) => {
            say(out, &said)?;
            return Ok(1);
        }
    };
    ask(&control, attempt, &mut input, out, &question).await
}

/// Asks the owner about `question` on `input` until a line, the end of the input, two minutes,
/// or the phone leaving, and answers the hub.
async fn ask(
    control: &impl Control,
    attempt: &str,
    input: &mut (impl AsyncBufRead + Unpin),
    out: &mut impl Write,
    question: &octet_tui::pairing::Question,
) -> Result<i32, CliError> {
    write!(out, "{} ", question.prompt())
        .and_then(|()| out.flush())
        .map_err(|e| terminal(&e))?;
    let mut line = String::new();
    // A whole line answers; the input ending, failing, or ending in the middle of a line is no
    // answer.
    let read = async {
        input
            .read_line(&mut line)
            .await
            .is_ok_and(|n| n > 0 && line.ends_with('\n'))
    };
    let heard = hear(control, attempt, &question.pending, read).await;
    // The prompt's line ends here, unless the owner's Enter ended it.
    if heard != Heard::Answer(true) {
        say(out, "")?;
    }
    match heard {
        Heard::Answer(whole) => {
            let yes = whole && is_yes(&line);
            let reply = answer(control, &question.pending, yes)
                .await
                .ok_or_else(no_answer)?;
            let outcome = Outcome::read(&reply);
            say(out, &outcome.said)?;
            Ok(i32::from(!outcome.paired))
        }
        Heard::TimedOut | Heard::Gone => {
            let said = let_go(control, &question.pending, heard == Heard::TimedOut).await;
            say(out, said)?;
            Ok(1)
        }
    }
}

/// Whether `line` says yes: `y` or `yes`, in any case.
fn is_yes(line: &str) -> bool {
    let word = line.trim();
    word.eq_ignore_ascii_case("y") || word.eq_ignore_ascii_case("yes")
}

/// Shows the pairing: its QR code where `width` fits it (any width when unknown), the link, and
/// what to do with them.
fn show(out: &mut impl Write, link: &str, width: Option<usize>) -> Result<(), CliError> {
    pair_lines(link, width)
        .iter()
        .try_for_each(|line| say(out, line))
}

/// How many paired phones went through another relay than the one in `link`, from the device
/// list as `octet hub devices` reads it: they cannot reach the hub now.
async fn elsewhere(directory: &Path, link: &str) -> usize {
    let Some(relay) = link_relay(link) else {
        return 0;
    };
    let Ok(store) = Store::open_devices(directory).await else {
        return 0;
    };
    store
        .devices()
        .iter()
        .filter(|device| device.relay != relay)
        .count()
}

/// The relay address a pairing link names, as the hub dials it and a device records it:
/// `wss://<host>` (version byte 1) or `ws://<host>` (0), with the port if there is one.
fn link_relay(link: &str) -> Option<String> {
    /// The version byte, the identity key, the channel id and the secret.
    const HEAD: usize = 1 + 32 * 3;
    let payload = unbase64url(link.strip_prefix("octet://pair/")?)?;
    let scheme = match payload.first()? {
        1 => "wss",
        0 => "ws",
        _ => return None,
    };
    let authority = std::str::from_utf8(payload.get(HEAD..)?).ok()?;
    (!authority.is_empty()).then(|| format!("{scheme}://{authority}"))
}

/// The bytes of unpadded base64url `text`; `None` for any other character.
fn unbase64url(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut held) = (0_u32, 0_u32);
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&c| c == byte)?;
        bits = (bits << 6 | u32::try_from(value).ok()?) & 0xff_ffff;
        held += 6;
        if held >= 8 {
            held -= 8;
            bytes.push(u8::try_from(bits >> held & 0xff).ok()?);
        }
    }
    Some(bytes)
}

/// Whether a hub answers on `control` within [`START_WITHIN`].
async fn started(control: &impl Control) -> bool {
    timeout(START_WITHIN, async {
        while !answers(control).await {
            sleep(START_POLL).await;
        }
    })
    .await
    .is_ok()
}

/// Writes `line` and a newline to `out`, at once.
fn say(out: &mut impl Write, line: &str) -> Result<(), CliError> {
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|e| terminal(&e))
}

/// A failure to write to the terminal.
fn terminal(error: &std::io::Error) -> CliError {
    failed(format!("Couldn't write to the terminal: {error}"))
}

/// The hub stopped answering.
fn no_answer() -> CliError {
    failed(HUB_SILENT)
}

/// A failure while running.
fn failed(message: impl Into<String>) -> CliError {
    CliError::Run(message.into())
}

/// Whether `tmux` is on the `PATH`, where the hub's service finds it too.
fn tmux_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|search| crate::service::find_on_path(&search, "tmux").is_some())
}

/// The terminal's width in columns, when there is a terminal.
fn terminal_width() -> Option<usize> {
    crossterm::terminal::size()
        .ok()
        .map(|(columns, _)| usize::from(columns))
}

/// Standard input as an async reader of lines, read on a thread of its own: a blocking read of a
/// terminal cannot be cancelled, and on the runtime's blocking pool it would hold the exit up
/// until Enter is pressed. The thread ends with the process.
///
/// # Errors
///
/// The pipe to the thread cannot be made.
pub(crate) fn stdin_reader() -> std::io::Result<BufReader<UnixStream>> {
    let (mut theirs, ours) = std::os::unix::net::UnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let ours = UnixStream::from_std(ours)?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut theirs);
    });
    Ok(BufReader::new(ours))
}

#[cfg(test)]
mod tests;
