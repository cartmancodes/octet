# TUI clarity: approvals, tool activity, composer, sidebar, skills, compact layout: design

**Date:** 2026-10-12. **Branch:** `feat/tui-clarity`.
**Source:** a design review of the terminal preview and the rendering and input code
(`output/octet-tui-review.html`, untracked). It is a design review, not a usability test.

## The problem

Octet's crimson identity and mascot stay. What changes is how the screen uses space and says what is
happening and what a key will do.

1. **Approvals.** The dialog spends most of its room on the vendor's raw request (pretty JSON). The
   action, command, files, folder and what the grant covers are not stated. Headless mode's denial
   warning prints the request's first line, which is `{`.
2. **Composer hints contradict.** During a turn the title says "Enter queues it" while the hint row
   still says "Enter send". Attachments and the queued count sit in the title, where a long title
   hides them.
3. **Tool activity crowds the reply.** Every tool call is its own entry with a `TOOL` label, a blank
   line and its full preview. Claude reports only a tool's input, never its result, so a failed call
   looks the same as one that worked.
4. **The sidebar repeats the header.** Engine and model are in the header already; the session ID
   wraps; quick commands and keys are in help. The goal shows a status, not its objective; the
   current activity and the queue are not shown.
5. **Skills run by accident.** Enter in the skills picker runs the skill at once, and for another
   provider's skill switches provider and runs it, with no chance to add arguments. The command
   palette cannot be filtered.
6. **Short terminals lose the transcript first.** The header takes 3 rows and the margins 2. In
   scrollback, streaming text moves what is being read, and nothing says output is still arriving.

## Goal

- An approval leads with what it would do, where, and what allowing it grants; the full request stays
  below, scrollable. The guard against answer keys typed as it opens stays.
- One instruction for Enter, matching the state: send when idle, queue while a turn runs, switch
  provider when the draft names another provider's skill. Chips have their own row.
- Routine tool calls collapse into one summary line per run of calls ("Read 3 files · Bash 2");
  running calls show progress; failures stay expanded. Ctrl+O shows every call in full.
- The sidebar shows the task: the goal's objective, what is happening now, and the queue. F2 hides it.
- The palette filters as you type. Enter on a skill prepares `/name ` in the draft; a second Enter
  sends it. Switching provider is announced before Enter does it.
- Under 30 rows, a one-row header and no vertical margin give the transcript the room. Scrollback
  stays still while output arrives, and says "New output below · Ctrl+End follows".

## Non-goals

- New grants ("allow for this session", "always allow"): the dialog states the existing one.
- Per-group expand or a transcript focus mode: one global toggle.
- Reproducing the review's HTML mock pixel for pixel.
- Changing colours, the mascot or the minimum terminal size (38 × 12).

## Design

### 1. Typed tool and approval events (`octet-engine`, `octet-core`, `octet`)

The adapters already parse each vendor's wire format, so they fill typed fields; the interface does
no JSON parsing.

**Tool calls.** `Event::Tool(String)` becomes `Event::Tool(ToolCall)`:

```rust
pub struct ToolCall {
    /// The vendor's tool-use or item ID: pairs a call's start with its end.
    pub id: String,
    /// As the vendor names it: `Read`, `Bash`, `commandExecution`, `mcp__x__y`.
    pub name: String,
    /// One line naming its subject: a path, command, pattern or MCP tool; empty if none.
    pub target: String,
    pub phase: ToolPhase, // Running | Done | Failed
    /// The preview shown today: input, output, exit status; bounded as now.
    pub detail: String,
}
```

- **Claude.** A `tool_use` block emits `Running`. `target` is the first of `input.file_path`,
  `input.path`, `input.command`, `input.pattern`, `input.url` that is a string, else empty. The
  adapter now also reads `tool_result` blocks in `"user"` messages (ignored today): each emits
  `Done`, or `Failed` when `is_error` is true, with the block's `tool_use_id` and its text
  (bounded by `EVENT_BYTES` as other previews are) as `detail`. Subagent tool calls are emitted, as
  today.
- **Codex.** `item/started` emits `Running`; `item/completed` emits `Done`, or `Failed` when the
  item's status is `failed` or `declined` or its exit code is non-zero. `target` is the command, the
  first changed path, or the MCP tool's name. `detail` is today's `codex_tool_detail` text.

**Approvals.** `Event::Approval` gains `summary: ApprovalSummary`; `detail` keeps the full request.

```rust
pub struct ApprovalSummary {
    /// What it would do, as a sentence: "Run a shell command", "Edit 2 files",
    /// "Use MCP tool fixture_echo", "Use Read".
    pub action: String,
    pub command: Option<String>,
    pub files: Vec<String>,
    pub cwd: Option<String>,
    /// What allowing grants. Octet answers one request at a time:
    /// "This request only, once".
    pub scope: String,
}
```

- Claude: from `request.tool_name` and `request.input` (`command`, `file_path`, `path`, `edits`).
- Codex: from `params.command`, `params.cwd` and `params.changes` (file-change approvals).
- A field the request lacks stays `None` or empty.

**Other consumers.**

- **Journal** (`octet_core` event JSON): `tool` becomes `{id, name, target, phase, detail}`;
  `approval` gains `summary`. Only `octet-core` writes these and nothing parses them back, so old
  journals stay valid as text.
- **Headless:** the denial warning names `summary.action` (and the command when there is one).
- **Handoff:** a tool entry is written from `name`, `target` and `detail`.
- **Testkit:** the fake vendors gain a `tool_result` (one with `is_error: true`), a completed and a
  failed Codex item, and a file-change approval.

### 2. Grouped tool activity (`octet-tui` transcript)

- Consecutive tool events form one `Role::Tools` entry holding `Vec<ToolCall>`. A tool event joins
  the last entry when it is a tools entry; any other entry starts a new group. A `Done` or `Failed`
  replaces the call with the same `id`; one with no match is added. A call without an ID gets a
  synthetic one and is never paired.
- **Collapsed** (default):
  - one summary line: `▸ Read 3 files · Edit 1 · Bash 2   Ctrl+O details`. Counts by tool name in
    order of first appearance. Known tools in words (Read, Edit, Write, Grep, Glob, Bash,
    commandExecution → "Bash", fileChange → "Edit"); others as `Name ×N`.
  - each running call: `… Bash cargo test` (muted).
  - each failed call in full: `✗ Bash cargo test (exit 101)` in amber, then the last 8 lines of its
    detail, then `+N more lines · Ctrl+O` when cut.
- **Expanded:** each call as `✓ Read src/config.rs` / `✗ …` / `… …`, then its detail, muted.
- A tools entry has no `TOOL` label row or blank line before it; it sits under the reply it belongs
  to.
- When a turn finishes, calls still `Running` show `· stopped` (muted).
- **Ctrl+O** (unbound today) flips `chat.tools_expanded`, drops the cached rows of tools entries
  only, and says "Tool details shown" / "Tool details hidden" on the status line. Following stays at
  the bottom; a scrollback offset is clamped. Help and the palette list it.

### 3. Approval dialog

Same modal size and amber frame, reordered:

```
 Permission required · #3 · 2 pending
 Run a shell command
 $ cargo test --workspace
 in ~/Projects/LocalCode
 Allows: this request only, once
 ┌ Full request · PgUp/PgDn ───────────┐
 │ { "tool_name": "Bash", ...          │
 └─────────────────────────────────────┘
 A  Allow once     D / Esc  Deny     Ctrl+C  Cancel turn
```

- Files: up to 5, then `+N more`. Lines with nothing to show are left out.
- The full request takes the remaining height and scrolls as today; on a short screen the summary
  keeps its rows and the request box shrinks, down to none.
- `approval_armed` and the approval keys are unchanged.

### 4. Composer

- Title: ` Prompt ` when idle, ` Next prompt ` while a turn runs. No instructions in it.
- The hint row says what Enter does, by state:
  - idle: `Enter send · Alt+Enter newline · ↑ history · Ctrl+P commands`
  - running: `Enter queue · Esc cancel turn · Ctrl+P commands`
  - the draft names a skill only another provider has: `Enter switches to Codex, then runs /review`,
    amber.
  - under 80 columns: the first two items.
- A chips row, inside the box above the hints, only when there is something to show:
  `+ src/main.rs · + image shot.png · 2 queued`. The box is one row taller while it shows.

### 5. Task sidebar

The `Workspace` card becomes ` Task `:

- **GOAL:** the objective, wrapped, at most 4 lines (`…` when cut); then `active · 3/20 turns`. With
  none: `No goal · /goal sets one`.
- **NOW:** `Running Bash cargo test` (the latest running tool), `Writing reply`, `Thinking`,
  `Waiting for approval`, `Connecting`, `Idle`, `Stopped`, from the connection phase and the tools.
- **QUEUED:** the first line of up to 3 queued prompts, cut to width; `+N more`. With none:
  `Nothing queued`.

Engine, model and session leave it (header and `/session`); quick commands and keys leave it (F1).
**F2** shows or hides it for the session (not Ctrl+B: tmux's prefix). It shows by default from 112
columns; hidden, the transcript takes the width.

### 6. Command palette and skills

- **Palette:** typed characters filter it, case-insensitive, on name and summary. The title shows
  the filter: ` Commands · "mod" · ↑ ↓ · Enter run · Esc close `. Backspace edits it; any change
  selects the first match; with no match the list says `No matching commands`. Esc closes.
- **Skills picker:** Enter and Tab both put `/name ` in the draft and close the picker, for any
  provider's skill (Tab today refuses another provider's); the title becomes
  ` Skills · Enter choose `. Nothing runs until Enter sends the draft.
- **Typed and picked skills share one rule.** A name the current provider has runs there, as today.
  A draft `/name args` where only another provider has `name`: the composer's amber hint says so
  before Enter; Enter switches provider and runs it with
  `args`, through the existing `pending_skill` path (`SkillRun` with the arguments). This replaces
  today's "is a Codex skill; /skills runs it there" note. `can_reconnect` refusals still stop it.

### 7. Compact layout and scrollback

- **Compact** when the terminal is under 30 rows:
  - header: one row, `◆ Octet · Ask · claude / opus-4  ~/Projects/LocalCode   ● working · $0.12`.
    The mark is one crimson cell coloured by the mascot state (`◇` in monochrome). The mascot state
    label is left out; the status chip says it. Under 80 columns the workspace goes before the
    model.
  - vertical margin 0 (side margin unchanged).
  - standing status hints: ` F1 help · Ctrl+C ×2 quit`; notices still use the line.
  - At 24 rows the transcript gets 16 rows instead of 12.
- **Scrollback stays still:** `chat.scroll` counts rows from the bottom, so rows added below now add
  to it while it is above zero. The rows seen at the last frame are kept to measure that.
- **New output:** scrolled up with rows added since, the conversation card's bottom border says
  ` New output below · Ctrl+End follows ` in crimson; this replaces the ` · scrollback ` title.
  Ctrl+End or sending a prompt clears it.

## Error handling

- Missing tool IDs, unmatched ends and cancelled turns as in section 2.
- An approval summary with no command, files or folder shows the action and scope only.
- Every new string from a vendor passes through `clean()` before it is drawn.

## Testing

Test-first throughout.

- **Engine:** Claude `tool_use` → `Running` with each target field; `tool_result` → `Done` /
  `Failed`; Codex started/completed/failed/declined/non-zero exit; each approval summary field for
  both vendors; the journal JSON for `tool` and `approval`.
- **Headless:** the denial warning names the action.
- **TUI (`view/tests.rs`, `app/tests.rs`):** grouping and pairing; collapsed rows, failed calls
  expanded and cut at 8 lines; Ctrl+O re-wraps tools entries only; stopped calls; the approval
  summary lines and the shrinking request box; each composer hint state and the chips row; sidebar
  goal, now and queue; F2; palette filtering, Backspace, no match; skill Enter inserts; the
  cross-provider hint and switch with arguments; compact row budgets at 24 and 12 rows; scroll
  anchoring and the new-output border.
- **Live:** the tmux driver with fake vendors over each changed screen.
- **Linux:** lint and tests in Docker (`rust:1.98.1`) before calling it done.

## Phases

1. Typed events: engine, journal, headless, handoff, testkit.
2. Grouped tool activity and the approval dialog.
3. Composer and skills.
4. Sidebar and palette.
5. Compact layout and scrollback.
