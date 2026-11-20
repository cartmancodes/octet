//! `octet pair`: one command from nothing to a paired phone. It starts the hub if none answers
//! (installing the per-user service, as `octet hub install` does), opens a pairing, shows its QR
//! code and link, waits for a phone, and asks the owner at the Mac whether to pair the phone that
//! came, by the six-digit code both show. Exit code 0 when a phone was paired, 1 otherwise.
//!
//! The hub never says when a held phone leaves, so while the question is open the command asks
//! `pair_wait` again about once a second (one request at a time: the hub keeps one waiter): the
//! same pairing means the phone is still there.
use crate::args::CliError;
use octet_hub::{Store, plain};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::UnixStream,
    time::{Instant, sleep, sleep_until, timeout},
};

/// How long a hub that was just started has to answer on its control socket.
const START_WITHIN: Duration = Duration::from_secs(15);
/// How often the control socket is tried while the hub starts.
const START_POLL: Duration = Duration::from_millis(100);
/// How long `status` may take before the hub counts as not running.
const STATUS_WITHIN: Duration = Duration::from_secs(2);
/// How long `pair_secret` (which waits up to 10 seconds for the relay) or `pair_answer` (which
/// waits for the device to be saved) may take.
const REQUEST_WITHIN: Duration = Duration::from_secs(15);
/// How long the owner has to answer for one phone: the hub's own two minutes.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// How often, while the question is open, the hub is asked whether the phone is still held.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long one of those asks may take: a hub holding a phone answers at once.
const POLL_WITHIN: Duration = Duration::from_secs(5);
/// The longest control answer read, in bytes.
const ANSWER_LIMIT: u64 = 64 * 1024;
/// Longest device name, platform or hub message printed, in characters.
const NAME_CHARS: usize = 64;
/// Longest reason from the hub printed, in characters.
const REASON_CHARS: usize = 200;

/// Said when the owner refused the phone, or gave no answer before the input ended.
pub(crate) const NOT_PAIRED: &str = "Not paired";
/// Said when two minutes passed without an answer.
pub(crate) const TIMED_OUT: &str = "Pairing timed out";
/// Said when the held phone's connection ended before the answer.
pub(crate) const PHONE_LEFT: &str = "The phone left";
/// Said when the hub could not save the device the owner said yes to.
pub(crate) const NOT_SAVED: &str = "Could not save the device";
/// Said when `tmux` is not on the `PATH`.
pub(crate) const NO_TMUX: &str =
    "tmux isn't installed: the phone can watch and answer, but can't start sessions";
/// Said when the pairing link ran out, or was replaced, before a phone came.
pub(crate) const LINK_EXPIRED: &str =
    "The link expired before a phone used it; run octet pair again";
/// Said when the terminal is too narrow for the QR code.
pub(crate) const WIDEN: &str = "Widen the terminal to show the QR code";

/// Where `octet pair` sends its requests: the hub's control socket, or a test's stand-in.
pub(crate) trait Control {
    /// The hub's one-line answer to `request`; `None` when no hub answers.
    async fn call(&self, request: &Value) -> Option<Value>;
}

/// The hub's control socket.
#[derive(Debug)]
pub(crate) struct Socket(PathBuf);

impl Socket {
    /// The control socket of the hub whose files are in `directory`.
    pub(crate) fn of(directory: &Path) -> Self {
        Self(octet_core::remote::run::run_dir(directory).join("hub.sock"))
    }
}

impl Control for Socket {
    /// One request line, one answer line, on a connection of its own. The write side stays open
    /// until the answer: the hub takes a closed one as the client leaving.
    async fn call(&self, request: &Value) -> Option<Value> {
        let stream = UnixStream::connect(&self.0).await.ok()?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(read.take(ANSWER_LIMIT))
            .read_line(&mut line)
            .await
            .ok()?;
        serde_json::from_str(&line).ok()
    }
}

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
    let Some(link) = opened["link"].as_str() else {
        let reason = opened["reason"]
            .as_str()
            .or_else(|| opened["error"].as_str())
            .unwrap_or("The hub opened no pairing");
        say(out, &plain(reason, REASON_CHARS))?;
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
        .call(&json!({"type": "pair_wait"}))
        .await
        .ok_or_else(no_answer)?;
    let Some(question) = Question::read(&waited) else {
        let said = if waited["expired"] == true {
            LINK_EXPIRED
        } else {
            waited["error"]
                .as_str()
                .unwrap_or("The hub sent no phone to pair")
        };
        say(out, &plain(said, REASON_CHARS))?;
        return Ok(1);
    };
    ask(&control, &mut input, out, &question).await
}

/// A phone the hub holds for the owner's answer, as `pair_wait` tells of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Question {
    /// The pairing's id, which the answer names.
    pending: u64,
    /// The six digits the phone shows.
    code: String,
    /// The phone's name, as it claims.
    name: String,
    /// Its platform, as it claims.
    platform: String,
    /// Whether a device of its name is paired already, and pairing replaces it.
    replaces: bool,
}

impl Question {
    /// The question in a `pair_wait` answer; `None` for any other answer, or a code that is not
    /// six digits.
    fn read(answer: &Value) -> Option<Self> {
        let code = answer["code"].as_str()?;
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            pending: answer["pending"].as_u64()?,
            code: code.to_owned(),
            name: answer["name"].as_str()?.to_owned(),
            platform: answer["platform"].as_str()?.to_owned(),
            replaces: answer["replaces"].as_bool().unwrap_or(false),
        })
    }

    /// `Phone shows 482 913? Pair "<name>" (<platform>)? [y/N]`, or `Replace` in place of `Pair`
    /// when it replaces a device. The name and platform are the phone's claims, shown plain.
    fn prompt(&self) -> String {
        let (first, last) = self.code.split_at(3);
        let verb = if self.replaces { "Replace" } else { "Pair" };
        format!(
            "Phone shows {first} {last}? {verb} \"{}\" ({})? [y/N]",
            plain(&self.name, NAME_CHARS),
            plain(&self.platform, NAME_CHARS),
        )
    }
}

/// How the question ended.
enum Heard {
    /// The owner answered this line.
    Line,
    /// The input ended, or failed, before a whole line.
    Ended,
    /// Two minutes passed.
    TimedOut,
    /// The hub no longer holds the phone.
    Gone,
}

/// Asks the owner about `question` on `input` until a line, the end of the input, two minutes,
/// or the phone leaving, and answers the hub.
async fn ask(
    control: &impl Control,
    input: &mut (impl AsyncBufRead + Unpin),
    out: &mut impl Write,
    question: &Question,
) -> Result<i32, CliError> {
    write!(out, "{} ", question.prompt())
        .and_then(|()| out.flush())
        .map_err(|e| terminal(&e))?;
    let deadline = Instant::now() + ANSWER_WITHIN;
    let mut line = String::new();
    let heard = {
        let read = input.read_line(&mut line);
        let poll = still_held(control, question.pending);
        tokio::pin!(read, poll);
        loop {
            tokio::select! {
                biased;
                read = &mut read => {
                    break if read.is_ok_and(|n| n > 0) { Heard::Line } else { Heard::Ended };
                }
                () = sleep_until(deadline) => break Heard::TimedOut,
                held = &mut poll => {
                    if !held {
                        break Heard::Gone;
                    }
                    poll.set(still_held(control, question.pending));
                }
            }
        }
    };
    // A line the input ended in the middle of is no answer.
    let heard = match heard {
        Heard::Line if !line.ends_with('\n') => Heard::Ended,
        heard => heard,
    };
    // The prompt's line ends here, unless the owner's Enter ended it.
    if !matches!(heard, Heard::Line) {
        say(out, "")?;
    }
    match heard {
        Heard::Line | Heard::Ended => {
            let yes = matches!(heard, Heard::Line) && is_yes(&line);
            let answer = answer(control, question.pending, yes).await;
            answered(out, answer.as_ref())
        }
        Heard::TimedOut => {
            // The hub refuses it on its own clock too; this only tells the phone sooner.
            let _ = answer(control, question.pending, false).await;
            say(out, TIMED_OUT)?;
            Ok(1)
        }
        Heard::Gone => {
            // The hub remembers how the pairing it last let go ended.
            let answer = answer(control, question.pending, false).await;
            let timed_out = answer
                .as_ref()
                .is_some_and(|answer| answer["error"] == TIMED_OUT);
            say(out, if timed_out { TIMED_OUT } else { PHONE_LEFT })?;
            Ok(1)
        }
    }
}

/// Whether the hub, asked after [`POLL_EVERY`], still holds the pairing `pending`. A hub that
/// holds a phone tells of it at once; anything else (no answer, an expired secret, another
/// pairing, another waiter) means it has let this one go.
async fn still_held(control: &impl Control, pending: u64) -> bool {
    sleep(POLL_EVERY).await;
    let answer = within(POLL_WITHIN, control.call(&json!({"type": "pair_wait"}))).await;
    answer.is_some_and(|answer| answer["pending"].as_u64() == Some(pending))
}

/// The hub's answer to `pair_answer` for `pending`.
async fn answer(control: &impl Control, pending: u64, accept: bool) -> Option<Value> {
    let request = json!({"type": "pair_answer", "pending": pending, "accept": accept});
    within(REQUEST_WITHIN, control.call(&request)).await
}

/// Says how `pair_answer` went; 0 when the phone was paired.
fn answered(out: &mut impl Write, answer: Option<&Value>) -> Result<i32, CliError> {
    let answer = answer.ok_or_else(no_answer)?;
    if answer["paired"] == true {
        let device = answer["device"].as_str().unwrap_or_default();
        say(out, &format!("Paired \"{}\"", plain(device, NAME_CHARS)))?;
        return Ok(0);
    }
    let said = match answer["error"].as_str() {
        None => NOT_PAIRED.to_owned(),
        Some(error @ (TIMED_OUT | PHONE_LEFT | NOT_SAVED)) => error.to_owned(),
        Some(other) => plain(other, REASON_CHARS),
    };
    say(out, &said)?;
    Ok(1)
}

/// Whether `line` says yes: `y` or `yes`, in any case.
fn is_yes(line: &str) -> bool {
    let word = line.trim();
    word.eq_ignore_ascii_case("y") || word.eq_ignore_ascii_case("yes")
}

/// Shows the pairing: its QR code where `width` fits it (any width when unknown), the link, and
/// what to do with them.
fn show(out: &mut impl Write, link: &str, width: Option<usize>) -> Result<(), CliError> {
    say(out, "Pair a phone")?;
    say(out, "")?;
    match octet_tui::qr_rows(link) {
        Some(rows)
            if width
                .is_none_or(|width| rows.first().map_or(0, |row| row.chars().count()) <= width) =>
        {
            for row in rows {
                say(out, &row)?;
            }
            say(out, "")?;
        }
        Some(_) => say(out, WIDEN)?,
        None => say(out, "The link is too long for a QR code")?,
    }
    say(out, link)?;
    say(out, "Expires in 10 minutes · works once")?;
    say(
        out,
        "On the phone: open the Octet app → Pair → scan, or open the link",
    )
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

/// Whether a hub answers `status` on `control`.
async fn answers(control: &impl Control) -> bool {
    within(STATUS_WITHIN, control.call(&json!({"type": "status"})))
        .await
        .is_some()
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

/// `call`'s answer, if it comes within `limit`.
async fn within(limit: Duration, call: impl Future<Output = Option<Value>>) -> Option<Value> {
    timeout(limit, call).await.ok().flatten()
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
    failed("The hub stopped answering")
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
