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
#[must_use]
pub fn classify(engine: &str, error_text: &str, stopped_mid_turn: bool) -> FailureClass {
    let text = error_text.to_lowercase();
    let row = TABLE
        .iter()
        .find(|row| row.engine.is_none_or(|only| only == engine) && text.contains(row.needle));
    match row.map(|row| row.kind) {
        Some(Kind::Transient) => FailureClass::Transient {
            reset_at: reset_at(error_text),
        },
        Some(Kind::Config) => FailureClass::ConfigError,
        Some(Kind::Local) => FailureClass::LocalFailure,
        None if stopped_mid_turn => FailureClass::Transient { reset_at: None },
        None => FailureClass::Unknown,
    }
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
        // only the clock time differed).
        (
            "codex",
            "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \
             visit https://chatgpt.com/codex/settings/usage to purchase more credits or try \
             again at 9:04 AM. (usageLimitExceeded)",
            FailureClass::Transient { reset_at: None },
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
        // Octet's own text when Antigravity looks signed out; what agy
        // itself then says is unconfirmed.
        (
            "antigravity",
            "Sign in to Antigravity first: run `agy`. Antigravity said: Authentication required",
            FailureClass::ConfigError,
        ),
    ];

    #[test]
    fn real_vendor_texts_are_classified() {
        for (engine, text, class) in REAL {
            assert_eq!(classify(engine, text, false), *class, "{engine}: {text:?}");
            // A known text keeps its class when the vendor then goes away.
            assert_eq!(classify(engine, text, true), *class, "{engine}: {text:?}");
        }
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
