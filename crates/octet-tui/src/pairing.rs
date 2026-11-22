//! Pairing a phone at the Mac, shared by `octet pair` and `/remote-control pair` so both say the
//! same: the hub's control socket, what the hub tells of a held phone, the question put to the
//! owner, how long it stays open, and what each ending says.
//!
//! The hub never says when a held phone leaves, so while the question is open the hub is asked
//! `pair_wait` again about once a second, one request at a time on a connection of its own (the
//! hub keeps one waiter): the same pairing means the phone is still there. Every `pair_wait` names
//! the attempt `pair_secret` gave, so a newer opening elsewhere ends this one as expired. The
//! attempt and the pairing's `pending` id are random strings, so neither matches anything once
//! the hub has restarted.
use qrcode::{QrCode, render::unicode::Dense1x2};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::UnixStream,
    time::{Instant, sleep, sleep_until, timeout},
};

/// How long `status` may take before the hub counts as not running.
pub const STATUS_WITHIN: Duration = Duration::from_secs(2);
/// How long `pair_secret` (which waits up to 10 seconds for the relay) or `pair_answer` (which
/// waits for the device to be saved) may take.
pub const REQUEST_WITHIN: Duration = Duration::from_secs(15);
/// How long the owner has to answer for one phone: the hub's own two minutes.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// How often, while the question is open, the hub is asked whether the phone is still held.
pub const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long one of those asks may take: a hub holding a phone answers at once.
pub const POLL_WITHIN: Duration = Duration::from_secs(5);
/// The longest control answer read, in bytes.
const ANSWER_LIMIT: u64 = 64 * 1024;
/// Longest device name or platform shown, in characters.
pub const NAME_CHARS: usize = 64;
/// Longest reason from the hub shown, in characters.
pub const REASON_CHARS: usize = 200;

/// Said when the owner refused the phone, or gave no answer before the input ended.
pub const NOT_PAIRED: &str = "Not paired";
/// Said when two minutes passed without an answer.
pub const TIMED_OUT: &str = "Pairing timed out";
/// Said when the held phone's connection ended before the answer.
pub const PHONE_LEFT: &str = "The phone left";
/// Said when the hub could not save the device the owner said yes to.
pub const NOT_SAVED: &str = "Could not save the device";
/// Said when the terminal is too narrow for the QR code.
pub const WIDEN: &str = "Widen the terminal to show the QR code";
/// Said when the hub stops answering in the middle of a pairing.
pub const HUB_SILENT: &str = "The hub stopped answering";

/// Where pairing requests go: the hub's control socket, or a test's stand-in.
pub trait Control {
    /// The hub's one-line answer to `request`; `None` when no hub answers.
    fn call(&self, request: &Value) -> impl Future<Output = Option<Value>> + Send;
}

/// The hub's control socket.
#[derive(Debug, Clone)]
pub struct Socket(PathBuf);

impl Socket {
    /// The control socket of the hub whose files are in `directory`.
    #[must_use]
    pub fn of(directory: &Path) -> Self {
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

/// `call`'s answer, if it comes within `limit`.
pub async fn within(limit: Duration, call: impl Future<Output = Option<Value>>) -> Option<Value> {
    timeout(limit, call).await.ok().flatten()
}

/// Whether a hub answers `status` on `control`.
pub async fn answers(control: &impl Control) -> bool {
    within(STATUS_WITHIN, control.call(&json!({"type": "status"})))
        .await
        .is_some()
}

/// The link and attempt of a `pair_secret` answer; `None` when it gave no link.
#[must_use]
pub fn opened_link(opened: &Value) -> Option<(&str, &str)> {
    Some((opened["link"].as_str()?, opened["attempt"].as_str()?))
}

/// The `pair_wait` request for the opening `attempt`.
#[must_use]
pub fn wait_request(attempt: &str) -> Value {
    json!({"type": "pair_wait", "attempt": attempt})
}

/// What to say when `pair_secret` gave no link: the hub's reason or error, shown plain.
#[must_use]
pub fn no_link(opened: &Value) -> String {
    let reason = opened["reason"]
        .as_str()
        .or_else(|| opened["error"].as_str())
        .unwrap_or("The hub opened no pairing");
    plain(reason, REASON_CHARS)
}

/// What a `pair_wait` came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    /// A phone is held: the question for the owner.
    Asked(Question),
    /// The link ran out, or was replaced, before a phone came.
    Expired,
    /// Anything else, with what to say about it.
    Not(String),
}

impl Waited {
    /// What the `pair_wait` answer `waited` says.
    #[must_use]
    pub fn read(waited: &Value) -> Self {
        if let Some(question) = Question::read(waited) {
            Self::Asked(question)
        } else if waited["expired"] == true {
            Self::Expired
        } else {
            let said = waited["error"]
                .as_str()
                .unwrap_or("The hub sent no phone to pair");
            Self::Not(plain(said, REASON_CHARS))
        }
    }
}

/// A phone the hub holds for the owner's answer, as `pair_wait` tells of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The pairing's id, which the answer names.
    pub pending: String,
    /// The six digits the phone shows.
    pub code: String,
    /// The phone's name, as it claims.
    pub name: String,
    /// Its platform, as it claims.
    pub platform: String,
    /// Whether a device of its name is paired already, and pairing replaces it.
    pub replaces: bool,
}

impl Question {
    /// The question in a `pair_wait` answer; `None` for any other answer, or a code that is not
    /// six digits.
    #[must_use]
    pub fn read(answer: &Value) -> Option<Self> {
        let code = answer["code"].as_str()?;
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            pending: answer["pending"].as_str()?.to_owned(),
            code: code.to_owned(),
            name: answer["name"].as_str()?.to_owned(),
            platform: answer["platform"].as_str()?.to_owned(),
            replaces: answer["replaces"].as_bool().unwrap_or(false),
        })
    }

    /// `Phone shows 482 913? Pair "<name>" (<platform>)? [y/N]`, or `Replace` in place of `Pair`
    /// when it replaces a device. The name and platform are the phone's claims, shown plain.
    #[must_use]
    pub fn prompt(&self) -> String {
        let verb = if self.replaces { "Replace" } else { "Pair" };
        format!(
            "Phone shows {}? {verb} \"{}\" ({})? [y/N]",
            code_shown(&self.code),
            plain(&self.name, NAME_CHARS),
            plain(&self.platform, NAME_CHARS),
        )
    }
}

/// A six-digit code as the phone and the Mac show it: `482 913`.
#[must_use]
pub fn code_shown(code: &str) -> String {
    match (code.get(..3), code.get(3..)) {
        (Some(first), Some(last)) if !last.is_empty() => format!("{first} {last}"),
        _ => code.to_owned(),
    }
}

/// How the question ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard<A> {
    /// The owner answered, with what the answer future gave.
    Answer(A),
    /// Two minutes passed.
    TimedOut,
    /// The hub no longer holds the phone.
    Gone,
}

/// Waits for the owner's `answer` to the question about the held pairing `pending` of the
/// opening `attempt`, for two minutes, asking the hub about once a second whether it still holds
/// the phone.
pub async fn hear<A>(
    control: &impl Control,
    attempt: &str,
    pending: &str,
    answer: impl Future<Output = A>,
) -> Heard<A> {
    let deadline = Instant::now() + ANSWER_WITHIN;
    let poll = still_held(control, attempt, pending);
    tokio::pin!(answer, poll);
    loop {
        tokio::select! {
            biased;
            answer = &mut answer => break Heard::Answer(answer),
            () = sleep_until(deadline) => break Heard::TimedOut,
            held = &mut poll => {
                if !held {
                    break Heard::Gone;
                }
                poll.set(still_held(control, attempt, pending));
            }
        }
    }
}

/// Whether the hub, asked after [`POLL_EVERY`], still holds the pairing `pending`. A hub that
/// holds a phone tells of it at once; anything else (no answer, an expired secret, another
/// pairing, another waiter) means it has let this one go.
async fn still_held(control: &impl Control, attempt: &str, pending: &str) -> bool {
    sleep(POLL_EVERY).await;
    let answer = within(POLL_WITHIN, control.call(&wait_request(attempt))).await;
    answer.is_some_and(|answer| answer["pending"].as_str() == Some(pending))
}

/// The hub's answer to `pair_answer` for `pending`.
pub async fn answer(control: &impl Control, pending: &str, accept: bool) -> Option<Value> {
    let request = json!({"type": "pair_answer", "pending": pending, "accept": accept});
    within(REQUEST_WITHIN, control.call(&request)).await
}

/// After a question ended unanswered ([`Heard::TimedOut`] when `timed_out`, else
/// [`Heard::Gone`]): refuses the phone with one `pair_answer`, and says why the question ended.
/// The hub remembers how the pairing it last let go ended, so a phone gone because its two
/// minutes ran out says so.
pub async fn let_go(control: &impl Control, pending: &str, timed_out: bool) -> &'static str {
    let answer = answer(control, pending, false).await;
    let hub_timed_out = answer
        .as_ref()
        .is_some_and(|answer| answer["error"] == TIMED_OUT);
    if timed_out || hub_timed_out {
        TIMED_OUT
    } else {
        PHONE_LEFT
    }
}

/// How the owner's answer went, as the hub's `pair_answer` reply says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the phone was paired.
    pub paired: bool,
    /// What to say: `Paired "<name>"`, [`NOT_PAIRED`], or why not.
    pub said: String,
}

impl Outcome {
    /// What the hub's `pair_answer` `reply` says.
    #[must_use]
    pub fn read(reply: &Value) -> Self {
        if reply["paired"] == true {
            let device = reply["device"].as_str().unwrap_or_default();
            return Self {
                paired: true,
                said: format!("Paired \"{}\"", plain(device, NAME_CHARS)),
            };
        }
        let said = match reply["error"].as_str() {
            None => NOT_PAIRED.to_owned(),
            Some(error @ (TIMED_OUT | PHONE_LEFT | NOT_SAVED)) => error.to_owned(),
            Some(other) => plain(other, REASON_CHARS),
        };
        Self {
            paired: false,
            said,
        }
    }
}

/// The QR code of `link` as equal-width rows, dark modules on a light quiet zone; None when `link`
/// is too long.
#[must_use]
pub fn qr_rows(link: &str) -> Option<Vec<String>> {
    let image = QrCode::new(link.as_bytes())
        .ok()?
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .build();
    Some(image.lines().map(str::to_owned).collect())
}

/// What shows a pairing: its QR code where `width` columns fit it (any width when unknown), the
/// link, and what to do with them.
#[must_use]
pub fn pair_lines(link: &str, width: Option<usize>) -> Vec<String> {
    let mut lines = vec!["Pair a phone".to_owned(), String::new()];
    match qr_rows(link) {
        Some(rows)
            if width
                .is_none_or(|width| rows.first().map_or(0, |row| row.chars().count()) <= width) =>
        {
            lines.extend(rows);
            lines.push(String::new());
        }
        Some(_) => lines.push(WIDEN.to_owned()),
        None => lines.push("The link is too long for a QR code".to_owned()),
    }
    lines.push(link.to_owned());
    lines.push("Expires in 10 minutes · works once".to_owned());
    lines.push("On the phone: open the Octet app → Pair → scan, or open the link".to_owned());
    lines
}

/// `text` on one line, as the hub's `plain` gives it: line breaks and tabs as one space, other
/// control characters and bidirectional overrides dropped, cut to `limit` characters with `…`.
fn plain(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in text.chars() {
        if matches!(c, '\n' | '\r' | '\t') {
            // A run of line breaks and tabs is one space.
            if !out.ends_with(' ') && count < limit {
                out.push(' ');
                count += 1;
            }
            continue;
        }
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        if count == limit {
            out.push('…');
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hub's question about a phone called `name`.
    fn question(name: &str, replaces: bool) -> Value {
        json!({"pending": PENDING, "code": "482913", "name": name, "platform": "ios", "replaces": replaces})
    }

    /// The id of the pairing the hub holds, as the hub makes them.
    const PENDING: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    #[test]
    fn the_prompt_shows_the_code_spaced_and_the_phones_claims_plain() {
        let asked = Question::read(&question("Pixel", false)).unwrap();
        assert_eq!(
            asked.prompt(),
            "Phone shows 482 913? Pair \"Pixel\" (ios)? [y/N]"
        );
        let replacing = Question::read(&question("Pixel", true)).unwrap();
        assert_eq!(
            replacing.prompt(),
            "Phone shows 482 913? Replace \"Pixel\" (ios)? [y/N]"
        );
        let odd = Question::read(&question("Pix\nel\u{202e}\u{1b}", false)).unwrap();
        assert_eq!(
            odd.prompt(),
            "Phone shows 482 913? Pair \"Pix el\" (ios)? [y/N]"
        );
        let long = Question::read(&question(&"x".repeat(70), false)).unwrap();
        assert!(long.prompt().contains(&format!("\"{}…\"", "x".repeat(64))));
        assert_eq!(code_shown("482913"), "482 913");
    }

    #[test]
    fn a_question_or_a_link_with_a_numeric_id_is_none() {
        let mut asked = question("Pixel", false);
        asked["pending"] = json!(7);
        assert_eq!(Question::read(&asked), None);
        assert_eq!(
            Question::read(&question("Pixel", false)).unwrap().pending,
            PENDING
        );
        let opened = |attempt: Value| json!({"link": "octet://pair/AQID", "attempt": attempt});
        assert_eq!(opened_link(&opened(json!(3))), None);
        assert_eq!(
            opened_link(&opened(json!(PENDING))),
            Some(("octet://pair/AQID", PENDING))
        );
    }

    #[test]
    fn only_six_digits_make_a_question() {
        for code in ["48291", "4829134", "48291x", ""] {
            let mut asked = question("Pixel", false);
            asked["code"] = json!(code);
            assert_eq!(Question::read(&asked), None, "{code}");
        }
        assert_eq!(Waited::read(&json!({"expired": true})), Waited::Expired);
        assert_eq!(
            Waited::read(&json!({"error": "Another pairing is open"})),
            Waited::Not("Another pairing is open".into())
        );
    }

    #[test]
    fn each_reply_says_how_the_answer_went() {
        let said = |reply: Value| Outcome::read(&reply);
        assert_eq!(
            said(json!({"paired": true, "device": "Pixel"})),
            Outcome {
                paired: true,
                said: "Paired \"Pixel\"".into()
            }
        );
        assert_eq!(said(json!({"paired": false})).said, NOT_PAIRED);
        for error in [TIMED_OUT, PHONE_LEFT, NOT_SAVED] {
            assert_eq!(said(json!({"error": error})).said, error);
        }
        assert_eq!(
            said(json!({"error": "No phone is waiting for that answer"})).said,
            "No phone is waiting for that answer"
        );
    }
}
