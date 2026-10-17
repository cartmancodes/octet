//! `octet hub watch`: the text client over `<run>/hub.sock`, and the plain lines it prints.
use octet_core::remote::run::run_dir;
use serde_json::{Value, json};
use std::{collections::HashMap, io::ErrorKind, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// Said when no hub answers on its control socket.
pub const NOT_RUNNING: &str = "The hub isn't running; start it with: octet hub";
/// Longest text taken from one field of a frame, in characters.
const FIELD_CHARS: usize = 160;

/// `text` as one plain line for a terminal: control characters (escapes, newlines, tabs) and
/// bidirectional overrides are dropped or become a space, and it is cut at `limit` characters.
pub fn plain(text: &str, limit: usize) -> String {
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

/// The text at `value[key]`, plain; empty when it is not text.
fn field(value: &Value, key: &str) -> String {
    plain(
        value.get(key).and_then(Value::as_str).unwrap_or_default(),
        FIELD_CHARS,
    )
}

/// A session row as one line: engine, model, workspace, what it is doing, and its approvals.
fn session_line(row: &Value) -> String {
    let mut parts = vec![field(row, "engine")];
    let model = field(row, "model");
    if !model.is_empty() {
        parts.push(model);
    }
    parts.push(field(row, "workspace"));
    let doing = match field(row, "now") {
        now if now.is_empty() => field(row, "phase"),
        now => now,
    };
    parts.push(doing);
    match row.get("approvals").and_then(Value::as_u64).unwrap_or(0) {
        0 => {}
        1 => parts.push("1 approval".into()),
        n => parts.push(format!("{n} approvals")),
    }
    parts.join(" · ")
}

/// What a journal record `{type, data}` is worth a line for: a tool, an approval, a prompt or
/// the end of a turn.
fn event_line(event: &Value) -> Option<String> {
    let data = event.get("data")?;
    match event.get("type")?.as_str()? {
        "tool" => {
            let what = [field(data, "name"), field(data, "target")]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let detail = field(data, "detail");
            Some(match data.get("phase").and_then(Value::as_str) {
                Some("running") | None => format!("… {what}"),
                Some("failed") if detail.is_empty() => format!("✗ {what}"),
                Some("failed") => format!("✗ {what} ({detail})"),
                Some(_) => format!("✓ {what}"),
            })
        }
        "approval" => {
            let summary = data.get("summary").unwrap_or(&Value::Null);
            let id = data.get("id").and_then(Value::as_u64).unwrap_or(0);
            let (action, command) = (field(summary, "action"), field(summary, "command"));
            let ask = if command.is_empty() {
                action
            } else {
                format!("{action}: {command}")
            };
            Some(format!("⚠ approval {id}: {ask}"))
        }
        "user" => Some(format!("› {}", plain(data.as_str()?, FIELD_CHARS))),
        "finished" => Some(format!("Finished: {}", plain(data.as_str()?, FIELD_CHARS))),
        _ => None,
    }
}

/// Turns the control socket's frames into lines, naming each session's events by what the
/// last `sessions` frame said it was.
#[derive(Debug, Default)]
pub struct Lines {
    /// A short label for each session id.
    labels: HashMap<String, String>,
}

impl Lines {
    /// The lines for one frame: a line per session for `sessions`, one for a notable `event`.
    pub fn of(&mut self, frame: &Value) -> Vec<String> {
        match frame.get("type").and_then(Value::as_str) {
            Some("sessions") => {
                let rows = frame
                    .get("sessions")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                self.labels = rows.iter().filter_map(label).collect();
                if rows.is_empty() {
                    return vec!["No sessions".into()];
                }
                rows.iter().map(session_line).collect()
            }
            Some("event") => {
                let Some(line) = frame.get("event").and_then(event_line) else {
                    return Vec::new();
                };
                let id = frame
                    .get("session")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.labels.get(id) {
                    Some(label) => vec![format!("{label} · {line}")],
                    None => vec![line],
                }
            }
            _ => Vec::new(),
        }
    }
}

/// `(id, label)` for a session row: its engine and the last part of its workspace.
fn label(row: &Value) -> Option<(String, String)> {
    let id = row.get("id")?.as_str()?.to_owned();
    let workspace = field(row, "workspace");
    let place = workspace
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("/");
    Some((id, format!("{}:{place}", field(row, "engine"))))
}

/// Connects to the hub in `directory`, asks to watch, and calls `print` with each line until the
/// hub goes away or `print` returns false.
///
/// # Errors
///
/// Fails with [`NOT_RUNNING`] when no hub answers, or with why the stream ended.
pub async fn watch(directory: &Path, mut print: impl FnMut(&str) -> bool) -> Result<(), String> {
    let path = run_dir(directory).join("hub.sock");
    let stream = UnixStream::connect(&path)
        .await
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound | ErrorKind::ConnectionRefused => NOT_RUNNING.to_owned(),
            _ => format!("Couldn't reach the hub at {}: {e}", path.display()),
        })?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{}\n", json!({"type": "watch"})).as_bytes())
        .await
        .map_err(|e| format!("Couldn't ask the hub to watch: {e}"))?;
    let mut read = BufReader::new(read);
    let mut lines = Lines::default();
    let mut line = String::new();
    loop {
        line.clear();
        let read = read
            .read_line(&mut line)
            .await
            .map_err(|e| format!("The hub's stream failed: {e}"))?;
        if read == 0 {
            return Err("The hub stopped".into());
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(error) = frame.get("error").and_then(Value::as_str) {
            return Err(plain(error, FIELD_CHARS));
        }
        for text in lines.of(&frame) {
            if !print(&text) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: &Value) -> Vec<String> {
        Lines::default().of(value)
    }

    #[test]
    fn watch_lines_are_short_and_plain() {
        let sessions = json!({"v": 1, "type": "sessions", "sessions": [
            {"id": "1-2", "engine": "claude", "model": "gpt-5.5", "workspace": "/w",
             "phase": "running", "now": "Running Bash cargo test", "approvals": 1},
            {"id": "3-4", "engine": "codex", "workspace": "/x/y", "phase": "idle", "approvals": 0},
        ]});
        assert_eq!(
            frame(&sessions),
            [
                "claude · gpt-5.5 · /w · Running Bash cargo test · 1 approval",
                "codex · /x/y · idle",
            ]
        );
        let tool = |phase: &str, detail: &str| {
            json!({"type": "event", "session": "9", "seq": 1, "event": {"type": "tool", "data":
                {"id": "t1", "name": "Bash", "target": "cargo test", "phase": phase,
                 "detail": detail}}})
        };
        assert_eq!(frame(&tool("running", "")), ["… Bash cargo test"]);
        assert_eq!(frame(&tool("completed", "")), ["✓ Bash cargo test"]);
        assert_eq!(
            frame(&tool("failed", "exit 2")),
            ["✗ Bash cargo test (exit 2)"]
        );
        let approval = json!({"type": "event", "session": "9", "event": {"type": "approval",
            "data": {"id": 3, "detail": "", "summary": {"action": "Run a shell command",
            "command": "cargo test", "files": [], "cwd": null, "scope": "once"}}}});
        assert_eq!(
            frame(&approval),
            ["⚠ approval 3: Run a shell command: cargo test"]
        );
        let text = json!({"type": "event", "session": "9", "event": {"type": "text", "data": "x"}});
        assert!(frame(&text).is_empty());
        assert_eq!(
            frame(&json!({"type": "sessions", "sessions": []})),
            ["No sessions"]
        );
    }

    #[test]
    fn events_are_named_by_the_session_the_rows_described() {
        let mut lines = Lines::default();
        lines.of(&json!({"type": "sessions", "sessions": [
            {"id": "1-2", "engine": "claude", "workspace": "/home/me/app/", "phase": "idle"}]}));
        let user = json!({"type": "event", "session": "1-2", "event": {"type": "user",
            "data": "fix it"}});
        assert_eq!(lines.of(&user), ["claude:app · › fix it"]);
    }

    #[test]
    fn terminal_escapes_and_newlines_never_reach_the_screen() {
        assert_eq!(plain("a\u{1b}[31mb\nc\td\u{7}\u{9b}e", 100), "a[31mb c de");
        assert_eq!(plain("\u{202e}evil", 100), "evil");
        assert_eq!(plain("abcdef", 3), "abc…");
        let hostile = json!({"type": "sessions", "sessions": [
            {"id": "1", "engine": "cl\u{1b}]0;x\u{7}aude", "workspace": "/w\n/evil",
             "phase": "idle", "model": "m\u{1b}[2J"}]});
        let out = frame(&hostile).join("\n");
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
        assert_eq!(out.lines().count(), 1, "{out:?}");
        let tool = json!({"type": "event", "session": "1", "event": {"type": "tool", "data":
            {"name": "Ba\u{1b}sh", "target": "ls\r\nrm", "phase": "running", "detail": ""}}});
        assert_eq!(frame(&tool), ["… Bash ls rm"]);
    }

    #[tokio::test]
    async fn a_missing_hub_is_not_running() {
        let directory = std::env::temp_dir().join(format!("octet-watch-{}", std::process::id()));
        assert_eq!(
            watch(&directory, |_| true).await,
            Err(NOT_RUNNING.to_owned())
        );
    }
}
