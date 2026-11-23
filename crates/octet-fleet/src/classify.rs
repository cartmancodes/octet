//! How a turn that did not reply failed: the class decides what the runner
//! does next (retry, fail the run, or pause).
//!
//! [`classify`] matches the error text a session reported against a table of
//! known texts, per provider or for every one: the texts Octet itself writes,
//! the real vendors' error texts seen so far (each a test case naming where
//! it was seen), and the fake vendors' fixture texts.

/// What kind of failure ended a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// It may pass on its own: a usage or rate limit, an overloaded or
    /// unreachable service, the vendor exiting mid-turn. Retried while the
    /// tree is unchanged.
    Transient {
        /// When a usage limit resets, in seconds since the Unix epoch, if the
        /// text says.
        reset_at: Option<u64>,
    },
    /// The step cannot run as configured: an unknown model or effort, a
    /// missing binary, a vendor that is not signed in. Fails the run.
    ConfigError,
    /// Octet's own side failed: the journal or the disk. Pauses the run.
    LocalFailure,
    /// Nothing known. Pauses the run.
    Unknown,
}

/// A table row's class; a transient row's reset time comes from the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Transient,
    Config,
    Local,
}

/// One known error text: `needle` found in the lowercased text, from
/// `engine` (`None`: any provider).
struct Row {
    engine: Option<&'static str>,
    needle: &'static str,
    kind: Kind,
}

const fn any(needle: &'static str, kind: Kind) -> Row {
    Row {
        engine: None,
        needle,
        kind,
    }
}

const fn only(engine: &'static str, needle: &'static str, kind: Kind) -> Row {
    Row {
        engine: Some(engine),
        needle,
        kind,
    }
}

/// The known texts, lowercase, first match wins. Octet's own texts come
/// first: a storage failure that quotes a vendor's words is still Octet's.
const TABLE: &[Row] = &[
    // Octet's session pump (octet-core): the journal or a stalled consumer.
    any("storage failure:", Kind::Local),
    any("did not keep up with the vendor", Kind::Local),
    // Octet could not start the CLI (octet-engine's driver):
    // `Cannot start <engine>: <error>. Install the CLI and sign in first.`
    any("install the cli and sign in first", Kind::Config),
    // Octet's text when Antigravity looks signed out at session start.
    only("antigravity", "sign in to antigravity first", Kind::Config),
    // Codex: `codexErrorInfo` kinds, which Octet appends in parentheses.
    only("codex", "(usagelimitexceeded)", Kind::Transient),
    only("codex", "\"httpstatus\":429", Kind::Transient),
    only("codex", "(unauthorized)", Kind::Config),
    // Codex's refusal of a model the account does not serve.
    only(
        "codex",
        "model is not supported when using codex",
        Kind::Config,
    ),
    // Claude Code's usage-limit text, `Claude AI usage limit reached|<epoch>`.
    only("claude", "usage limit reached", Kind::Transient),
    // Claude Code's refusal of an unknown model.
    only("claude", "issue with the selected model", Kind::Config),
    // Any provider.
    any("not logged in", Kind::Config),
    any("login required", Kind::Config),
    any("please log in", Kind::Config),
    any("please run /login", Kind::Config),
    any("invalid api key", Kind::Config),
    any("authentication failed", Kind::Config),
    any("model not found", Kind::Config),
    any("unknown model", Kind::Config),
    any("model_not_found", Kind::Config),
    any("invalid model", Kind::Config),
    any("invalid effort", Kind::Config),
    any("unsupported effort", Kind::Config),
    any("invalid reasoning effort", Kind::Config),
    any("usage limit", Kind::Transient),
    any("rate limit", Kind::Transient),
    any("rate_limit", Kind::Transient),
    any("too many requests", Kind::Transient),
    any("overloaded", Kind::Transient),
    any("connection reset", Kind::Transient),
    any("econnreset", Kind::Transient),
    any("network error", Kind::Transient),
    any("stream disconnected", Kind::Transient),
    any("service unavailable", Kind::Transient),
];

/// The class of a turn that ended with `error_text` on `engine`.
/// `stopped_mid_turn` says the vendor stopped while the turn ran: an error
/// the table does not know is then transient (the process went away), where
/// otherwise it is [`FailureClass::Unknown`].
///
/// A Codex usage limit names its reset as a local clock time ("try again at
/// 9:04 AM"); reading it runs `date` once for the local time.
#[must_use]
pub fn classify(engine: &str, error_text: &str, stopped_mid_turn: bool) -> FailureClass {
    classify_at(engine, error_text, stopped_mid_turn, local_now)
}

/// [`classify`], with `now` reading the local clock (only for a Codex text
/// that names a clock time).
fn classify_at(
    engine: &str,
    error_text: &str,
    stopped_mid_turn: bool,
    now: impl FnOnce() -> Option<LocalNow>,
) -> FailureClass {
    let text = error_text.to_lowercase();
    let row = TABLE
        .iter()
        .find(|row| row.engine.is_none_or(|only| only == engine) && text.contains(row.needle));
    match row.map(|row| row.kind) {
        Some(Kind::Transient) => FailureClass::Transient {
            reset_at: reset_at(error_text).or_else(|| {
                let clock = clock_time(&text).filter(|_| engine == "codex")?;
                Some(next_occurrence(clock, now()?))
            }),
        },
        Some(Kind::Config) => FailureClass::ConfigError,
        Some(Kind::Local) => FailureClass::LocalFailure,
        None if stopped_mid_turn => FailureClass::Transient { reset_at: None },
        None => FailureClass::Unknown,
    }
}

/// Seconds in a day.
const DAY: u32 = 24 * 60 * 60;

/// The local time: the Unix second, and how far into the local day it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalNow {
    epoch: u64,
    seconds_of_day: u32,
}

/// How long `date` may take before the local time counts as unreadable.
const DATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The local time from `date`, which every macOS and Linux has at `/bin/date`
/// (else the one on `PATH`); `None` when it cannot be read within
/// [`DATE_TIMEOUT`].
fn local_now() -> Option<LocalNow> {
    let program = if std::path::Path::new("/bin/date").is_file() {
        "/bin/date"
    } else {
        "date"
    };
    let stdout = run_bounded(program, &["+%s %H %M %S"], DATE_TIMEOUT)?;
    parse_date(&String::from_utf8_lossy(&stdout))
}

/// `program`'s standard output when it exits successfully within `limit`;
/// `None` (and the process killed) otherwise. Its output is a line, so it
/// cannot fill the pipe while it is waited for.
fn run_bounded(program: &str, args: &[&str], limit: std::time::Duration) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    status.success().then_some(stdout)
}

/// `date '+%s %H %M %S'`'s output.
fn parse_date(text: &str) -> Option<LocalNow> {
    let mut fields = text.split_whitespace();
    let epoch = fields.next()?.parse().ok()?;
    let mut part = |limit: u32| fields.next()?.parse::<u32>().ok().filter(|n| *n < limit);
    let (hours, minutes, seconds) = (part(24)?, part(60)?, part(61)?);
    Some(LocalNow {
        epoch,
        seconds_of_day: hours * 3600 + minutes * 60 + seconds.min(59),
    })
}

/// The clock time a lowercased text names after "try again at ", as seconds
/// into the day: `H:MM am|pm`, or `HH:MM` on a 24-hour clock.
fn clock_time(text: &str) -> Option<u32> {
    let (_, rest) = text.split_once("try again at ")?;
    let (hours, rest) = rest.split_once(':')?;
    let number = |digits: &str| {
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u32>().ok())
            .flatten()
    };
    let hours = number(hours).filter(|_| hours.len() <= 2)?;
    let minutes = number(rest.get(..2)?).filter(|m| *m < 60)?;
    let after = &rest[2..];
    if after.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let after = after.trim_start();
    let hours = if let Some(pm) = after
        .strip_prefix("am")
        .map(|_| false)
        .or_else(|| after.strip_prefix("pm").map(|_| true))
    {
        if !(1..=12).contains(&hours) {
            return None;
        }
        hours % 12 + if pm { 12 } else { 0 }
    } else if hours < 24 {
        hours
    } else {
        return None;
    };
    Some(hours * 3600 + minutes * 60)
}

/// How long ago a named clock time may be and still count as now: Codex names
/// its reset to the minute, so a retry at that minute can fail again a few
/// seconds before the real reset, and its text names the same minute.
const JUST_PASSED: u32 = 60 * 60;

/// The Unix second of the next time the local clock shows `clock` (seconds
/// into the day): today if it is still ahead, now if it passed within the
/// last hour ([`JUST_PASSED`]; the usual backoff then applies), else
/// tomorrow. It adds the wall-clock difference, so across a daylight-saving
/// change it is up to an hour off.
fn next_occurrence(clock: u32, now: LocalNow) -> u64 {
    let ahead = if clock >= now.seconds_of_day {
        clock - now.seconds_of_day
    } else if (now.seconds_of_day + DAY - clock) % DAY <= JUST_PASSED {
        0
    } else {
        clock + DAY - now.seconds_of_day
    };
    now.epoch + u64::from(ahead)
}

/// The reset time a usage-limit text names, as Claude Code writes it: the
/// Unix seconds after a `|` at the end (`Claude AI usage limit reached|1767225600`).
fn reset_at(text: &str) -> Option<u64> {
    let (_, tail) = text.trim_end().rsplit_once('|')?;
    let digits = tail.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(engine, text, stopped mid-turn, class)`.
    const CASES: &[(&str, &str, bool, FailureClass)] = &[
        // The fake Codex's `fail` scenario, as Octet's error text writes it.
        (
            "codex",
            "You've hit your usage limit. (usageLimitExceeded)",
            false,
            FailureClass::Transient { reset_at: None },
        ),
        (
            "codex",
            "bad ({\"httpStatus\":429})",
            false,
            FailureClass::Transient { reset_at: None },
        ),
        // The fake Claude's `errors` scenario: nothing known.
        (
            "claude",
            "Fixture failure detail",
            false,
            FailureClass::Unknown,
        ),
        // ... unless the vendor went away mid-turn.
        (
            "claude",
            "Fixture failure detail",
            true,
            FailureClass::Transient { reset_at: None },
        ),
        (
            "claude",
            "Claude AI usage limit reached|1767225600",
            false,
            FailureClass::Transient {
                reset_at: Some(1_767_225_600),
            },
        ),
        (
            "claude",
            "API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}",
            false,
            FailureClass::Transient { reset_at: None },
        ),
        (
            "opencode",
            "Rate limit exceeded, retry later",
            false,
            FailureClass::Transient { reset_at: None },
        ),
        (
            "codex",
            "stream disconnected before completion: connection reset by peer",
            false,
            FailureClass::Transient { reset_at: None },
        ),
        // The opencode fixture's refusal of an unknown model.
        (
            "opencode",
            "Invalid params: model not found",
            false,
            FailureClass::ConfigError,
        ),
        (
            "claude",
            "Invalid API key · Please run /login",
            false,
            FailureClass::ConfigError,
        ),
        (
            "codex",
            "Cannot start codex: No such file or directory (os error 2). \
             Install the CLI and sign in first.",
            false,
            FailureClass::ConfigError,
        ),
        // A config error is one even when the vendor then exits.
        (
            "antigravity",
            "Not logged in",
            true,
            FailureClass::ConfigError,
        ),
        // Octet's own pump.
        (
            "codex",
            "Storage failure: No space left on device (os error 28). \
             Session stopped; journal may have an incomplete tail.",
            false,
            FailureClass::LocalFailure,
        ),
        // A Codex-only kind does not match another provider's text.
        (
            "claude",
            "odd (usageLimitExceeded)",
            false,
            FailureClass::Unknown,
        ),
        ("codex", "", false, FailureClass::Unknown),
    ];

    /// Texts the real vendors (or Octet in front of them) produced, as
    /// Octet's error event carried them; each names where it was seen.
    /// No real sample exists yet for a network error from any provider,
    /// for Codex or Antigravity being signed out, or for Claude's usage
    /// limit: those rows rest on the fixtures above.
    const REAL: &[(&str, &str, FailureClass)] = &[
        // Codex 0.15x, the 2026-11-06 music-decoder runs (three times;
        // only the clock time differed). At [`TEN_AM`], 9:04 AM passed
        // within the last hour, so it counts as now.
        (
            "codex",
            "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \
             visit https://chatgpt.com/codex/settings/usage to purchase more credits or try \
             again at 9:04 AM. (usageLimitExceeded)",
            FailureClass::Transient {
                reset_at: Some(1_800_000_000),
            },
        ),
        // Codex 0.154.0 with a model the ChatGPT account does not serve
        // (Octet's headless probe, 2026-10-23; the same text for
        // `gpt-5.5` and `gpt-6.1-terra`, whose code was `model_not_found`).
        (
            "codex",
            "The 'nonexistent-model-x' model is not supported when using Codex with a \
             ChatGPT account.",
            FailureClass::ConfigError,
        ),
        (
            "codex",
            "The 'gpt-6.1-terra' model is not supported when using Codex with a ChatGPT account.",
            FailureClass::ConfigError,
        ),
        // Claude Code 2.1.289 with an unknown model (Task 18 probe).
        (
            "claude",
            "There's an issue with the selected model (claude-nonexistent-x). It may not \
             exist or you may not have access to it.",
            FailureClass::ConfigError,
        ),
        // Claude Code without credentials (`--bare`, 2026-11-05 probe).
        (
            "claude",
            "Not logged in · Please run /login",
            FailureClass::ConfigError,
        ),
        // opencode 1.18.25 refusing a model over ACP (2026-10-23 probe).
        (
            "opencode",
            "opencode did not switch to model opencode/nonexistent: Invalid params: model \
             not found: opencode/nonexistent",
            FailureClass::ConfigError,
        ),
        // opencode's free-model quota (its own log, 2026-11-15).
        (
            "opencode",
            "AI_APICallError: Free usage limit reached. Free models work in OpenCode \
             (https://opencode.ai); subscribe to OpenCode Go to keep going: \
             https://opencode.ai/go",
            FailureClass::Transient { reset_at: None },
        ),
        // Antigravity 1.5.5 with an unknown model (Task 18 probe).
        (
            "antigravity",
            "Antigravity refused the model gemini-nonexistent-x: Unknown model ID: \
             gemini-nonexistent-x",
            FailureClass::ConfigError,
        ),
        // Octet's own text when Antigravity looks signed out (its `SIGN_IN`,
        // which then quotes agy; agy's own words are unconfirmed).
        (
            "antigravity",
            "Sign in to Antigravity first: run `agy`",
            FailureClass::ConfigError,
        ),
    ];

    /// 10:00:00 local time, at Unix second 1 800 000 000.
    const TEN_AM: LocalNow = LocalNow {
        epoch: 1_800_000_000,
        seconds_of_day: 10 * 3600,
    };

    #[expect(
        clippy::unnecessary_wraps,
        reason = "it stands in for the clock, which can fail"
    )]
    fn ten_am() -> Option<LocalNow> {
        Some(TEN_AM)
    }

    #[test]
    fn real_vendor_texts_are_classified() {
        for (engine, text, class) in REAL {
            assert_eq!(
                classify_at(engine, text, false, ten_am),
                *class,
                "{engine}: {text:?}"
            );
            // A known text keeps its class when the vendor then goes away.
            assert_eq!(
                classify_at(engine, text, true, ten_am),
                *class,
                "{engine}: {text:?}"
            );
        }
    }

    /// The reset Codex names in `when`, at [`TEN_AM`].
    fn codex_reset(when: &str) -> Option<u64> {
        let text =
            format!("You've hit your usage limit. … try again at {when}. (usageLimitExceeded)");
        match classify_at("codex", &text, false, ten_am) {
            FailureClass::Transient { reset_at } => reset_at,
            other => panic!("{when}: {other:?}"),
        }
    }

    #[test]
    fn codex_names_its_reset_as_a_local_clock_time() {
        let at = |seconds: u64| Some(TEN_AM.epoch + seconds);
        // Later today.
        assert_eq!(codex_reset("2:10 PM"), at(4 * 3600 + 10 * 60));
        assert_eq!(codex_reset("7:13 PM"), at(9 * 3600 + 13 * 60));
        assert_eq!(codex_reset("12:05 PM"), at(2 * 3600 + 5 * 60));
        assert_eq!(codex_reset("10:00 AM"), at(0));
        // Within the last hour: now (the reset has just passed).
        assert_eq!(codex_reset("9:04 AM"), at(0));
        assert_eq!(codex_reset("9:00 AM"), at(0));
        // Earlier in the day: tomorrow.
        assert_eq!(codex_reset("8:59 AM"), at(22 * 3600 + 59 * 60));
        assert_eq!(codex_reset("12:30 AM"), at(14 * 3600 + 30 * 60));
        // A 24-hour clock, either way round.
        assert_eq!(codex_reset("18:45"), at(8 * 3600 + 45 * 60));
        assert_eq!(codex_reset("06:00"), at(20 * 3600));
        // Anything else names no time.
        for when in [
            "13:04 PM", "0:30 AM", "24:00", "9:4 AM", "9:60 AM", "noon", "9:041 AM",
        ] {
            assert_eq!(codex_reset(when), None, "{when}");
        }
    }

    #[test]
    fn a_named_time_just_passed_is_now_not_tomorrow() {
        let text = "You've hit your usage limit. Try again at 9:04 AM. (usageLimitExceeded)";
        // Codex names the minute; its real reset is a few seconds into it.
        for (seconds_of_day, label) in [
            (9 * 3600 + 4 * 60 + 30, "9:04:30"),
            (9 * 3600 + 4 * 60, "9:04:00"),
            (9 * 3600 + 30 * 60, "9:30"),
            (10 * 3600 + 3 * 60 + 59, "10:03:59"),
        ] {
            let now = LocalNow {
                epoch: 1_800_000_000,
                seconds_of_day,
            };
            assert_eq!(
                classify_at("codex", text, false, || Some(now)),
                FailureClass::Transient {
                    reset_at: Some(1_800_000_000)
                },
                "at {label}"
            );
        }
        // More than an hour ago: tomorrow.
        let now = LocalNow {
            epoch: 1_800_000_000,
            seconds_of_day: 10 * 3600 + 5 * 60,
        };
        assert_eq!(
            classify_at("codex", text, false, || Some(now)),
            FailureClass::Transient {
                reset_at: Some(1_800_000_000 + 23 * 3600 - 60)
            }
        );
    }

    #[test]
    fn a_clock_time_without_a_clock_names_no_reset() {
        let text = "You've hit your usage limit. Try again at 9:04 AM. (usageLimitExceeded)";
        assert_eq!(
            classify_at("codex", text, false, || None),
            FailureClass::Transient { reset_at: None }
        );
    }

    #[test]
    fn the_clock_is_read_only_for_a_codex_clock_time() {
        let reads = std::cell::Cell::new(0);
        let clock = || {
            reads.set(reads.get() + 1);
            Some(TEN_AM)
        };
        for (engine, text) in [
            ("codex", "You've hit your usage limit. (usageLimitExceeded)"),
            ("claude", "Claude AI usage limit reached|1767225600"),
            ("opencode", "Rate limit exceeded; try again at 9:04 AM"),
            ("codex", "The 'x' model is not supported when using Codex."),
        ] {
            let _ = classify_at(engine, text, false, clock);
        }
        assert_eq!(reads.get(), 0);
        let _ = classify_at("codex", "usage limit; try again at 9:04 AM", false, clock);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn the_local_clock_reads() {
        let now = local_now().expect("`date` gives the local time");
        assert!(now.seconds_of_day < 86_400, "{now:?}");
        let system = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now.epoch.abs_diff(system) < 60, "{now:?} vs {system}");
        assert_eq!(
            parse_date("1800000000 09 04 59\n"),
            Some(LocalNow {
                epoch: 1_800_000_000,
                seconds_of_day: 9 * 3600 + 4 * 60 + 59
            })
        );
        assert_eq!(parse_date("1800000000 24 00 00"), None);
        assert_eq!(parse_date("soon"), None);
    }

    #[test]
    fn a_hung_clock_reads_as_none_within_its_limit() {
        let started = std::time::Instant::now();
        assert_eq!(
            run_bounded("sleep", &["5"], std::time::Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(
            run_bounded("sh", &["-c", "echo hi"], DATE_TIMEOUT).as_deref(),
            Some(&b"hi\n"[..])
        );
        assert_eq!(run_bounded("sh", &["-c", "exit 1"], DATE_TIMEOUT), None);
    }

    #[test]
    fn only_octets_own_start_failure_is_a_missing_binary() {
        assert_eq!(
            classify(
                "claude",
                "Cannot start the build: the cache is locked",
                false
            ),
            FailureClass::Unknown
        );
    }

    #[test]
    fn known_texts_are_classified() {
        for (engine, text, stopped, class) in CASES {
            assert_eq!(
                classify(engine, text, *stopped),
                *class,
                "{engine}: {text:?}"
            );
        }
    }

    #[test]
    fn a_reset_time_is_read_only_from_a_trailing_epoch() {
        assert_eq!(reset_at("limit reached|1767225600"), Some(1_767_225_600));
        assert_eq!(reset_at("limit reached|1767225600\n"), Some(1_767_225_600));
        assert_eq!(reset_at("limit reached|soon"), None);
        assert_eq!(reset_at("limit reached|"), None);
        assert_eq!(reset_at("limit reached"), None);
    }
}
