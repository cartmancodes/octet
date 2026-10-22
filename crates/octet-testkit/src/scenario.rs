//! The fake vendor's scripts, named once for `protocol-child` and the tests
//! that drive it. A prompt with one of these texts (or, where noted, a
//! `--model` value or launch argument) runs that script.

/// Codex, Claude and opencode: hold the turn open until interrupted.
pub const HOLD: &str = "hold";
/// Codex: hold the turn, after a text marker that the turn is named.
pub const HOLD_MARKED: &str = "hold-marked";
/// Codex: name the turn late, so steers are held and cancels wait.
pub const LATE_START: &str = "late-start";
/// Codex: refuse `turn/start` after a pause.
pub const REFUSE_START: &str = "refuse-start";
/// Codex: hold the turn and exit when asked to interrupt it.
pub const DIE_ON_INTERRUPT: &str = "die-on-interrupt";
/// Codex, Claude and opencode: ask for one approval and echo the answer.
pub const APPROVAL: &str = "approval";
/// Codex, Claude and opencode: ask for nine approvals at once.
pub const APPROVALS_9: &str = "approvals-9";
/// Codex: a file change starts, then asks for approval.
pub const FILE_APPROVAL: &str = "file-approval";
/// Claude: ask for an approval, then cancel the request.
pub const APPROVAL_CANCEL: &str = "approval-cancel";
/// Codex: a tool with large output.
pub const BIGTOOL: &str = "bigtool";
/// Codex: output for another thread that must not show, and no turn end.
/// opencode: the same, for another session.
pub const CHATTER: &str = "chatter";
/// Codex: a failed turn (usage limit). opencode: an error reply to the prompt.
pub const FAIL: &str = "fail";
/// Codex: a failed turn whose message is nested JSON.
pub const FAIL_NESTED: &str = "fail-nested";
/// Codex: the skills change mid-turn (`skills/changed`), then a normal reply.
pub const SKILLS_CHANGED: &str = "skills-changed";
/// Codex: a slow turn of dots.
pub const SLOW: &str = "slow";
/// Codex: echo the thread and turn parameters received. opencode: echo the
/// launch arguments, environment, model, modes and how the session was opened.
pub const PARAMS: &str = "params";
/// Claude: one tool use.
pub const TOOL: &str = "tool";
/// Codex, Claude and opencode: one tool whose result is an error.
pub const TOOL_FAIL: &str = "tool-fail";
/// Claude: a failed result with error details.
pub const ERRORS: &str = "errors";
/// Claude: echo the launch arguments.
pub const ARGV: &str = "argv";
/// Claude: echo the permission modes requested so far.
pub const MODES: &str = "modes";
/// Claude: answer held mode switches (with `HANG_MODE`).
pub const FLUSH: &str = "flush";
/// Claude: a result without `is_error`.
pub const NO_IS_ERROR: &str = "no-is-error";
/// Claude: 200 one-character deltas, then the result.
pub const DELTA_BURST: &str = "delta-burst";
/// Claude: 3000 deltas of 100 characters, far more than a stalled reader
/// can take.
pub const DELTA_FLOOD: &str = "delta-flood";
/// Claude: a main reply, then a subagent's (Task tool) messages.
pub const SUBAGENT: &str = "subagent";
/// Claude: a result whose session ID holds an escape sequence.
pub const ODD_SESSION: &str = "odd-session";
/// Claude: a local command's reply, whose model is the placeholder `<synthetic>`.
pub const SYNTHETIC: &str = "synthetic";
/// Codex: a huge unknown server request, then a huge unknown turn status.
pub const ODD_STRINGS: &str = "odd-strings";
/// Codex: hold the turn and acknowledge interrupts without ending it.
pub const IGNORE_INTERRUPT: &str = "ignore-interrupt";

/// Claude launch argument (`--model`): exit at the handshake with stderr.
pub const DIE_STDERR: &str = "die-stderr";
/// Claude launch argument: report `auto` at the handshake.
pub const REPORT_AUTO: &str = "report-auto";
/// Claude launch argument: report `plan` at the handshake.
pub const REPORT_PLAN: &str = "report-plan";
/// Claude launch argument: hold mode switches until `FLUSH`.
pub const HANG_MODE: &str = "hang-mode";
/// Claude launch argument: confirm every switch as `plan`.
pub const ODD_MODE: &str = "odd-mode";
/// Claude launch argument: confirm switches after 2.5 seconds.
pub const LATE_MODE: &str = "late-mode";
/// Claude launch argument: refuse every switch.
pub const REJECT_MODE: &str = "reject-mode";
/// Claude launch argument: run a `SessionStart` hook before the handshake's
/// reply, whose frames name the session, as Claude does with a plugin's hook.
pub const START_HOOK: &str = "start-hook";

/// Codex `--model`: refuse to open the thread.
pub const REFUSE_THREAD: &str = "refuse-thread";
/// Codex `--model`: report a stricter policy than asked for.
pub const REPORT_STRICTER: &str = "report-stricter";

/// The reply the fake Codex gives a prompt with no script.
pub const CODEX_REPLY: &str = "Hello fixture";
/// The reply the fake Claude gives a prompt with no script.
pub const CLAUDE_REPLY: &str = "Hello Claude";
/// The reply the fake opencode gives a prompt with no script.
pub const OPENCODE_REPLY: &str = "Hello opencode";

/// opencode model value: once set, every later mode switch is refused with
/// "unknown agent" (the first one, right after the model, still succeeds).
pub const OPENCODE_REJECT_MODE: &str = "reject-mode";
/// opencode model value: offers the same effort levels as the free model,
/// and each `effort` or `mode` request writes `asked-<option>` in the
/// working directory, then waits for `release-<option>` there before its
/// reply, so a test can act while the request is in flight.
pub const OPENCODE_GATED: &str = "gated-options";
/// opencode: hold the turn while another session asks for a permission;
/// the answer is echoed and ends the turn.
pub const OPENCODE_OTHER_PERMISSION: &str = "other-permission";
/// opencode: hold the turn, and ask for a permission when it is cancelled;
/// the answer is echoed and ends the turn.
pub const OPENCODE_PERMISSION_ON_CANCEL: &str = "permission-on-cancel";
/// opencode: the name of the skill the fake lists as a slash command.
pub const OPENCODE_SKILL: &str = "fixture-opencode-skill";

/// Goal objective (`/goal …`): one step, then completion with evidence.
pub const GOAL: &str = "fixture-goal";
/// Goal objective: hold each goal turn open until interrupted.
pub const GOAL_HOLD: &str = "fixture-hold";
/// Goal objective: fail each goal turn.
pub const GOAL_FAIL: &str = "fixture-fail";
/// How every goal prompt starts; `octet_core::goal::PROMPT_PREFIX` must
/// match (a test in octet-core pins it).
pub const GOAL_PROMPT_PREFIX: &str = "Octet active goal: ";

/// Whether `text` is the goal prompt for `objective`.
pub fn is_goal_prompt(text: &str, objective: &str) -> bool {
    text.strip_prefix(GOAL_PROMPT_PREFIX)
        .and_then(|rest| rest.strip_prefix(objective))
        .is_some_and(|rest| rest.starts_with('\n'))
}

/// Claude: the request ID of the `APPROVAL` script's permission request.
pub const APPROVAL_ID: &str = "perm-1";
/// Claude: the request ID `APPROVAL_CANCEL` asks for and then cancels.
pub const CANCELLED_APPROVAL_ID: &str = "perm-2";
/// Codex and Claude: `APPROVALS_9` numbers its requests `cap-1` … `cap-9`.
pub const CAP_PREFIX: &str = "cap-";
