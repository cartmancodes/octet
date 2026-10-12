# TUI clarity: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** approvals lead with what they would do; routine tool calls collapse and failures stay
open; the composer says one thing about Enter; the sidebar shows the task; skills never run by
accident; short terminals keep the transcript; scrollback stays still and says output is arriving.

**Architecture:**

- **Engine (`octet-engine`):** `Event::Tool` carries a typed `ToolCall` and `Event::Approval` an
  `ApprovalSummary`, both built by the adapter that parses the vendor's wire format
  (`live/activity.rs` holds the types and shared helpers). Claude's `tool_result` blocks, ignored
  today, end its calls.
- **`octet-core` / `octet`:** the journal writes the new fields; headless names the action.
- **TUI (`octet-tui`):** a tools entry holds the calls of one run and draws them collapsed or
  expanded; the dialog, composer, sidebar, palette and layout change in `view.rs`; keys in
  `input.rs`; the skill rule in `commands.rs`.

**Tech stack:** Rust 1.98.1, edition 2024, tokio, serde_json, ratatui. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-12-tui-clarity-design.md`.

## Global constraints

- **The gate:** cargo always runs through `scripts/rust-env.sh`. The gate is `make rust-check`; it
  must pass after every task. Commit only inside `if make rust-check; then git commit …; fi`, never
  after `; echo`.
- **Focused runs while iterating:** `scripts/rust-env.sh cargo test -p <crate> <filter>`.
- **TDD:** each new test is watched failing before its implementation.
- **Lints:** exceptions are `#[expect(lint, reason = "…")]`, never a bare `#[allow]`. Product lines
  are at most 120 columns. `rustfmt` decides layout.
- **Comments:** match the surrounding density: a doc line on each new item, saying what it is for.
- **Commits:** stage by path; each message ends with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Branch `feat/tui-clarity`.
- **Sanitising:** every vendor string drawn by the TUI goes through `crate::text::clean` first.
- **Colours:** only `ACCENT`, `AMBER`, `MUTED`, `FG`, `EDGE`, `PANEL`, `BG`, `SELECTED` from
  `view.rs`. Amber means warnings and permission decisions.
- **Minimum size:** 38 × 12 still draws everything (tests at 40 × 12 and 38 × 12 where noted).
- **Rulings recorded up front** (behaviour the spec leaves open, decided here):
  - A failed call's header is `✗ {label} {target}`; its exit status is in its detail, not parsed
    out into the header.
  - The approval scope string is `This request only, once`; the dialog prints
    `Allows: This request only, once`. Paths are printed as sent (no `~`).
  - A call with an empty `id` never pairs with another (no synthetic IDs needed).
  - Summary words: `Read` → "Read N files"; `Edit`, `MultiEdit`, `NotebookEdit`, `fileChange` →
    "Edited N files"; `Write` → "Wrote N files"; `Bash`, `commandExecution` → "Ran N commands";
    `Grep`, `Glob` → "Searched N times"; anything else → "{name} ×N". Singular when N is 1
    ("Read 1 file", "Ran 1 command", "Searched once").
  - Codex file-change approvals carry only an `itemId` (checked against the Codex 0.154 app-server
    schema): the adapter remembers each `fileChange` item's paths from `item/started` to name them.
- **Real vendors:** tiny prompts, a throwaway `--cwd`; Codex with `--model gpt-5.5`. Never edit
  the user's Claude or Codex configuration.

## Review focus

1. **A tool result whose call was never seen** (Claude `tool_result` for an ID it did not announce,
   or a call trimmed from the transcript): it must appear as its own call, never panic or attach to
   the wrong call (Task 3 test `an_unmatched_end_is_its_own_call`).
2. **A turn cancelled while calls run:** the running lines must say `· stopped`, and the sidebar
   must not keep saying "Running …" (Task 3 test `cancelled_calls_say_stopped`, Task 9 test
   `now_ignores_stopped_calls`).
3. **A typed `/name` that is an Octet command and also a skill on the other provider** (`/model`):
   Octet's command runs, no switch, no amber hint (Task 6 test
   `an_octet_command_never_switches_provider`).
4. **Scrolled up while a long reply streams and the terminal is resized:** the view must re-anchor,
   not jump by a miscount or panic on a trimmed anchor entry (Task 11 test
   `resizing_while_scrolled_reanchors`).
5. **An approval for a huge command or a hundred files on a 40 × 12 screen:** the summary must be
   bounded (3 command lines, 5 files) and the keys row must stay visible (Task 5 test
   `a_huge_approval_keeps_its_keys_on_a_small_screen`).

---

### Task 1: Typed events, the journal, headless, and Claude's tool results

The interface change lands whole: types, every producer and consumer, so the workspace compiles.
The TUI only adapts here (it still shows a tool call as one text entry); Tasks 3–5 use the types.

**Files:**

- Create: `crates/octet-engine/src/live/activity.rs`
- Modify: `crates/octet-engine/src/live/mod.rs` (module, re-exports, `Event`, `emit`)
- Modify: `crates/octet-engine/src/live/claude.rs` (`tool_use`, `"user"` frames, approvals, state)
- Modify: `crates/octet-engine/src/live/codex.rs` (compile only: wrap today's text; Task 2 types it)
- Modify: `crates/octet-engine/src/live/demo.rs` (demo approval summary)
- Modify: `crates/octet-engine/src/live/protocol.rs` (`queue_approval` takes the summary)
- Modify: `crates/octet-core/src/lib.rs` (re-exports, `record`)
- Modify: `crates/octet/src/headless.rs:108-118`
- Modify: `crates/octet-tui/src/app.rs:752-763`, `crates/octet-tui/src/mascot.rs` (`State::tool`)
- Modify: `crates/octet-testkit/src/bin/protocol-child/claude.rs`, `crates/octet-testkit/src/scenario.rs`
- Test: `crates/octet-engine/src/live/activity.rs` (unit), `crates/octet-engine/src/live/claude.rs`
  (unit), `crates/octet-engine/tests/live.rs`, `crates/octet-core/src/lib.rs` (unit),
  `crates/octet/tests/headless.rs`, and every TUI test that builds `Event::Tool`/`Event::Approval`.

**Interfaces:**

- Produces (re-exported from `octet_core`):

```rust
pub enum ToolPhase { Running, Done, Failed }            // ToolPhase::as_str() -> "running" | "done" | "failed"
pub struct ToolCall { pub id: String, pub name: String, pub target: String, pub phase: ToolPhase, pub detail: String }
pub struct ApprovalSummary { pub action: String, pub command: Option<String>, pub files: Vec<String>,
                             pub cwd: Option<String>, pub scope: String }
impl ApprovalSummary { pub fn new(action: impl Into<String>) -> Self }  // scope = APPROVAL_SCOPE
pub const APPROVAL_SCOPE: &str = "This request only, once";
// Event::Tool(ToolCall)
// Event::Approval { id: u64, detail: String, summary: ApprovalSummary }
```

- Produces (crate-private in `octet-engine`): `activity::input_target(&Value) -> String`,
  `activity::one_line(&str) -> String`.
- Produces (TUI test helper): `crate::test_support::tool(id, name, target, phase) -> octet_core::Event`
  and `crate::test_support::approval(id, detail) -> octet_core::Event`.

- [ ] **Step 1: Write the failing unit tests for the shared helpers**

In the new `crates/octet-engine/src/live/activity.rs`, start with the tests only (the module is
declared in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn the_target_is_the_first_naming_field_on_one_line() {
        assert_eq!(input_target(&json!({"file_path":"src/a.rs","command":"x"})), "src/a.rs");
        assert_eq!(input_target(&json!({"command":"cargo test\n--all"})), "cargo test");
        assert_eq!(input_target(&json!({"pattern":"fn main"})), "fn main");
        assert_eq!(input_target(&json!({"url":"https://x.test"})), "https://x.test");
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
```

- [ ] **Step 2: Write the failing Claude adapter tests**

Append to the `tests` module in `crates/octet-engine/src/live/claude.rs`:

```rust
#[test]
fn a_bash_request_is_summarised_with_its_command_and_folder() {
    let request = json!({"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cargo test"}});
    let summary = claude_approval_summary(&request, Path::new("/work"));
    assert_eq!(summary.action, "Run a shell command");
    assert_eq!(summary.command.as_deref(), Some("cargo test"));
    assert_eq!(summary.cwd.as_deref(), Some("/work"));
    assert!(summary.files.is_empty());
}
#[test]
fn edit_write_and_mcp_requests_say_what_they_touch() {
    let edit = json!({"tool_name":"Edit","input":{"file_path":"src/a.rs"}});
    let summary = claude_approval_summary(&edit, Path::new("/w"));
    assert_eq!((summary.action.as_str(), summary.files.as_slice()), ("Edit 1 file", &["src/a.rs".to_owned()][..]));
    let write = json!({"tool_name":"Write","input":{"file_path":"new.txt"}});
    assert_eq!(claude_approval_summary(&write, Path::new("/w")).action, "Write 1 file");
    let mcp = json!({"tool_name":"mcp__fixture__fixture_echo","input":{}});
    assert_eq!(claude_approval_summary(&mcp, Path::new("/w")).action, "Use MCP tool fixture_echo");
    let other = json!({"tool_name":"WebFetch","input":{"url":"https://x.test"}});
    assert_eq!(claude_approval_summary(&other, Path::new("/w")).action, "Use WebFetch");
}
#[test]
fn tool_result_text_joins_its_parts() {
    assert_eq!(result_text(&json!("plain")), "plain");
    assert_eq!(result_text(&json!([{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}])), "a\nb");
    assert_eq!(result_text(&json!(null)), "");
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `scripts/rust-env.sh cargo test -p octet-engine --lib activity claude`
Expected: compile errors: `input_target`, `ApprovalSummary`, `claude_approval_summary`,
`result_text` not found.

- [ ] **Step 4: Add the types and helpers**

Top of `crates/octet-engine/src/live/activity.rs`:

```rust
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
    ["file_path", "notebook_path", "path", "command", "pattern", "url"]
        .iter()
        .find_map(|key| input[key].as_str())
        .map(one_line)
        .unwrap_or_default()
}
```

In `crates/octet-engine/src/live/mod.rs`: add `mod activity;` beside the other modules and
`pub use activity::{APPROVAL_SCOPE, ApprovalSummary, ToolCall, ToolPhase};`. Change the enum:

```rust
    /// A tool call started or ended.
    Tool(ToolCall),
    /// The vendor asks to run something and waits for the user.
    Approval {
        /// Octet's ID for answering it.
        id: u64,
        /// The full request, as shown to the user.
        detail: String,
        /// What it would do, for the dialog's first lines.
        summary: ApprovalSummary,
    },
```

and in `emit` bound the call's detail instead of a string:

```rust
        // Tool detail is a preview: megabytes of command output must not flood
        // the queue, so it is cut to one event.
        Event::Tool(mut call) => {
            call.detail = limited(&call.detail);
            return tx
                .try_send(Event::Tool(call))
                .map_err(|_| DriverError::ConsumerOverloaded);
        }
```

- [ ] **Step 5: Claude: summaries, `tool_use` as `Running`, `tool_result` as `Done`/`Failed`**

In `claude.rs` import `ApprovalSummary, ToolCall, ToolPhase` and
`activity::{input_target, one_line}` (add `activity` to the `super::{…}` list), and
`std::collections::HashMap`. Add to `ClaudeProtocol` (and its constructor, as `HashMap::new()`):

```rust
    /// Tool calls announced and not yet ended this turn: their names and targets, for the results.
    tools: HashMap<String, (String, String)>,
```

Free functions near `claude_stray_reply`:

```rust
/// What a `can_use_tool` request would do, for the dialog. Claude runs tools in the session's
/// folder, `cwd`.
pub(super) fn claude_approval_summary(request: &Value, cwd: &Path) -> ApprovalSummary {
    let tool = request["tool_name"].as_str().unwrap_or("a tool");
    let input = &request["input"];
    let mut summary = ApprovalSummary::new(match tool {
        "Bash" => "Run a shell command".to_owned(),
        "Edit" | "MultiEdit" | "NotebookEdit" => "Edit 1 file".to_owned(),
        "Write" => "Write 1 file".to_owned(),
        _ if tool.starts_with("mcp__") => {
            format!("Use MCP tool {}", tool.rsplit("__").next().unwrap_or(tool))
        }
        _ => format!("Use {}", one_line(tool)),
    });
    summary.command = input["command"].as_str().map(str::to_owned);
    summary.files = ["file_path", "notebook_path"]
        .iter()
        .filter_map(|key| input[key].as_str())
        .map(str::to_owned)
        .take(1)
        .collect();
    summary.cwd = Some(cwd.display().to_string());
    summary
}
/// A `tool_result` block's content as text: a string, or the text of its parts.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
```

At the approval site (`claude.rs:340-343`):

```rust
                let detail = serde_json::to_string_pretty(&v["request"]).unwrap_or_default();
                let summary = claude_approval_summary(&v["request"], &core.config.cwd);
                return core.queue_approval(v, detail, summary).await;
```

At the `tool_use` site (`claude.rs:384-387`):

```rust
                    if block["type"] == "tool_use" {
                        let name = one_line(block["name"].as_str().unwrap_or("tool"));
                        let id = one_line(block["id"].as_str().unwrap_or_default());
                        let target = input_target(&block["input"]);
                        if !id.is_empty() && self.tools.len() < 256 {
                            self.tools.insert(id.clone(), (name.clone(), target.clone()));
                        }
                        core.emit(Event::Tool(ToolCall {
                            id,
                            name: name.clone(),
                            target,
                            phase: ToolPhase::Running,
                            detail: format!("{name}\n{}", block["input"]),
                        }))?;
                    }
```

New arms in the frame `match kind` (before `_ => {}`), and the turn's end clears the map:

```rust
            "user" => self.tool_results(core, &v)?,
            "result" => {
                self.tools.clear();
                Self::result(core, &v)?;
            }
```

with, in `impl ClaudeProtocol`:

```rust
    /// The `tool_result` blocks of a `"user"` frame: each ends its call.
    fn tool_results(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        for block in v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_result")
        {
            let id = one_line(block["tool_use_id"].as_str().unwrap_or_default());
            let (name, target) = self.tools.remove(&id).unwrap_or_else(|| ("tool".into(), String::new()));
            let phase = if block["is_error"].as_bool() == Some(true) {
                ToolPhase::Failed
            } else {
                ToolPhase::Done
            };
            core.emit(Event::Tool(ToolCall {
                id,
                name,
                target,
                phase,
                detail: result_text(&block["content"]),
            }))?;
        }
        Ok(())
    }
```

`queue_approval` in `protocol.rs` gains a `summary: ApprovalSummary` parameter and passes it into
`Event::Approval { id, detail, summary }`. Codex's call site (`codex.rs:530-531`) passes
`ApprovalSummary::new("Approve a Codex request")` for now (Task 2 replaces it). Codex's tool site
(`codex.rs:263`) becomes, for now:

```rust
                        core.emit(Event::Tool(ToolCall {
                            id: one_line(item["id"].as_str().unwrap_or_default()),
                            name: one_line(item["type"].as_str().unwrap_or("tool")),
                            target: String::new(),
                            phase: if method == "item/started" { ToolPhase::Running } else { ToolPhase::Done },
                            detail: codex_tool_detail(item, phase),
                        }))?;
```

The demo (`demo.rs:246`) sends
`summary: ApprovalSummary { files: vec!["hello.txt".into()], ..ApprovalSummary::new("Write 1 file (demo: nothing runs)") }`.

- [ ] **Step 6: Run the engine unit tests**

Run: `scripts/rust-env.sh cargo test -p octet-engine --lib`
Expected: PASS, including the six new tests.

- [ ] **Step 7: Fake Claude ends its tool, and a failing tool script; failing live test**

In `scenario.rs` add:

```rust
/// Claude: one tool use whose result is an error.
pub const TOOL_FAIL: &str = "tool-fail";
```

In `protocol-child/claude.rs`, the `TOOL` script emits, after the `assistant` frame:

```rust
                emit(
                    &json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"fixture\n","is_error":false}]}}),
                );
```

and a new arm:

```rust
            scenario::TOOL_FAIL => {
                emit(&claude_init(sid));
                emit(
                    &json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"Bash","input":{"command":"false"}}]}}),
                );
                emit(
                    &json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_9","content":[{"type":"text","text":"exit 1"}],"is_error":true}]}}),
                );
                emit(&claude_result(sid, ""));
            }
```

Replace `claude_tool_is_announced_once_with_its_input` in `crates/octet-engine/tests/live.rs`:

```rust
async fn claude_tool_calls(prompt: &str) -> Vec<ToolCall> {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(prompt.into())).unwrap();
    let mut tools = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Tool(call) => tools.push(call),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    stop(&handle, task).await;
    tools
}
#[tokio::test]
async fn claude_tool_starts_with_its_target_and_ends_with_its_result() {
    let tools = claude_tool_calls(scenario::TOOL).await;
    assert_eq!(tools.len(), 2, "{tools:?}");
    assert_eq!((tools[0].phase, tools[0].name.as_str(), tools[0].target.as_str()), (ToolPhase::Running, "Bash", "echo fixture"));
    assert_eq!((tools[1].phase, tools[1].id.as_str(), tools[1].name.as_str()), (ToolPhase::Done, "toolu_1", "Bash"));
    assert_eq!(tools[1].target, "echo fixture");
    assert_eq!(tools[1].detail, "fixture\n");
}
#[tokio::test]
async fn claude_tool_error_is_a_failed_call() {
    let tools = claude_tool_calls(scenario::TOOL_FAIL).await;
    assert_eq!(tools.last().map(|t| (t.phase, t.detail.as_str())), Some((ToolPhase::Failed, "exit 1")));
}
```

Update the `bigtool` test (`live.rs:782`): `Event::Tool(call) => { tool_bytes += call.detail.len(); tool = call.detail; }`.
Import `ToolCall, ToolPhase` from `octet_engine::live`.

Run: `scripts/rust-env.sh cargo test -p octet-engine --test live claude_tool`
Expected: PASS (the fixture now sends results; the adapter emits them).

- [ ] **Step 8: Journal and headless; failing tests first**

In `crates/octet-core/src/lib.rs` tests (or add a `#[cfg(test)] mod tests` at the end), test:

```rust
#[test]
fn a_tool_call_and_an_approval_journal_their_fields() {
    let call = Event::Tool(ToolCall { id: "t1".into(), name: "Bash".into(), target: "ls".into(),
        phase: ToolPhase::Failed, detail: "exit 2".into() });
    assert_eq!(event_json(&call)["data"], json!({"id":"t1","name":"Bash","target":"ls","phase":"failed","detail":"exit 2"}));
    let summary = ApprovalSummary { command: Some("ls".into()), ..ApprovalSummary::new("Run a shell command") };
    let approval = Event::Approval { id: 3, detail: "{}".into(), summary };
    let data = &event_json(&approval)["data"];
    assert_eq!(data["summary"]["action"], "Run a shell command");
    assert_eq!(data["summary"]["command"], "ls");
    assert_eq!(data["summary"]["scope"], APPROVAL_SCOPE);
}
```

(First check how `event_json` nests `record`'s value — read `event_json` at `lib.rs:94`; use its
real key in place of `"data"` if it differs.)

Run: `scripts/rust-env.sh cargo test -p octet-core a_tool_call_and_an_approval`
Expected: FAIL (does not compile until `record` handles the new shapes).

Then add `ApprovalSummary, APPROVAL_SCOPE, ToolCall, ToolPhase` to the `pub use` list at the top of
`lib.rs`, and in `record`:

```rust
        Event::Tool(call) => (
            "tool",
            json!({"id":call.id,"name":call.name,"target":call.target,"phase":call.phase.as_str(),"detail":call.detail}),
        ),
        Event::Approval { id, detail, summary } => (
            "approval",
            json!({"id":id,"detail":detail,"summary":{
                "action":summary.action,"command":summary.command,"files":summary.files,
                "cwd":summary.cwd,"scope":summary.scope}}),
        ),
```

In `crates/octet/src/headless.rs:108-118`:

```rust
            Event::Approval { id, summary, .. } => {
                let what = match &summary.command {
                    Some(command) => format!("{}: {command}", summary.action),
                    None => summary.action.clone(),
                };
                warn(&format!(
                    "denied an approval ({what}); print mode cannot ask. \
                     Use --mode auto or --mode full-access for unattended runs."
                ));
```

Add a headless test in `crates/octet/tests/headless.rs` beside the existing approval-denial test
(find it with `grep -n "denied an approval" crates/octet/tests/headless.rs`): its stderr now
contains `denied an approval (Run a shell command: echo fixture)` for the Claude `approval` script
(and `Approve a Codex request` for Codex until Task 2 changes it; assert Claude here).

Run: `scripts/rust-env.sh cargo test -p octet-core && scripts/rust-env.sh cargo test -p octet --test headless`
Expected: PASS.

- [ ] **Step 9: The TUI adapts (no new behaviour yet)**

`crates/octet-tui/src/mascot.rs`: `State::tool` takes the call:

```rust
    pub(crate) fn tool(call: &octet_core::ToolCall) -> Self {
        // The tool's name and command, not arbitrary tool output.
        let name = call.name.to_ascii_lowercase();
        let command = call.target.trim();
        if ["dispatch", "delegate", "spawn_agent", "task", "collabagent"]
            .iter()
            .any(|word| name.contains(word))
        {
            Self::Delegating
        } else if ["search", "read", "glob", "grep"].iter().any(|word| name.contains(word))
            || ["rg ", "grep ", "find ", "cat ", "ls"].iter().any(|prefix| command.starts_with(prefix))
        {
            Self::Searching
        } else {
            Self::Coding
        }
    }
```

`crates/octet-tui/src/app.rs:752-763`:

```rust
            Event::Tool(call) => {
                self.chat.reply_break = true;
                self.conn.activity = State::tool(&call);
                self.add(Role::Tool, &format!("{}\n{}", call.name, call.detail));
            }
            Event::Approval { id, detail, .. } => {
```

(the rest of the approval arm is unchanged until Task 5).

`crates/octet-tui/src/test_support.rs` gains:

```rust
/// A tool call event, as an adapter sends it.
pub(crate) fn tool(id: &str, name: &str, target: &str, phase: octet_core::ToolPhase) -> octet_core::Event {
    octet_core::Event::Tool(octet_core::ToolCall {
        id: id.into(),
        name: name.into(),
        target: target.into(),
        phase,
        detail: format!("{name} {target}"),
    })
}
/// An approval event with a shell-command summary.
pub(crate) fn approval(id: u64, detail: &str) -> octet_core::Event {
    octet_core::Event::Approval {
        id,
        detail: detail.into(),
        summary: octet_core::ApprovalSummary {
            command: Some("cargo test".into()),
            cwd: Some("/tmp".into()),
            ..octet_core::ApprovalSummary::new("Run a shell command")
        },
    }
}
```

Replace every `Event::Tool("…".into())` and `Event::Approval { id, detail }` in the TUI tests
(`app/tests.rs:21`, `view/tests.rs:198,281,285,288,290,331,543,577`, `tests.rs:138`,
`tests/slash.rs:682`, `tests/keys.rs:488`) with these helpers. In `view/tests.rs:281-288` the
mascot cases become `tool("1","Read","src/main.rs",Running)` → `SEARCHING`,
`tool("2","Bash","cargo check",Running)` → `CODING`, `tool("3","dispatch_subagent","",Running)` →
`DELEGATING`.

- [ ] **Step 10: The gate, then commit**

Run: `make rust-check`
Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-engine/src/live/activity.rs crates/octet-engine/src/live/mod.rs \
    crates/octet-engine/src/live/claude.rs crates/octet-engine/src/live/codex.rs \
    crates/octet-engine/src/live/demo.rs crates/octet-engine/src/live/protocol.rs \
    crates/octet-engine/tests/live.rs crates/octet-core/src/lib.rs crates/octet/src/headless.rs \
    crates/octet/tests/headless.rs crates/octet-testkit crates/octet-tui/src
  git commit -m "Events: a tool call and an approval summary are typed; Claude's tool results end its calls

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 2: Codex: typed calls, failures and approval summaries

**Files:**

- Modify: `crates/octet-engine/src/live/codex.rs` (`CodexProtocol`, item handling, `server_request`, new helpers)
- Modify: `crates/octet-testkit/src/scenario.rs`, `crates/octet-testkit/src/bin/protocol-child/codex.rs`
- Test: `crates/octet-engine/src/live/codex.rs` (unit), `crates/octet-engine/tests/live.rs`

**Interfaces:**

- Consumes: `ToolCall`, `ToolPhase`, `ApprovalSummary`, `activity::one_line` (Task 1).
- Produces: `codex_tool_call(item: &Value, started: bool) -> ToolCall`;
  `codex_approval_summary(method: &str, params: &Value, files: &[String]) -> ApprovalSummary`;
  scenarios `TOOL_FAIL` (shared name with Claude) and `FILE_APPROVAL`.

- [ ] **Step 1: Failing unit tests**

Append to `codex.rs` tests:

```rust
#[test]
fn an_item_is_a_call_that_fails_on_status_or_exit() {
    let started = json!({"id":"c1","type":"commandExecution","command":"cargo test","status":"inProgress"});
    let call = codex_tool_call(&started, true);
    assert_eq!((call.id.as_str(), call.name.as_str(), call.target.as_str(), call.phase),
        ("c1", "commandExecution", "cargo test", ToolPhase::Running));
    let ok = json!({"id":"c1","type":"commandExecution","command":"x","status":"completed","exitCode":0});
    assert_eq!(codex_tool_call(&ok, false).phase, ToolPhase::Done);
    for failed in [
        json!({"id":"c","type":"commandExecution","status":"completed","exitCode":101}),
        json!({"id":"c","type":"commandExecution","status":"failed"}),
        json!({"id":"c","type":"fileChange","status":"declined","changes":[]}),
        json!({"id":"c","type":"mcpToolCall","status":"failed","tool":"echo"}),
    ] {
        assert_eq!(codex_tool_call(&failed, false).phase, ToolPhase::Failed, "{failed}");
    }
    let files = json!({"id":"f","type":"fileChange","status":"completed","changes":[{"path":"a.rs","kind":"update","diff":""}]});
    assert_eq!(codex_tool_call(&files, false).target, "a.rs");
    let mcp = json!({"id":"m","type":"mcpToolCall","status":"completed","tool":"echo","server":"fx"});
    assert_eq!(codex_tool_call(&mcp, false).target, "echo");
}
#[test]
fn approvals_name_the_command_folder_or_files() {
    let params = json!({"command":"cargo test","cwd":"/w","itemId":"c1"});
    let summary = codex_approval_summary("item/commandExecution/requestApproval", &params, &[]);
    assert_eq!(summary.action, "Run a shell command");
    assert_eq!((summary.command.as_deref(), summary.cwd.as_deref()), (Some("cargo test"), Some("/w")));
    let files = ["a.rs".to_owned(), "b.rs".to_owned()];
    let summary = codex_approval_summary("item/fileChange/requestApproval", &json!({"itemId":"f"}), &files);
    assert_eq!((summary.action.as_str(), summary.files.len()), ("Edit 2 files", 2));
    assert_eq!(codex_approval_summary("item/fileChange/requestApproval", &json!({}), &[]).action, "Change files");
}
```

Run: `scripts/rust-env.sh cargo test -p octet-engine --lib codex`
Expected: FAIL to compile (`codex_tool_call`, `codex_approval_summary` missing).

- [ ] **Step 2: Implement**

```rust
/// A Codex tool item as a call: running when `started`, else done or failed by its status
/// and exit code.
pub(super) fn codex_tool_call(item: &Value, started: bool) -> ToolCall {
    let kind = item["type"].as_str().unwrap_or("tool");
    let failed = matches!(item["status"].as_str(), Some("failed" | "declined"))
        || item["exitCode"].as_i64().is_some_and(|code| code != 0);
    let phase = match (started, failed) {
        (true, _) => ToolPhase::Running,
        (false, true) => ToolPhase::Failed,
        (false, false) => ToolPhase::Done,
    };
    let target = match kind {
        "commandExecution" => item["command"].as_str(),
        "fileChange" => item.pointer("/changes/0/path").and_then(Value::as_str),
        "mcpToolCall" => item["tool"].as_str(),
        _ => None,
    };
    ToolCall {
        id: one_line(item["id"].as_str().unwrap_or_default()),
        name: one_line(kind),
        target: target.map(one_line).unwrap_or_default(),
        phase,
        detail: codex_tool_detail(item, if started { "running" } else { "finished" }),
    }
}
/// The paths a `fileChange` item would change.
fn change_paths(item: &Value) -> Vec<String> {
    item["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|change| change["path"].as_str())
        .map(str::to_owned)
        .collect()
}
/// What a Codex approval request would do. A file-change request names only its item, so its
/// `files` come from that item's start.
pub(super) fn codex_approval_summary(method: &str, params: &Value, files: &[String]) -> ApprovalSummary {
    if method == "item/fileChange/requestApproval" {
        let mut summary = ApprovalSummary::new(match files.len() {
            0 => "Change files".to_owned(),
            1 => "Edit 1 file".to_owned(),
            n => format!("Edit {n} files"),
        });
        summary.files = files.to_vec();
        return summary;
    }
    let mut summary = ApprovalSummary::new("Run a shell command");
    summary.command = params["command"].as_str().map(str::to_owned);
    summary.cwd = params["cwd"].as_str().map(str::to_owned);
    summary
}
```

Add to `CodexProtocol` (and its constructor):

```rust
    /// Each `fileChange` item's paths from its start, for its approval; cleared as the turn ends.
    file_changes: HashMap<String, Vec<String>>,
```

In the `item/started | item/completed` arm:

```rust
                    "commandExecution" | "fileChange" | "mcpToolCall" => {
                        let started = method == "item/started";
                        if started && item["type"] == "fileChange" && self.file_changes.len() < 64 {
                            let id = one_line(item["id"].as_str().unwrap_or_default());
                            self.file_changes.insert(id, change_paths(item));
                        }
                        core.emit(Event::Tool(codex_tool_call(item, started)))?;
                    }
```

In `turn/completed`, after the turn-ID check: `self.file_changes.clear();`. In `server_request`:

```rust
            let detail = serde_json::to_string_pretty(&v["params"]).unwrap_or_default();
            let item = v.pointer("/params/itemId").and_then(Value::as_str).unwrap_or_default();
            let files = self.file_changes.get(item).map_or(&[][..], Vec::as_slice);
            let summary = codex_approval_summary(method, &v["params"], files);
            return core.queue_approval(v, detail, summary).await;
```

Remove Task 1's interim `ToolCall { … }` literal and `"Approve a Codex request"`.

Run: `scripts/rust-env.sh cargo test -p octet-engine --lib codex`
Expected: PASS.

- [ ] **Step 3: Fake Codex scripts and live tests**

`scenario.rs`: document `TOOL_FAIL` as "Codex and Claude: one tool whose result is an error" and add

```rust
/// Codex: a file change starts, then asks for approval.
pub const FILE_APPROVAL: &str = "file-approval";
```

`protocol-child/codex.rs`, new arms:

```rust
            scenario::TOOL_FAIL => {
                emit(&json!({"method":"item/started","params":{"threadId":"fixture-thread","turnId":active,"item":{"id":"cmd-9","type":"commandExecution","command":"false","status":"inProgress"}}}));
                emit(&json!({"method":"item/completed","params":{"threadId":"fixture-thread","turnId":active,"item":{"id":"cmd-9","type":"commandExecution","command":"false","status":"failed","exitCode":1,"aggregatedOutput":"boom"}}}));
                done();
            }
            scenario::FILE_APPROVAL => {
                emit(&json!({"method":"item/started","params":{"threadId":"fixture-thread","turnId":active,"item":{"id":"fc-1","type":"fileChange","status":"inProgress","changes":[{"path":"a.rs","kind":"update","diff":""},{"path":"b.rs","kind":"add","diff":""}]}}}));
                emit(&json!({"id":"permission","method":"item/fileChange/requestApproval","params":{"threadId":"fixture-thread","turnId":active,"itemId":"fc-1"}}));
            }
```

(Read how the `APPROVAL` script answers `permission` and ends its turn; `FILE_APPROVAL` must end
the same way. Reuse that path: match `"permission"` already handles the reply.)

`tests/live.rs`:

```rust
#[tokio::test]
async fn codex_failed_command_is_a_failed_call() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(scenario::TOOL_FAIL.into())).unwrap();
    let mut calls = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Tool(call) => calls.push(call),
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    assert_eq!(calls.iter().map(|c| c.phase).collect::<Vec<_>>(), [ToolPhase::Running, ToolPhase::Failed]);
    assert_eq!(calls[1].target, "false");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_file_approval_names_the_items_files() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(scenario::FILE_APPROVAL.into())).unwrap();
    let Event::Approval { id, summary, .. } = wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await else {
        unreachable!()
    };
    assert_eq!(summary.action, "Edit 2 files");
    assert_eq!(summary.files, ["a.rs", "b.rs"]);
    handle.send(Command::Answer { id, allow: false }).unwrap();
    stop(&handle, task).await;
}
```

Run: `scripts/rust-env.sh cargo test -p octet-engine --test live codex_`
Expected: PASS.

- [ ] **Step 4: Gate and commit**

```bash
if make rust-check; then
  git add crates/octet-engine/src/live/codex.rs crates/octet-engine/tests/live.rs crates/octet-testkit
  git commit -m "Codex: tool items are typed calls that fail on status or exit; approvals name their command or files

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 3: Grouped tool activity in the transcript

**Files:**

- Modify: `crates/octet-tui/src/app.rs` (`Entry`, `Transcript`, `Event::Tool`, `Event::Finished`, `add`, `trim`)
- Modify: `crates/octet-tui/src/view/transcript.rs` (`Entry::rows`, new `tool_rows`, `label`, `summary`)
- Modify: `crates/octet-tui/src/handoff.rs:68-77` and its test at `handoff.rs:200-215`
- Test: `crates/octet-tui/src/view/transcript.rs` (unit), `crates/octet-tui/src/app/tests.rs`

**Interfaces:**

- Consumes: `octet_core::{ToolCall, ToolPhase}`, `test_support::tool` (Task 1).
- Produces:
  - `Entry { …, pub(crate) tools: Vec<octet_core::ToolCall>, pub(crate) expanded: bool, pub(crate) turn_over: bool, pub(crate) seq: u64 }`
  - `Entry::size(&self) -> usize`, `Entry::record(&mut self, call: ToolCall)`
  - `Transcript { …, pub(crate) tools_expanded: bool, pub(crate) next_seq: u64 }`
  - `transcript::label(name: &str) -> &str` (display name: `commandExecution` → `Bash`, `fileChange` → `Edit`, `mcpToolCall` → `MCP`, else the name)
  - `App::running_tool(&self) -> Option<&ToolCall>` (the newest call still running in a tools entry whose turn is not over)

- [ ] **Step 1: Failing tests for grouping, pairing and rows**

In `crates/octet-tui/src/app/tests.rs`:

```rust
use octet_core::ToolPhase::{Done, Failed, Running};
use crate::test_support::tool;
#[test]
fn consecutive_calls_share_one_entry_and_ends_replace_starts() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.event(tool("1", "Read", "a.rs", Running));
    app.event(tool("2", "Read", "b.rs", Running));
    app.event(tool("1", "Read", "a.rs", Done));
    let entry = app.chat.entries.back().unwrap();
    assert!(entry.role == Role::Tool);
    assert_eq!(entry.tools.iter().map(|c| (c.id.as_str(), c.phase)).collect::<Vec<_>>(), [("1", Done), ("2", Running)]);
    app.event(Event::Text("then".into()));
    app.event(tool("3", "Bash", "ls", Running));
    assert_eq!(app.chat.entries.iter().filter(|e| e.role == Role::Tool).count(), 2, "text starts a new group");
}
#[test]
fn an_unmatched_end_is_its_own_call() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(tool("9", "Bash", "ls", Done));
    app.event(tool("", "Bash", "a", Running));
    app.event(tool("", "Bash", "a", Done));
    assert_eq!(app.chat.entries.back().unwrap().tools.len(), 3, "an empty ID never pairs");
}
#[test]
fn cancelled_calls_say_stopped() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.event(tool("1", "Bash", "sleep 9", Running));
    app.event(Event::Finished { outcome: octet_core::Outcome::Interrupted });
    let rows = app.visible_lines(80, 10);
    assert!(rows.iter().any(|r| r.to_string().contains("Bash sleep 9 · stopped")), "{rows:?}");
    assert!(app.running_tool().is_none());
}
#[test]
fn tool_bytes_are_counted_and_trimmed() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(tool("1", "Read", "a.rs", Running));
    let size = app.chat.entries.back().unwrap().size();
    assert_eq!(app.chat.bytes, size);
    app.event(tool("1", "Read", "a.rs", Done));
    assert_eq!(app.chat.bytes, app.chat.entries.back().unwrap().size());
}
```

In `crates/octet-tui/src/view/transcript.rs` tests:

```rust
fn call(id: &str, name: &str, target: &str, phase: octet_core::ToolPhase, detail: &str) -> octet_core::ToolCall {
    octet_core::ToolCall { id: id.into(), name: name.into(), target: target.into(), phase, detail: detail.into() }
}
fn texts(rows: &[Line<'static>]) -> Vec<String> {
    rows.iter().map(ToString::to_string).collect()
}
#[test]
fn collapsed_calls_summarise_done_show_running_and_open_failures() {
    use octet_core::ToolPhase::*;
    let mut entry = Entry::tools(Engine::CLAUDE, false);
    for c in [
        call("1", "Read", "a.rs", Done, "x"),
        call("2", "Read", "b.rs", Done, "x"),
        call("3", "Read", "c.rs", Done, "x"),
        call("4", "commandExecution", "cargo build", Done, "x"),
        call("5", "Grep", "fn main", Running, "x"),
        call("6", "Bash", "cargo test", Failed, &(1..=12).map(|n| format!("out {n}")).collect::<Vec<_>>().join("\n")),
    ] {
        entry.record(c);
    }
    let rows = texts(entry.rows(80));
    assert_eq!(rows[0], "▸ Read 3 files · Ran 1 command   Ctrl+O details");
    assert_eq!(rows[1], "… Grep fn main");
    assert_eq!(rows[2], "✗ Bash cargo test");
    assert_eq!(&rows[3..11], (5..=12).map(|n| format!("  out {n}")).collect::<Vec<_>>().as_slice());
    assert_eq!(rows[11], "  +4 more lines · Ctrl+O");
    assert_eq!(rows.len(), 12, "no label row, no blank line");
}
#[test]
fn expanded_calls_list_each_with_its_detail() {
    use octet_core::ToolPhase::*;
    let mut entry = Entry::tools(Engine::CLAUDE, true);
    entry.record(call("1", "Read", "a.rs", Done, "line one\nline two"));
    entry.record(call("2", "Bash", "ls", Failed, "boom"));
    assert_eq!(texts(entry.rows(80)), ["✓ Read a.rs", "  line one", "  line two", "✗ Bash ls", "  boom"]);
}
#[test]
fn summary_words_are_singular_for_one() {
    assert_eq!(summary(&[call("1", "Read", "a", octet_core::ToolPhase::Done, "")]), "Read 1 file");
    let grep = call("1", "Grep", "a", octet_core::ToolPhase::Done, "");
    let fetch = call("2", "WebFetch", "u", octet_core::ToolPhase::Done, "");
    assert_eq!(summary(&[grep.clone(), fetch.clone(), fetch]), "Searched once · WebFetch ×2");
}
```

Run: `scripts/rust-env.sh cargo test -p octet-tui consecutive_calls an_unmatched cancelled_calls tool_bytes collapsed_calls expanded_calls summary_words`
Expected: FAIL to compile (`Entry::tools`, `record`, `size`, `running_tool`, `summary` missing).

- [ ] **Step 2: Entry and App changes**

`app.rs` `Entry` gains the fields from **Interfaces** (doc lines: "The calls of a tools entry, in
order", "Its calls are drawn in full", "Its turn ended: calls still running were stopped", "Its
place in the conversation, newest highest"). `Entry::new` sets them to empty/false/false/0, and:

```rust
    /// An empty tools entry, drawn in full when `expanded`.
    pub(crate) fn tools(engine: octet_core::Engine, expanded: bool) -> Self {
        Self { expanded, ..Self::new(Role::Tool, engine, String::new()) }
    }
    /// Bytes it holds, for the transcript's limit.
    pub(crate) fn size(&self) -> usize {
        self.text.len()
            + self.tools.iter().map(|c| c.name.len() + c.target.len() + c.detail.len()).sum::<usize>()
    }
    /// A call's start or end: an end replaces the call with its ID (keeping the start's target
    /// when it has none); anything else is added. Its rows are drawn again.
    pub(crate) fn record(&mut self, mut call: octet_core::ToolCall) {
        let same = self.tools.iter_mut().find(|c| !call.id.is_empty() && c.id == call.id);
        match same {
            Some(existing) => {
                if call.target.is_empty() {
                    call.target = std::mem::take(&mut existing.target);
                }
                *existing = call;
            }
            None => self.tools.push(call),
        }
        self.invalidate();
    }
```

`Transcript` gains `tools_expanded: bool` (doc: "Tool calls are drawn in full (Ctrl+O)") and
`next_seq: u64`; both start `false`/`0` in `App::new`.

`App::add` assigns the sequence and uses `size()`; `trim` subtracts `old.size()`:

```rust
        self.chat.bytes += text.len();
        let mut entry = Entry::new(role, self.conn.engine, text);
        self.chat.next_seq += 1;
        entry.seq = self.chat.next_seq;
        self.chat.entries.push_back(entry);
        self.trim();
```

New method beside `add`:

```rust
    /// A tool call joins the newest entry when that is a tools entry, else starts one.
    fn tool(&mut self, mut call: octet_core::ToolCall) {
        call.name = clean(&call.name);
        call.target = clean(&call.target);
        call.detail = clean(&call.detail);
        if call.detail.len() > BLOCK_BYTES {
            let start = call.detail.ceil_char_boundary(call.detail.len() - BLOCK_BYTES);
            call.detail = format!("[Earlier output is in the journal]\n{}", &call.detail[start..]);
        }
        if self.chat.entries.back().is_none_or(|e| e.role != Role::Tool || e.turn_over) {
            let mut entry = Entry::tools(self.conn.engine, self.chat.tools_expanded);
            self.chat.next_seq += 1;
            entry.seq = self.chat.next_seq;
            self.chat.entries.push_back(entry);
        }
        let entry = self.chat.entries.back_mut().expect("a tools entry was just ensured");
        let before = entry.size();
        entry.record(call);
        self.chat.bytes = self.chat.bytes - before + entry.size();
        self.trim();
    }
    /// The newest call still running, in a turn that has not ended.
    pub(crate) fn running_tool(&self) -> Option<&octet_core::ToolCall> {
        self.chat
            .entries
            .iter()
            .rev()
            .filter(|e| e.role == Role::Tool && !e.turn_over)
            .flat_map(|e| e.tools.iter().rev())
            .find(|c| c.phase == octet_core::ToolPhase::Running)
    }
```

`Event::Tool(call)` arm: `self.chat.reply_break = true; self.conn.activity = State::tool(&call); self.tool(call);`.
`Event::Finished` arm, first lines:

```rust
                for entry in self.chat.entries.iter_mut().filter(|e| e.role == Role::Tool && !e.turn_over) {
                    entry.turn_over = true;
                    entry.invalidate();
                }
```

- [ ] **Step 3: Rows for a tools entry**

In `transcript.rs`, `Entry::rows` starts with:

```rust
        if self.role == Role::Tool {
            if self.width != Some(width) {
                self.cache = tool_rows(&self.tools, self.expanded, self.turn_over, width);
                self.width = Some(width);
            }
            return &self.cache;
        }
```

and the module gains:

```rust
/// Rows of a failed call kept when collapsed.
const FAILED_ROWS: usize = 8;
/// A tool's name as the transcript says it.
pub(crate) fn label(name: &str) -> &str {
    match name {
        "commandExecution" => "Bash",
        "fileChange" => "Edit",
        "mcpToolCall" => "MCP",
        other => other,
    }
}
/// The finished calls in words, by name in order of first appearance: "Read 3 files · Ran 1 command".
pub(crate) fn summary(calls: &[octet_core::ToolCall]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        let name = label(&call.name);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    counts
        .into_iter()
        .map(|(name, n)| match name {
            "Read" => format!("Read {}", plural(n, "file", "files")),
            "Edit" | "MultiEdit" | "NotebookEdit" => format!("Edited {}", plural(n, "file", "files")),
            "Write" => format!("Wrote {}", plural(n, "file", "files")),
            "Bash" => format!("Ran {}", plural(n, "command", "commands")),
            "Grep" | "Glob" if n == 1 => "Searched once".to_owned(),
            "Grep" | "Glob" => format!("Searched {n} times"),
            other => format!("{other} ×{n}"),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}
/// A tools entry's rows: collapsed, a summary of finished calls, a line per running call and
/// each failure open (cut to its last `FAILED_ROWS` lines); expanded, every call and its detail.
fn tool_rows(calls: &[octet_core::ToolCall], expanded: bool, turn_over: bool, width: u16) -> Vec<Line<'static>> {
    use octet_core::ToolPhase::{Done, Failed, Running};
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut push = |text: String, style: Style, rows: &mut Vec<Line<'static>>| {
        for row in wrap(&text, width) {
            rows.push(Line::from(Span::styled(row, style)));
        }
    };
    let head = |call: &octet_core::ToolCall, mark: &str| {
        let stopped = if call.phase == Running && turn_over { " · stopped" } else { "" };
        format!("{mark} {} {}{stopped}", label(&call.name), call.target).trim_end().to_owned()
    };
    let detail = |text: &str, keep: Option<usize>, rows: &mut Vec<Line<'static>>| {
        let lines: Vec<&str> = text.lines().collect();
        let skip = keep.map_or(0, |keep| lines.len().saturating_sub(keep));
        for line in &lines[skip..] {
            push(format!("  {line}"), Style::default().fg(MUTED), rows);
        }
        if skip > 0 {
            push(format!("  +{skip} more lines · Ctrl+O"), Style::default().fg(MUTED), rows);
        }
    };
    if expanded {
        for call in calls {
            let (mark, color) = match call.phase { Done => ("✓", MUTED), Failed => ("✗", AMBER), Running => ("…", MUTED) };
            push(head(call, mark), Style::default().fg(color), &mut rows);
            detail(&call.detail, None, &mut rows);
        }
        return rows;
    }
    let done: Vec<octet_core::ToolCall> = calls.iter().filter(|c| c.phase == Done).cloned().collect();
    if !done.is_empty() {
        push(format!("▸ {}   Ctrl+O details", summary(&done)), Style::default().fg(MUTED), &mut rows);
    }
    for call in calls.iter().filter(|c| c.phase == Running) {
        push(head(call, "…"), Style::default().fg(MUTED), &mut rows);
    }
    for call in calls.iter().filter(|c| c.phase == Failed) {
        push(head(call, "✗"), Style::default().fg(AMBER), &mut rows);
        detail(&call.detail, Some(FAILED_ROWS), &mut rows);
    }
    rows
}
```

Import `Engine` in the transcript tests (`use octet_core::Engine;` is already there). If the borrow
checker rejects the `push` closure capturing nothing but taking `rows` as an argument, make it a
plain `fn push(rows: &mut Vec<Line<'static>>, text: &str, style: Style, width: usize)`.

- [ ] **Step 4: Handoff from the calls**

`handoff.rs:68-77`:

```rust
        Role::Tool => {
            let mut lines = String::new();
            for call in &entry.tools {
                let line = format!("{} · {}", call.name, call.target);
                let line: String = line.chars().take(TOOL_LINE).collect();
                let _ = writeln!(lines, "Tool ({engine}): {line}");
            }
            lines
        }
```

(add `use std::fmt::Write as _;` if absent). Update `each_role_renders_as_the_spec_says`: build the
tool entry with `let mut tools = Entry::tools(Engine::CODEX, false); tools.record(ToolCall { id: "1".into(), name: "Bash".into(), target: "cargo build".into(), phase: ToolPhase::Done, detail: String::new() });`
and assert `text.contains("Tool (codex): Bash · cargo build")`.

- [ ] **Step 5: Run the tests**

Run: `scripts/rust-env.sh cargo test -p octet-tui`
Expected: PASS. Fix any older test that read a tool entry's `text` (e.g. `entries_text` checks of
tool text): read `entry.tools` instead.

- [ ] **Step 6: Gate and commit**

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: a run of tool calls is one entry; finished calls summarised, failures open

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 4: Ctrl+O shows or hides tool details

**Files:**

- Modify: `crates/octet-tui/src/app.rs` (new `toggle_tool_details`)
- Modify: `crates/octet-tui/src/input.rs` (`composer_key`, `palette_key`)
- Modify: `crates/octet-tui/src/view.rs` (`help_lines`, `PALETTE_KEYS`)
- Test: `crates/octet-tui/src/tests/keys.rs`, `crates/octet-tui/src/view/tests.rs`

**Interfaces:**

- Consumes: `Entry::{expanded, invalidate}`, `Transcript::tools_expanded` (Task 3).
- Produces: `App::toggle_tool_details(&mut self)`; `PALETTE_KEYS` has 4 rows, the new one last:
  `("Ctrl+O", "Show or hide tool details")`.

- [ ] **Step 1: Failing test**

`tests/keys.rs`:

```rust
#[tokio::test]
async fn ctrl_o_flips_tool_details_for_every_tools_entry() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.event(crate::test_support::tool("1", "Read", "a.rs", octet_core::ToolPhase::Done));
    app.event(octet_core::Event::Text("x".into()));
    app.event(crate::test_support::tool("2", "Read", "b.rs", octet_core::ToolPhase::Done));
    key_action(&mut app, &vendor, ctrl_key('o')).await;
    assert!(app.chat.tools_expanded);
    assert!(app.chat.entries.iter().filter(|e| e.role == crate::app::Role::Tool).all(|e| e.expanded));
    assert_eq!(app.status_line, "Tool details shown");
    key_action(&mut app, &vendor, ctrl_key('o')).await;
    assert!(!app.chat.tools_expanded);
    assert_eq!(app.status_line, "Tool details hidden");
    app.event(crate::test_support::tool("3", "Read", "c.rs", octet_core::ToolPhase::Done));
    assert!(!app.chat.entries.back().unwrap().expanded, "a new entry follows the setting");
}
```

(If `ctrl_key` is not an existing helper in `tests.rs`, use
`KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)`; check `grep -n "fn ctrl_key\|fn key(" crates/octet-tui/src/tests.rs`.)

In `view/tests.rs`, extend `the_palette_shows_every_command_and_its_keys` (read it first) so it
expects a `Ctrl+O` row, and add to `help_names_the_command_palette_key`'s neighbour:

```rust
#[test]
fn help_names_the_tool_details_key() {
    assert!(help_lines().iter().any(|l| l.contains("Ctrl+O show or hide tool details")));
}
```

Run: `scripts/rust-env.sh cargo test -p octet-tui ctrl_o_flips help_names_the_tool palette_shows`
Expected: FAIL.

- [ ] **Step 2: Implement**

`app.rs`:

```rust
    /// Ctrl+O: every tools entry is drawn in full, or collapsed again.
    pub(crate) fn toggle_tool_details(&mut self) {
        let expanded = !self.chat.tools_expanded;
        self.chat.tools_expanded = expanded;
        for entry in self.chat.entries.iter_mut().filter(|e| e.role == Role::Tool) {
            entry.expanded = expanded;
            entry.invalidate();
        }
        self.hint(if expanded { "Tool details shown" } else { "Tool details hidden" });
    }
```

`input.rs` `composer_key`: `KeyCode::Char('o') if ctrl => app.toggle_tool_details(),` beside
`Ctrl+X`. `palette_key`: a `"Ctrl+O" => app.toggle_tool_details(),` arm before the `@` arm.
`view.rs`: append `("Ctrl+O", "Show or hide tool details")` to `PALETTE_KEYS` (type becomes
`[(&str, &str); 4]`); in `help_lines` change `"Ctrl+P commands · Ctrl+G write the prompt in $EDITOR"`
to keep it and add a line after it: `"Ctrl+O show or hide tool details"`.

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: Ctrl+O shows or hides every tool call's detail

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 5: The approval dialog leads with the summary

**Files:**

- Modify: `crates/octet-tui/src/app.rs` (`Overlays::approvals` element type; `Event::Approval`, `Event::ApprovalClosed`)
- Modify: `crates/octet-tui/src/view.rs` (`overlays`, `approval`, new `summary_lines`)
- Modify: `crates/octet-tui/src/input.rs:403-406`, `crates/octet-tui/src/lib.rs:407` (front access)
- Test: `crates/octet-tui/src/view/tests.rs`

**Interfaces:**

- Consumes: `octet_core::ApprovalSummary`, `test_support::approval` (Task 1).
- Produces: `pub(crate) struct Asked { pub(crate) id: u64, pub(crate) detail: String, pub(crate) summary: octet_core::ApprovalSummary }`;
  `Overlays::approvals: VecDeque<Asked>`; `view::summary_lines(&ApprovalSummary) -> Vec<String>`.

- [ ] **Step 1: Failing tests**

`view/tests.rs`:

```rust
#[test]
fn the_approval_leads_with_action_command_folder_and_scope() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(crate::test_support::approval(3, "{\n  \"tool_name\": \"Bash\"\n}"));
    let rows = screen(100, 30, &mut app);
    let at = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap_or_else(|| panic!("{text}\n{}", rows.join("\n")));
    assert!(at("Permission required · #3 · 1 pending") < at("Run a shell command"));
    assert!(at("Run a shell command") < at("$ cargo test"));
    assert!(at("$ cargo test") < at("in /tmp"));
    assert!(at("in /tmp") < at("Allows: This request only, once"));
    assert!(at("Allows:") < at("Full request · PgUp/PgDn"));
    assert!(at("Full request") < at("\"tool_name\": \"Bash\""));
    assert!(rows.iter().any(|r| r.contains("A  Allow once")));
}
#[test]
fn summary_lines_bound_commands_and_files() {
    let summary = octet_core::ApprovalSummary {
        command: Some("a\nb\nc\nd".into()),
        files: (0..9).map(|n| format!("f{n}")).collect(),
        ..octet_core::ApprovalSummary::new("Edit 9 files")
    };
    assert_eq!(
        summary_lines(&summary),
        ["Edit 9 files", "$ a", "  b", "  c", "  …", "f0", "f1", "f2", "f3", "f4", "+4 more", "Allows: This request only, once"]
    );
}
#[test]
fn a_huge_approval_keeps_its_keys_on_a_small_screen() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(octet_core::Event::Approval {
        id: 1,
        detail: "x\n".repeat(500),
        summary: octet_core::ApprovalSummary {
            command: Some("y".repeat(5000)),
            files: (0..100).map(|n| format!("f{n}")).collect(),
            ..octet_core::ApprovalSummary::new("Edit 100 files")
        },
    });
    for (w, h) in [(40, 12), (38, 12), (80, 24)] {
        let rows = screen(w, h, &mut app);
        assert!(rows.iter().any(|r| r.contains("Allow once")), "{w}×{h}\n{}", rows.join("\n"));
    }
}
```

Run: `scripts/rust-env.sh cargo test -p octet-tui the_approval_leads summary_lines_bound a_huge_approval`
Expected: FAIL.

- [ ] **Step 2: Implement**

`app.rs`: define `Asked` (doc: "An approval waiting for the user") and change
`approvals: VecDeque<(u64, String)>` to `VecDeque<Asked>`. The `Event::Approval` arm:

```rust
            Event::Approval { id, detail, summary } => {
                let first = self.overlay.approvals.is_empty();
                let summary = octet_core::ApprovalSummary {
                    action: clean(&summary.action),
                    command: summary.command.as_deref().map(clean),
                    files: summary.files.iter().map(|f| clean(f)).collect(),
                    cwd: summary.cwd.as_deref().map(clean),
                    scope: clean(&summary.scope),
                };
                self.overlay.approvals.push_back(Asked { id, detail: clean(&detail), summary });
```

`ApprovalClosed`: `.front().map(|asked| asked.id)` and `.retain(|asked| asked.id != id)`. Update
`input.rs:403` (`if let Some(asked) = app.overlay.approvals.front() { let id = asked.id; …`) and
every other `approvals.front()` / tuple use (`grep -n "approvals" crates/octet-tui/src -r`).

`view.rs`:

```rust
/// The approval's lead: what it would do, its command (3 lines at most), its files (5 at most),
/// where, and what allowing grants.
pub(crate) fn summary_lines(summary: &octet_core::ApprovalSummary) -> Vec<String> {
    let mut lines = vec![summary.action.clone()];
    if let Some(command) = &summary.command {
        let all: Vec<&str> = command.lines().collect();
        for (n, line) in all.iter().take(3).enumerate() {
            lines.push(format!("{} {line}", if n == 0 { "$" } else { " " }));
        }
        if all.len() > 3 {
            lines.push("  …".into());
        }
    }
    lines.extend(summary.files.iter().take(5).cloned());
    if summary.files.len() > 5 {
        lines.push(format!("+{} more", summary.files.len() - 5));
    }
    if let Some(cwd) = &summary.cwd {
        lines.push(format!("in {cwd}"));
    }
    lines.push(format!("Allows: {}", summary.scope));
    lines
}
fn approval(frame: &mut Frame, area: Rect, asked: &crate::app::Asked, scroll: &mut u16, count: usize) {
    let area = modal(area, 86, 24);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(PANEL)), area);
    let width = usize::from(area.width.saturating_sub(2));
    let lead = wrapped(&summary_lines(&asked.summary), width);
    // The keys keep their rows; the lead comes next; the full request takes what is left.
    let [heading, lead_area, request, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(cells(lead.len())),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(format!(" Permission required · #{} · {count} pending", asked.id))
            .style(Style::default().fg(AMBER).bg(PANEL).bold()),
        heading,
    );
    frame.render_widget(
        Paragraph::new(lead.iter().map(|l| Line::from(format!(" {l}"))).collect::<Vec<_>>())
            .style(Style::default().fg(FG).bg(PANEL)),
        lead_area,
    );
    let inner = usize::from(request.width.saturating_sub(2));
    let rows = wrapped(&asked.detail.lines().collect::<Vec<_>>(), inner);
    let visible = request.height.saturating_sub(2);
    *scroll = (*scroll).min(cells(rows.len()).saturating_sub(visible));
    if request.height >= 3 {
        frame.render_widget(
            Paragraph::new(rows.join("\n"))
                .scroll((*scroll, 0))
                .block(card(" Full request · PgUp/PgDn "))
                .style(Style::default().bg(PANEL).fg(MUTED)),
            request,
        );
    }
    frame.render_widget(
        Paragraph::new("\n A  Allow once     D / Esc  Deny     Ctrl+C  Cancel turn")
            .style(Style::default().fg(AMBER).bg(PANEL)),
        keys,
    );
}
```

Note: with `Length(lead)` before `Min(0)`, ratatui shrinks the request box first; if the lead
alone exceeds the modal, ratatui's solver still keeps the `Length(2)` keys row only if the lead is
capped, so cap it: `Constraint::Length(cells(lead.len()).min(area.height.saturating_sub(3)))`.
`overlays` passes `app.overlay.approvals.front()` as `asked`. Update `approval_scroll_is_clamped`
to use `test_support::approval(1, "one short line")`.

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: an approval leads with its action, command, files, folder and grant; the request follows

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 6: Skills: Enter prepares; another provider's skill is announced, then switches

**Files:**

- Modify: `crates/octet-tui/src/commands.rs` (`skill_command`, new `find_skill`, `foreign_skill`)
- Modify: `crates/octet-tui/src/input.rs` (`completion_key`, `accept_completion`, remove `pick_skill`; new `choose_skill`)
- Modify: `crates/octet-tui/src/view.rs:323` (picker title)
- Test: `crates/octet-tui/src/tests/keys.rs`, `crates/octet-tui/src/tests/slash.rs`

**Interfaces:**

- Produces:
  - `commands::foreign_skill(app: &App, draft: &str) -> Option<octet_core::Engine>`: the provider
    the draft's `/name` would switch to: not an Octet command, not a current-provider skill, and a
    skill only another provider lists.
  - `crate::app::SkillRun` unchanged; typed foreign skills now fill it with their arguments.

- [ ] **Step 1: Failing tests**

`tests/keys.rs`: replace `the_skills_picker_lists_both_providers_and_enter_runs` and
`picking_another_providers_skill_switches_then_runs` with:

```rust
#[tokio::test]
async fn enter_in_the_skills_picker_prepares_the_skill() {
    let mut app = skills_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    let picker = app.composer.completion.as_ref().expect("picker open");
    assert!(picker.items[0].starts_with("imagegen · codex"), "{:?}", picker.items);
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "/imagegen ");
    assert!(app.composer.completion.is_none());
    assert!(vendor.sent.borrow().is_empty(), "nothing runs until the draft is sent");
    type_keys(&mut app, &vendor, "a cat").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(matches!(&vendor.sent.borrow()[0], Command::Skill { name, args, .. } if name == "imagegen" && args == "a cat"));
}
#[tokio::test]
async fn another_providers_skill_is_prepared_then_switches_with_its_arguments() {
    let mut app = skills_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(app.composer.editor.text(), "/brainstorming ", "Tab prepares another provider's too");
    type_keys(&mut app, &vendor, "the cache").await;
    assert_eq!(crate::commands::foreign_skill(&app, app.composer.editor.text()), Some(Engine::CLAUDE));
    let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(matches!(&action, Action::Exit(Exit::Model(s)) if s.provider == Engine::CLAUDE));
    let run = app.pending_skill.as_ref().expect("waits for the switch");
    assert_eq!((run.skill.name.as_str(), run.args.as_str()), ("brainstorming", "the cache"));
    assert!(app.composer.editor.text().is_empty());
}
#[tokio::test]
async fn an_octet_command_never_switches_provider() {
    let mut app = skills_app();
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(Engine::CLAUDE, &ws, vec![skill("model", &[], None)], std::time::SystemTime::now());
    assert_eq!(crate::commands::foreign_skill(&app, "/model list"), None);
    assert_eq!(crate::commands::foreign_skill(&app, "/imagegen x"), None, "the current provider's");
    assert_eq!(crate::commands::foreign_skill(&app, "hello /brainstorming"), None, "not a slash draft");
}
```

Keep the existing post-switch half (`app.connection(…)` then `session_event(Ready)` runs it) as a
separate test `a_pending_skill_runs_once_the_switch_is_ready` that sets `app.pending_skill` directly
with `args: "the cache".into()` and asserts `Command::Skill { name, args, .. }` with both values.

`tests/slash.rs`: rename `another_providers_skill_typed_gives_the_hint` to
`another_providers_skill_typed_switches` and assert
`matches!(action, Some(Action::Exit(Exit::Model(s))) if s.provider == Engine::CLAUDE)` and
`app.pending_skill.is_some()`. Add a refusal case: with a turn open
(`app.event(octet_core::Event::Started)`), the action is `None`, `pending_skill` is `None`, and the
status line holds the `can_reconnect` refusal.

Run: `scripts/rust-env.sh cargo test -p octet-tui skill`
Expected: FAIL.

- [ ] **Step 2: Implement**

`commands.rs`:

```rust
/// Where a typed `/NAME` skill is: the current provider's, or only another's.
enum Found {
    Here(octet_core::SkillInfo),
    There(octet_core::Engine, octet_core::SkillInfo),
}
/// The skill `/NAME` names (by name or alias), the current provider's first.
fn find_skill(app: &App, name: &str) -> Option<Found> {
    let wanted = name.strip_prefix('/').unwrap_or(name);
    let named = |skill: &octet_core::SkillInfo| skill.name == wanted || skill.aliases.iter().any(|a| a == wanted);
    let entries = app.skill_entries();
    if let Some((_, skill)) = entries.iter().find(|(engine, skill)| *engine == app.conn.engine && named(skill)) {
        return Some(Found::Here(skill.clone()));
    }
    entries.into_iter().find(|(_, skill)| named(skill)).map(|(engine, skill)| Found::There(engine, skill))
}
/// The provider sending `draft` would switch to: it is `/NAME …` where NAME is no Octet command
/// and only another provider's skill.
pub(crate) fn foreign_skill(app: &App, draft: &str) -> Option<octet_core::Engine> {
    let name = draft.strip_prefix('/')?.split(char::is_whitespace).next()?;
    if name.is_empty() || Spec::find(&format!("/{name}")).is_some() {
        return None;
    }
    match find_skill(app, name)? {
        Found::There(engine, _) => Some(engine),
        Found::Here(_) => None,
    }
}
```

(Check how `Spec::find` expects its argument: `try_command` passes `name` with its slash, so pass
`&format!("/{name}")` as shown, or the bare name if `Spec::find` strips it.)

`skill_command` becomes:

```rust
fn skill_command(app: &mut App, vendor: &dyn Vendor, name: &str, argument: &str) -> Option<Action> {
    match find_skill(app, name) {
        // A skill that cannot go leaves the draft, as a prompt does.
        Some(Found::Here(skill)) => crate::input::run_skill(app, vendor, &skill, argument).then_some(Action::Continue),
        Some(Found::There(engine, skill)) => {
            if let Err(refusal) = can_reconnect(app) {
                app.hint(refusal);
                return None;
            }
            app.pending_skill = Some(crate::app::SkillRun { engine, skill, args: argument.to_owned() });
            Some(Action::Exit(Exit::Model(octet_core::model::Selection { provider: engine, model: None })))
        }
        None => {
            app.note(format!(
                "Unknown command {name}. Use /help, or edit the draft: \
                 only a path such as /usr/lib can start a prompt with a slash. \
                 /skills lists what each provider offers."
            ));
            None
        }
    }
}
```

`input.rs`: delete `pick_skill`; add

```rust
/// Enter or Tab on a skills picker line: the draft becomes `/name `, for its arguments.
/// Nothing runs until the draft is sent.
fn choose_skill(app: &mut App, item: Option<String>) {
    if let Some((_, skill)) = item.and_then(|item| picked_skill(app, &item)) {
        app.composer.editor.set(format!("/{} ", skill.name));
    }
    app.composer.completion = None;
}
```

In `completion_key`, the `Kind::Skill` Enter branch becomes
`let item = …; choose_skill(app, item); return Some(Action::Continue);`, and in
`accept_completion` the `Kind::Skill` arm becomes
`composer::Kind::Skill => { let item = Some(item.clone()); choose_skill(app, item); return; }`
(the `vendor` argument `completion_key` no longer needs for skills stays for its other uses).
`view.rs:323`: `crate::composer::Kind::Skill => " Skills · Enter choose ",`.

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: Enter on a skill prepares it; another provider's skill switches only when the draft is sent

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 7: The composer says one thing about Enter; chips get their own row

**Files:**

- Modify: `crates/octet-tui/src/view.rs` (`paint`, `composer`, `composer_title`, `composer_height`, new `chips`, `composer_hint`)
- Test: `crates/octet-tui/src/view/tests.rs`

**Interfaces:**

- Consumes: `commands::foreign_skill` (Task 6).
- Produces: `composer_height(draft_rows: usize, chips: bool) -> u16`; `chips(app: &App) -> Option<String>`;
  `composer_hint(app: &App, width: u16) -> (String, Color)`.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn the_hint_row_matches_what_enter_does() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    assert_eq!(composer_hint(&app, 100).0, "Enter send · Alt+Enter newline · ↑ history · Ctrl+P commands");
    assert_eq!(composer_hint(&app, 60).0, "Enter send · Alt+Enter newline");
    app.event(Event::Started);
    assert_eq!(composer_hint(&app, 100).0, "Enter queue · Esc cancel turn · Ctrl+P commands");
    let rows = screen(100, 30, &mut app);
    assert!(rows.iter().any(|r| r.contains("╭ Next prompt ─")), "{}", rows.join("\n"));
    assert!(!rows.iter().any(|r| r.contains("Enter send")), "no contradicting hint");
}
#[test]
fn the_hint_warns_before_a_provider_switch() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        octet_core::Engine::CLAUDE,
        &ws,
        vec![octet_core::SkillInfo { name: "review".into(), description: String::new(), aliases: Vec::new(), argument_hint: String::new(), path: None }],
        std::time::SystemTime::now(),
    );
    assert!(app.composer.editor.insert("/review src"));
    assert_eq!(composer_hint(&app, 100), ("Enter switches to claude, then runs /review".into(), AMBER));
}
#[test]
fn chips_have_their_own_row_and_grow_the_box() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert_eq!(composer_height(1, false), 4);
    assert_eq!(composer_height(1, true), 5);
    app.attach(crate::shell::Ran { command: "ls".into(), status: crate::shell::Status::Exited(0), output: "a\n".into() });
    app.composer.queue.push_back(crate::vendor::Prompt::plain("later".into()));
    assert_eq!(chips(&app).as_deref(), Some("+ ls (exit 0) · 1 queued"));
    let rows = screen(100, 30, &mut app);
    let title = rows.iter().position(|r| r.contains("╭ Prompt ─")).unwrap();
    assert!(rows[title + 2].contains("+ ls (exit 0) · 1 queued"), "{}", rows.join("\n"));
}
```

Four existing tests read chips from the title and must read `chips(&app)` instead:
`tests/keys.rs:392` (expect `"1 queued"`), `tests/slash.rs:344`, `:390`, `:494` (same image
strings as today). Update `composer_grows_with_the_draft`, `composer_counts_wrapped_rows`, `composer_height_is_capped`
to pass `false`.

Run: `scripts/rust-env.sh cargo test -p octet-tui the_hint_row the_hint_warns chips_have composer_`
Expected: FAIL.

- [ ] **Step 2: Implement**

```rust
/// The prompt box's title: whether this prompt goes now or after the turn.
pub(crate) fn composer_title(app: &App) -> &'static str {
    if app.conn.is_running() { " Next prompt " } else { " Prompt " }
}
/// What waits to go with the next prompt: attachments, images, the queue.
pub(crate) fn chips(app: &App) -> Option<String> {
    let mut parts: Vec<String> = crate::composer::chip(&app.composer.attachments).into_iter().collect();
    match app.composer.images.as_slice() {
        [] => {}
        [one] => parts.push(format!("+ image {}", one.name)),
        many => parts.push(format!("+{} images", many.len())),
    }
    if !app.composer.queue.is_empty() {
        parts.push(format!("{} queued", app.composer.queue.len()));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}
/// The hint row: what Enter does now, then the next keys that fit.
pub(crate) fn composer_hint(app: &App, width: u16) -> (String, Color) {
    let draft = app.composer.editor.text();
    if let Some(engine) = crate::commands::foreign_skill(app, draft) {
        let name = draft.split(char::is_whitespace).next().unwrap_or(draft);
        return (format!("Enter switches to {engine}, then runs {name}"), AMBER);
    }
    let keys: &[&str] = if app.conn.is_running() {
        &["Enter queue", "Esc cancel turn", "Ctrl+P commands"]
    } else {
        &["Enter send", "Alt+Enter newline", "↑ history", "Ctrl+P commands"]
    };
    let shown = if width >= 80 { keys.len() } else { 2 };
    (keys[..shown].join(" · "), MUTED)
}
/// The composer's height: borders, 1–4 draft rows, the chips row when there is one, the hint row.
fn composer_height(draft_rows: usize, chips: bool) -> u16 {
    2 + cells(draft_rows.clamp(1, 4)) + u16::from(chips) + 1
}
```

In `paint`: `Constraint::Length(composer_height(draft.0.len(), chips(app).is_some()))`.
In `composer`: `let composer = card(composer_title(app))…`; rows inside:

```rust
    let chips = chips(app);
    // At the minimum height the draft keeps its row; the hint, then the chips, give way.
    let hint_rows = u16::from(inner.height >= 2);
    let chip_rows = u16::from(chips.is_some() && inner.height >= 3);
    let input = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2),
        height: inner.height - hint_rows - chip_rows, ..inner };
    // … draft drawn as today …
    if let Some(chips) = chips.filter(|_| chip_rows == 1) {
        frame.render_widget(
            Paragraph::new(chips).style(Style::default().fg(ACCENT)),
            Rect { y: input.bottom(), height: 1, ..input },
        );
    }
    let (hint, color) = composer_hint(app, width);
    frame.render_widget(Paragraph::new(hint).style(Style::default().fg(color)),
        Rect { x: input.x, y: inner.bottom().saturating_sub(1), width: input.width, height: hint_rows });
```

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: the composer's hint says what Enter does now; chips have their own row

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 8: The command palette filters as you type

**Files:**

- Modify: `crates/octet-tui/src/app.rs` (`Overlays::palette_query`)
- Modify: `crates/octet-tui/src/view.rs` (`palette`, new `palette_matches`)
- Modify: `crates/octet-tui/src/input.rs` (`palette_press`; Ctrl+P clears the filter)
- Test: `crates/octet-tui/src/tests/keys.rs`, `crates/octet-tui/src/view/tests.rs`

**Interfaces:**

- Produces: `Overlays::palette_query: String`;
  `view::palette_matches(query: &str) -> Vec<usize>` (indices into `palette_entries()`, in order).

- [ ] **Step 1: Failing tests**

```rust
// tests/keys.rs
#[tokio::test]
async fn typing_filters_the_palette_and_enter_runs_the_match() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    key_action(&mut app, &vendor, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)).await;
    type_keys(&mut app, &vendor, "edit").await;
    assert_eq!(app.overlay.palette_query, "edit");
    let names: Vec<&str> = view::palette_matches(&app.overlay.palette_query)
        .into_iter()
        .map(|i| view::palette_entries().nth(i).unwrap().0)
        .collect();
    assert!(names.contains(&"Ctrl+G"), "{names:?}");
    assert!(names.iter().all(|n| {
        let (name, summary) = view::palette_entries().find(|(k, _)| k == n).unwrap();
        format!("{name} {summary}").to_lowercase().contains("edit")
    }));
    key_action(&mut app, &vendor, key(KeyCode::Backspace)).await;
    assert_eq!(app.overlay.palette_query, "edi");
    assert_eq!(app.overlay.selection, 0, "a change selects the first match");
}
#[tokio::test]
async fn opening_the_palette_starts_unfiltered() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.overlay.palette_query = "zz".into();
    key_action(&mut app, &vendor, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)).await;
    assert!(app.overlay.palette_query.is_empty());
}
// view/tests.rs
#[test]
fn a_palette_with_no_match_says_so() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.overlay.palette = true;
    app.overlay.palette_query = "qqqq".into();
    let rows = screen(100, 30, &mut app);
    assert!(rows.iter().any(|r| r.contains("No matching commands")));
    assert!(rows.iter().any(|r| r.contains("Commands · \"qqqq\"")));
}
```

(Ctrl+G's summary is "Write the prompt in $EDITOR": "edit" matches through "$EDITOR".)
Run: `scripts/rust-env.sh cargo test -p octet-tui palette` — Expected: FAIL.

- [ ] **Step 2: Implement**

`app.rs`: `Overlays` gains `pub(crate) palette_query: String` (doc: "What the palette is filtered
by"), empty in `App::new`.

`view.rs`:

```rust
/// The palette rows whose name or summary contains `query`, ignoring case.
pub(crate) fn palette_matches(query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    palette_entries()
        .enumerate()
        .filter(|(_, (name, summary))| format!("{name} {summary}").to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}
```

`palette(frame, area, selected, query)`: the modal keeps its full-list size (so it does not jump
as you type); lines come from `palette_matches(query)` (`selected` indexes into the matches); an
empty match list draws `"   No matching commands"` in `MUTED`; the title is
`" Commands · ↑ ↓ choose · Enter run · Esc close "` when `query` is empty, else
`format!(" Commands · \"{query}\" · ↑ ↓ · Enter run · Esc close ")`. `overlays` passes
`&app.overlay.palette_query`.

`input.rs` `palette_press`:

```rust
async fn palette_press(app: &mut App, vendor: &dyn Vendor, key: KeyEvent) -> Action {
    let ctrl_c = ctrl(key, 'c');
    let matches = view::palette_matches(&app.overlay.palette_query);
    match key.code {
        _ if ctrl_c => app.overlay.palette = false,
        KeyCode::Esc => app.overlay.palette = false,
        KeyCode::Up => app.overlay.selection = app.overlay.selection.saturating_sub(1),
        KeyCode::Down => app.overlay.selection = (app.overlay.selection + 1).min(matches.len().saturating_sub(1)),
        KeyCode::Backspace => {
            app.overlay.palette_query.pop();
            app.overlay.selection = 0;
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.overlay.palette_query.push(c);
            app.overlay.selection = 0;
        }
        KeyCode::Enter => {
            let Some(&index) = matches.get(app.overlay.selection) else {
                return Action::Continue;
            };
            app.overlay.palette = false;
            if let Some(spec) = COMMANDS.get(index) {
                return command(app, vendor, spec.name).await;
            }
            return palette_key(app, view::PALETTE_KEYS[index - COMMANDS.len()].0);
        }
        _ => {}
    }
    Action::Continue
}
```

`composer_key` Ctrl+P: `KeyCode::Char('p') if ctrl => { app.overlay.palette = true; app.overlay.palette_query.clear(); app.overlay.selection = 0; }`.
The existing `the_palette_offers_the_editor_mentions_and_shell` test still holds: with an empty
query, match indices equal row indices.

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: typing filters the command palette

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 9: The Task sidebar, and F2

**Files:**

- Modify: `crates/octet-tui/src/app.rs` (`App::sidebar: bool`, new `now_line`)
- Modify: `crates/octet-tui/src/view.rs` (`conversation`, `sidebar`, `help_lines`)
- Modify: `crates/octet-tui/src/input.rs` (`composer_key` F2)
- Test: `crates/octet-tui/src/view/tests.rs`, `crates/octet-tui/src/app/tests.rs`

**Interfaces:**

- Consumes: `App::running_tool`, `transcript::label` (Task 3).
- Produces: `App::sidebar: bool` (shown; `true` in `App::new`); `App::now_line(&self) -> String`.

- [ ] **Step 1: Failing tests**

```rust
// app/tests.rs
#[test]
fn now_says_what_is_happening() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert_eq!(app.now_line(), "Connecting");
    crate::test_support::idle(&mut app);
    assert_eq!(app.now_line(), "Idle");
    app.event(Event::Started);
    assert_eq!(app.now_line(), "Thinking");
    app.event(Event::Text("x".into()));
    assert_eq!(app.now_line(), "Writing reply");
    app.event(crate::test_support::tool("1", "commandExecution", "cargo test", octet_core::ToolPhase::Running));
    assert_eq!(app.now_line(), "Running Bash cargo test");
    app.event(crate::test_support::approval(1, "{}"));
    assert_eq!(app.now_line(), "Waiting for approval");
}
#[test]
fn now_ignores_stopped_calls() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    crate::test_support::idle(&mut app);
    app.event(Event::Started);
    app.event(crate::test_support::tool("1", "Bash", "sleep 9", octet_core::ToolPhase::Running));
    app.event(Event::Finished { outcome: octet_core::Outcome::Interrupted });
    assert_eq!(app.now_line(), "Idle");
}
// view/tests.rs — replaces sidebar_lines_fit_the_panel_without_wrapping
#[test]
fn the_task_sidebar_shows_goal_now_and_queue() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    crate::test_support::idle(&mut app);
    app.composer.queue.push_back(crate::vendor::Prompt::plain("then add tests\nmore".into()));
    let rows = screen(120, 40, &mut app);
    let text = rows.join("\n");
    assert!(text.contains("╭ Task"), "{text}");
    for expected in ["GOAL", "No goal · /goal sets one", "NOW", "Idle", "QUEUED", "then add tests"] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    for gone in ["ENGINE", "SESSION", "QUICK COMMANDS"] {
        assert!(!text.contains(gone), "{gone}");
    }
    app.sidebar = false;
    let rows = screen(120, 40, &mut app);
    assert!(!rows.join("\n").contains("╭ Task"));
}
```

Add a goal case: set a goal through `app.goals.set_goal(Some(octet_core::goal::Goal::new("Make every test pass on Linux and macOS").unwrap()))`
and assert the objective's first words and `/20 turns` (use `MAX_GOAL_TURNS`) appear. Add a key
test in `tests/keys.rs`: F2 flips `app.sidebar`.

Run: `scripts/rust-env.sh cargo test -p octet-tui now_ the_task_sidebar f2` — Expected: FAIL.

- [ ] **Step 2: Implement**

`app.rs`:

```rust
    /// What is happening now, for the sidebar.
    pub(crate) fn now_line(&self) -> String {
        if !self.overlay.approvals.is_empty() {
            return "Waiting for approval".into();
        }
        match self.conn.phase {
            ConnPhase::Connecting => "Connecting".into(),
            ConnPhase::Stopped => "Stopped".into(),
            ConnPhase::Idle => "Idle".into(),
            ConnPhase::Running { .. } => match self.running_tool() {
                Some(call) => format!("Running {} {}", crate::view::transcript::label(&call.name), call.target)
                    .trim_end()
                    .to_owned(),
                None if self.chat.entries.back().is_some_and(|e| e.role == Role::Assistant) => "Writing reply".into(),
                None => "Thinking".into(),
            },
        }
    }
```

`view.rs` `conversation`: `if width >= 112 && app.sidebar { … }`. `sidebar`:

```rust
fn sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let block = card(" Task ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width.saturating_sub(1)).max(1);
    let head = |text: &'static str| Line::from(Span::styled(format!(" {text}"), Style::default().fg(MUTED)));
    let body = |text: String| Line::from(format!(" {text}"));
    let mut lines = vec![Line::default(), head("GOAL")];
    match app.goals.goal() {
        Some(goal) => {
            let mut rows = transcript::wrap(goal.objective(), width);
            if rows.len() > 4 {
                rows.truncate(4);
                rows[3].push('…');
            }
            lines.extend(rows.into_iter().map(body));
            lines.push(Line::from(Span::styled(
                format!(" {} · {}/{} turns", goal.status(), goal.turns(), octet_core::goal::MAX_GOAL_TURNS),
                Style::default().fg(MUTED),
            )));
        }
        None => lines.push(body("No goal · /goal sets one".into())),
    }
    lines.extend([Line::default(), head("NOW")]);
    lines.extend(transcript::wrap(&app.now_line(), width).into_iter().take(2).map(body));
    lines.extend([Line::default(), head("QUEUED")]);
    if app.composer.queue.is_empty() {
        lines.push(body("Nothing queued".into()));
    }
    for prompt in app.composer.queue.iter().take(3) {
        let first = prompt.display().lines().next().unwrap_or_default();
        lines.push(body(transcript::wrap(first, width).swap_remove(0)));
    }
    if app.composer.queue.len() > 3 {
        lines.push(body(format!("+{} more", app.composer.queue.len() - 3)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
```

Remove the now-unused `COMMANDS` quick-command use from `sidebar` (keep the import if help still
uses it). `input.rs` `composer_key`: `KeyCode::F(2) => app.sidebar = !app.sidebar,`. `help_lines`
keys: add `"F2 show or hide the Task sidebar"`. If `registry::Spec::quick` is now unused, delete
the field and its values (cargo will warn); check `grep -n "quick" crates/octet-tui/src -r`.

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: the sidebar shows the task (goal, now, queue); F2 hides it

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 10: Compact layout under 30 rows

**Files:**

- Modify: `crates/octet-tui/src/view.rs` (`paint`, new `compact`, `header_line`, `mark_color`; `status_line`)
- Test: `crates/octet-tui/src/view/tests.rs` (new tests; update the header tests)

**Interfaces:**

- Produces: `view::COMPACT_BELOW: u16 = 30`; `fn compact(area: Rect) -> bool`.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn a_short_terminal_gets_a_one_row_header_and_no_vertical_margin() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = false;
    crate::test_support::idle(&mut app);
    app.event(Event::User("hi".into()));
    let rows = screen(100, 24, &mut app);
    assert!(rows[0].contains("Octet · ask"), "{}", rows[0]);
    assert!(rows[0].contains("● ready"), "{}", rows[0]);
    assert!(rows[0].contains("/tmp"), "{}", rows[0]);
    assert!(rows[1].contains("╭ Conversation"), "{}", rows[1]);
    // The demo is offline, so its standing hints say so.
    assert_eq!(rows[23].trim_end(), " Offline demo · F1 help · Ctrl+C ×2 quit");
    let transcript_rows = rows.iter().filter(|r| r.starts_with(" │")).count();
    assert!(transcript_rows >= 16, "{transcript_rows}\n{}", rows.join("\n"));
}
#[test]
fn a_tall_terminal_keeps_the_banner() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = false;
    let rows = screen(100, 30, &mut app);
    let mini: String = rows[1].chars().skip(1).take(9).collect();
    assert_eq!(mini, "▀".repeat(9));
}
#[test]
fn a_narrow_short_header_puts_the_workspace_before_the_model() {
    let config = octet_core::Config { model: Some("a-very-long-model-name-indeed".into()),
        ..octet_core::Config::new(octet_core::Engine::CODEX, "codex", "/tmp/ws") };
    let mut app = App::new(&config, "journal".into());
    let rows = screen(60, 20, &mut app);
    let ws = rows[0].find("/tmp/ws").expect(&rows[0]);
    assert!(rows[0].find("a-very").is_none_or(|model| ws < model), "{}", rows[0]);
}
```

Update older tests: `header_banner_shows_mini_version_mode_activity_and_workspace` keeps only
`(160, 42)` and `(80, 30)`; `permission_mode_stays_visible_on_narrow_headers` checks `rows[0]`
(height 24 is compact); `status_shows_in_full_on_narrow_terminals` checks `rows[0]`;
`an_empty_composer_gives_rows_back_to_the_conversation` expects the composer top border at
`rows[16 - 5]` (no bottom margin). Run each changed test before the implementation to see it fail.

Run: `scripts/rust-env.sh cargo test -p octet-tui a_short_terminal a_tall_terminal a_narrow_short header_ permission_mode status_shows an_empty_composer`
Expected: FAIL.

- [ ] **Step 2: Implement**

```rust
/// Below this many rows the header is one row and the layout has no vertical margin.
pub(crate) const COMPACT_BELOW: u16 = 30;
/// Short terminals trade the banner for transcript rows.
fn compact(area: Rect) -> bool {
    area.height < COMPACT_BELOW
}
/// The one-cell mark's colour: the mascot's state at a glance.
fn mark_color(state: mascot::State) -> Color {
    match state {
        mascot::State::Approval | mascot::State::Error => AMBER,
        mascot::State::Idle | mascot::State::Sleeping => MUTED,
        _ => ACCENT,
    }
}
/// The compact header: mark, name, mode, model and workspace on the left; status and usage right.
fn header_line(frame: &mut Frame, area: Rect, app: &App) {
    let status = status_text(app);
    let right = if app.conn.usage.is_empty() { status } else { format!("{status} · {}", app.conn.usage) };
    let right_width = cells(right.width()).min(area.width / 2);
    let [left, right_area] = Layout::horizontal([Constraint::Min(6), Constraint::Length(right_width)])
        .spacing(1)
        .areas(area);
    let mark = if app.monochrome { "◇" } else { "◆" };
    let model = format!("{} / {}", app.conn.engine, app.conn.model);
    let (first, second) = if area.width < 80 { (&app.conn.workspace, &model) } else { (&model, &app.conn.workspace) };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(mark_color(app.mascot_state()))),
            Span::styled("Octet", Style::default().fg(ACCENT).bold()),
            Span::styled(" · ", Style::default().fg(MUTED)),
            mode_chip(app),
            Span::styled(format!(" · {first}  {second}"), Style::default().fg(MUTED)),
        ])),
        left,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(right, Style::default().fg(status_color(app)))).alignment(Alignment::Right),
        right_area,
    );
}
```

Extract `status_text(app) -> String` and `status_color(app) -> Color` from `header` (today's
`status` string and `status_color` logic) so both headers share them. `paint`:

```rust
    let compact = compact(area);
    let regions = Layout::vertical([
        Constraint::Length(if compact { 1 } else { 3 }),
        Constraint::Min(3),
        Constraint::Length(composer_height(draft.0.len(), chips(app).is_some())),
        Constraint::Length(1),
    ])
    .horizontal_margin(1)
    .vertical_margin(u16::from(!compact))
    .split(area);
    if compact {
        header_line(frame, regions[0], app);
    } else {
        header(frame, regions[0], app);
    }
```

`status_line` takes `compact: bool`; with an empty status line it draws
`" Offline demo · F1 help · Ctrl+C ×2 quit"` (offline) or `" F1 help · Ctrl+C ×2 quit"` when
compact, today's strings otherwise. The monochrome path keeps working (`without_colour` resets the
mark's colour; `◇` is drawn).

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: under 30 rows, a one-row header and no vertical margin give the transcript the room

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 11: Scrollback stays still and says new output is below

**Files:**

- Modify: `crates/octet-tui/src/app.rs` (`Transcript::{anchor, unseen}`, `follow_latest`, `toggle_tool_details`, `Event::User`)
- Modify: `crates/octet-tui/src/view/transcript.rs` (`visible_lines`, new `anchor_scroll`)
- Modify: `crates/octet-tui/src/view.rs` (`conversation` card titles)
- Test: `crates/octet-tui/src/view/tests.rs`

**Interfaces:**

- Consumes: `Entry::seq`, `Transcript::next_seq` (Task 3).
- Produces: `pub(crate) struct Anchor { seq: u64, rows: usize, width: u16, added: usize }`;
  `Transcript::anchor: Option<Anchor>`, `Transcript::unseen: bool`.

- [ ] **Step 1: Failing tests**

```rust
fn stream_lines(app: &mut App, n: usize) {
    for i in 0..n {
        app.event(Event::Text(format!("line {i}\n")));
    }
}
#[test]
fn scrolled_up_the_view_stays_still_while_output_arrives() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    stream_lines(&mut app, 60);
    app.scroll_by(20);
    let before: Vec<String> = app.visible_lines(80, 10).iter().map(ToString::to_string).collect();
    stream_lines(&mut app, 5);
    let after: Vec<String> = app.visible_lines(80, 10).iter().map(ToString::to_string).collect();
    assert_eq!(before, after);
    assert!(app.chat.unseen);
    let rows = screen(100, 30, &mut app);
    assert!(rows.iter().any(|r| r.contains("New output below · Ctrl+End follows")), "{}", rows.join("\n"));
    app.follow_latest();
    assert!(!app.chat.unseen);
    let rows = screen(100, 30, &mut app);
    assert!(!rows.iter().any(|r| r.contains("New output below")));
}
#[test]
fn resizing_while_scrolled_reanchors() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    stream_lines(&mut app, 60);
    app.scroll_by(20);
    let _ = app.visible_lines(80, 10);
    let scroll = app.chat.scroll;
    let _ = app.visible_lines(50, 10);
    assert_eq!(app.chat.scroll, scroll, "a new width re-anchors without moving");
    stream_lines(&mut app, 2);
    let _ = app.visible_lines(50, 10);
    assert!(app.chat.scroll > scroll);
    // The anchor entry is trimmed away: no panic, still bounded.
    app.chat.entries.pop_front();
    let _ = app.visible_lines(50, 10);
}
```

Run: `scripts/rust-env.sh cargo test -p octet-tui scrolled_up_the_view resizing_while`
Expected: FAIL.

- [ ] **Step 2: Implement**

`app.rs`: `Anchor` (doc: "Where the reader was when they scrolled up: the newest entry, its rows
then, the width, and the rows added below since") and the two `Transcript` fields (`None`/`false`).
`follow_latest` and the `Event::User` arm also set `anchor = None; unseen = false`.
`toggle_tool_details` and the `catalog_focus` branch of `visible_lines` set `anchor = None`
(their rows change on purpose).

`transcript.rs`, at the start of `visible_lines` (after the `catalog_focus` block):

```rust
        self.anchor_scroll(width);
```

and:

```rust
    /// Scrolled up, rows added below push the scroll by as many, so the view stays still.
    fn anchor_scroll(&mut self, width: u16) {
        if self.chat.scroll == 0 {
            self.chat.anchor = None;
            self.chat.unseen = false;
            return;
        }
        let anchor = match self.chat.anchor {
            Some(anchor) if anchor.width == width => anchor,
            _ => {
                // New, or a new width: anchor here without moving.
                self.chat.anchor = self.chat.entries.back_mut().map(|entry| crate::app::Anchor {
                    seq: entry.seq,
                    rows: entry.rows(width).len(),
                    width,
                    added: 0,
                });
                return;
            }
        };
        let mut grown = 0usize;
        for entry in self.chat.entries.iter_mut().rev() {
            if entry.seq < anchor.seq {
                break;
            }
            let rows = entry.rows(width).len();
            grown += if entry.seq == anchor.seq { rows.saturating_sub(anchor.rows) } else { rows };
        }
        if grown > anchor.added {
            self.chat.scroll = (self.chat.scroll + grown - anchor.added).min(MAX_SCROLL);
            self.chat.unseen = true;
            if let Some(kept) = &mut self.chat.anchor {
                kept.added = grown;
            }
        }
    }
```

(make `MAX_SCROLL` `pub(crate)` in `app.rs`).

`view.rs` `conversation`: the card is always `card(" Conversation ")`; when `app.chat.unseen`,
add `.title_bottom(Line::from(Span::styled(" New output below · Ctrl+End follows ", Style::default().fg(ACCENT))))`.
The ` · scrollback ` title goes.

- [ ] **Step 3: Run, gate, commit**

Run: `scripts/rust-env.sh cargo test -p octet-tui` — Expected: PASS.

```bash
if make rust-check; then
  git add crates/octet-tui/src
  git commit -m "TUI: scrollback stays still while output arrives, and says new output is below

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 12: Docs, live checks, Linux

**Files:**

- Modify: `docs/tui.md`, `docs/rust/tui-features.md`, `README.md` (key lists, screens), `CHANGELOG.md`
- Check only: `docs/rust/parity-matrix.md`, `docs/remote-control.md` (fix any key or screen they name that changed)

- [ ] **Step 1: Find every stale mention**

Run: `grep -rn "Enter runs\|Workspace\b\|QUICK\|scrollback\|Request details\|Enter queues\|Tab inserts\|is a .* skill" docs README.md | grep -v superpowers`
Update each to the new behaviour: Ctrl+O tool details; F2 Task sidebar; palette filtering; skills
Enter prepares, another provider's skill switches when sent (with the amber hint); the approval
dialog's lead; compact layout under 30 rows; "New output below · Ctrl+End follows".

- [ ] **Step 2: CHANGELOG**

Under `### Added` / `### Changed` in `[0.1.0] - Unreleased`, one bullet each for: typed tool and
approval events (journal fields `tool.{id,name,target,phase,detail}`, `approval.summary`);
grouped tool activity and Ctrl+O; the approval lead; one Enter hint and the chips row; the Task
sidebar and F2; palette filtering; skills prepared by Enter (and change the existing "Enter runs
one" bullet); the compact layout; still scrollback.

- [ ] **Step 3: Live checks with the fake vendors (tmux)**

Build: `scripts/rust-env.sh cargo build --release -p octet && scripts/rust-env.sh cargo build -p octet-testkit --bins`.
In a private tmux server (`tmux -L octet-clarity`), run
`target/release/octet --engine codex --binary target/debug/protocol-child --journal-dir <scratch>`
at 100 × 24 and 120 × 40, and check by `capture-pane -p`:

1. `tool-fail`: a `✗ Bash false` line with `boom` under it; Ctrl+O shows `… Bash false` → `✗`.
2. `file-approval`: dialog shows `Edit 2 files`, `a.rs`, `b.rs`, `Allows: This request only, once`, keys row; `D` denies.
3. `slow` with PgUp mid-turn: the text stays put and `New output below · Ctrl+End follows` shows; Ctrl+End clears it.
4. During `hold`: hint row `Enter queue · Esc cancel turn · Ctrl+P commands`; type and Enter → chips row `1 queued`; sidebar QUEUED lists it; Esc.
5. Ctrl+P, type `mod`: the list narrows; Esc.
6. At 100 × 24 the header is one row; at 120 × 40 the banner is three rows and the Task sidebar shows; F2 hides it.

Repeat 1 with `--engine claude` (`tool-fail`). Wait for turns by counting `finished` records in
the journal, not by the screen.

- [ ] **Step 4: Linux**

Run: `scripts/rust-env.sh cargo clippy --locked --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`
Expected: PASS. Then, with Docker Desktop started (`open -a Docker`):
`docker run --rm -v "$PWD":/w -w /w -v octet-linux-target:/target -v octet-linux-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target rust:1.98.1 bash -c 'cargo test --locked --workspace'`
Expected: PASS. Quit Docker afterwards: `osascript -e 'quit app "Docker"'`.

- [ ] **Step 5: Gate and commit**

```bash
if make rust-check; then
  git add docs/tui.md docs/rust/tui-features.md README.md CHANGELOG.md docs/rust/parity-matrix.md docs/remote-control.md
  git commit -m "Docs: grouped tools, the approval lead, the Task sidebar, palette filtering, skills and the compact layout

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```
