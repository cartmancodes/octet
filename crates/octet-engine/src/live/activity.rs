//! Tool calls and approval requests as the interface shows them, typed by the adapter that
//! parsed the vendor's wire format.
use super::short;
use serde_json::Value;

/// Where a tool call is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPhase {
    /// Started; no result yet.
    Running,
    /// Finished and the vendor reported no error.
    Done,
    /// Finished with an error, a non-zero exit, or a decline.
    Failed,
}
impl ToolPhase {
    /// The journal's name for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}
/// One tool call or its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// The vendor's tool-use or item ID: pairs a call's start with its end. May be empty.
    pub id: String,
    /// As the vendor names it: `Read`, `Bash`, `commandExecution`, `mcp__x__y`.
    pub name: String,
    /// One line naming its subject: a path, command, pattern or MCP tool; empty if none.
    pub target: String,
    /// Where it is.
    pub phase: ToolPhase,
    /// The preview: input, output, exit status. Bounded when emitted.
    pub detail: String,
}
/// What allowing an approval grants: Octet answers one request at a time.
pub const APPROVAL_SCOPE: &str = "This request only, once";
/// An approval request, as the dialog leads with it. The full request stays in `detail`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovalSummary {
    /// What it would do, as a sentence: "Run a shell command", "Edit 2 files".
    pub action: String,
    /// The command it would run.
    pub command: Option<String>,
    /// The files it would change.
    pub files: Vec<String>,
    /// Where it would run.
    pub cwd: Option<String>,
    /// What allowing grants.
    pub scope: String,
}
impl ApprovalSummary {
    /// A summary of `action` with nothing else known yet.
    #[must_use]
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            scope: APPROVAL_SCOPE.into(),
            ..Self::default()
        }
    }
}
/// `text`'s first line, cut to a short length.
pub(crate) fn one_line(text: &str) -> String {
    short(text.lines().next().unwrap_or_default())
}
/// One line naming what a tool `input` acts on: the first of its naming fields that is a string.
pub(crate) fn input_target(input: &Value) -> String {
    [
        "file_path",
        "notebook_path",
        "path",
        "command",
        "pattern",
        "url",
    ]
    .iter()
    .find_map(|key| input[key].as_str())
    .map(one_line)
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn the_target_is_the_first_naming_field_on_one_line() {
        assert_eq!(
            input_target(&json!({"file_path":"src/a.rs","command":"x"})),
            "src/a.rs"
        );
        assert_eq!(
            input_target(&json!({"command":"cargo test\n--all"})),
            "cargo test"
        );
        assert_eq!(input_target(&json!({"pattern":"fn main"})), "fn main");
        assert_eq!(
            input_target(&json!({"url":"https://x.test"})),
            "https://x.test"
        );
        assert_eq!(input_target(&json!({"file_path":7,"path":"b"})), "b");
        assert_eq!(input_target(&json!({})), "");
    }
    #[test]
    fn one_line_is_bounded() {
        let long = "y".repeat(1000);
        assert!(one_line(&long).len() <= 256 + '…'.len_utf8());
    }
    #[test]
    fn a_new_summary_grants_one_request() {
        let summary = ApprovalSummary::new("Run a shell command");
        assert_eq!(summary.scope, APPROVAL_SCOPE);
        assert!(summary.command.is_none() && summary.files.is_empty() && summary.cwd.is_none());
    }
}
