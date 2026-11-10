# Antigravity Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `antigravity` a fourth Octet provider, driven over `agy acp`, so Gemini models (Flash first) work in Octet with a Google account, with the features Claude and Codex have wherever the CLI allows.

**Architecture:** One new protocol file (`crates/octet-engine/src/live/antigravity.rs`) and one row in `Engine::ALL`; the shared driver does the rest. The ACP helpers opencode already has move to a shared module (`live/acp.rs`) that both vendors call. Octet starts `agy acp --cli_dir_name <DIR>` against a settings file it writes itself, which pins how Antigravity asks; a workspace's `.agents/` configuration must be trusted before the CLI starts; a tripwire stops the session if a command or edit ever runs unasked in an asking mode.

**Tech Stack:** Rust (crates `octet-engine`, `octet-core`, `octet-tui`, `octet`, `octet-testkit`, `octet-gate`), tokio, serde_json, sha2; the Expo phone app (`apps/mobile`, TypeScript).

**Spec:** `docs/superpowers/specs/2026-11-10-antigravity-provider-design.md` (amended 2026-11-11: the "Checked while planning" facts and decisions 22–24).

## Global Constraints

- The provider's name is `antigravity`, its title `Antigravity`, its default binary `agy`. Verified version: `1.5.5`.
- Wire: JSON-RPC 2.0, one object per line; every frame Octet sends carries `"jsonrpc":"2.0"`. `initialize` sends `{"protocolVersion":1,"clientCapabilities":{}}`.
- Mode IDs: `ask` → `default`, `accept-edits` → `accept-edits`, `auto` → `auto`, `full-access` → `default` (with the `always-proceed` settings). Octet never uses Antigravity's `plan` mode and never calls `session/set_mode` or `session/fork`.
- Option IDs: `model`, `thought_level`, `mode`, all set with `session/set_config_option {sessionId, configId, value}`.
- Permission options: `allow` (`allow_once`) and `deny` (`reject_once`). Request IDs are strings; echo `id` as received.
- Octet never writes under `~/.gemini`, never reads vendor credentials, never answers an approval itself, and keeps every buffer bounded (`docs/rust/adding-a-provider.md`).
- opencode's behaviour and tests do not change. Claude's and Codex's do not either.
- Follow `.claude/skills/rust-engineer/SKILL.md`; match the surrounding code: doc comments on every item, `pub(super)` inside `live/`.
- Every task ends with `make rust-check` passing (fmt, clippy `-D warnings`, docs `-D warnings`, tests). A helper no code uses until a later task gets `#[cfg_attr(not(test), expect(dead_code, reason = "used by the protocol in a later task"))]`, removed in that task.
- Linux parity before the last task's commit (memory `linux-ci-parity`). The GitHub workflows stay disabled.
- No test, probe or gate run may add a session to the user's Antigravity history: real-CLI runs pass `--gemini-dir <temporary folder>`.
- `/mode` copy for Antigravity, verbatim, in `Mode::ALL` order:
  1. `"Antigravity asks before commands, edits and other actions; reads inside the workspace never ask; a subagent's requests never reach Octet (mode default)"`
  2. `"Edits proceed; commands and other actions ask; a subagent's requests never reach Octet (mode accept-edits)"`
  3. `"Antigravity's own checker and sandbox decide; it asks rarely (network and delete commands ran unasked when tested) (mode auto)"`
  4. `"No permission checks at all (toolPermission always-proceed)"`

## Review Focus

- **The user's settings file is damaged, huge, or has a `permissions` that is not an object:** the session still starts pinned, with one notice, and nothing of the damaged part is copied. Test in Task 3.
- **A trusted workspace's `.agents/` changes** (a file edited, a file added under `plugins/`, a file behind a link): Octet asks again. Test in Task 4.
- **`--effort high` with a model that has no thinking level:** a notice, no level sent, and the session connects. Test in Task 6.
- **Antigravity changes the model by itself** (a `config_option_update` naming another model, as on a quota fallback): the header follows it (`Event::ModelSelected`). Test in Task 8.
- **An edit begun under `accept-edits` ends after the user switched to `ask`:** the tripwire must not fire. Test in Task 8.

---

### Task 1: Shared ACP helpers

**Files:**
- Create: `crates/octet-engine/src/live/acp.rs`
- Modify: `crates/octet-engine/src/live/opencode.rs` (calls the shared functions; its own copies go)
- Modify: `crates/octet-engine/src/live/mod.rs` (`mod acp;`)

**Interfaces:**
- Produces (all `pub(super)` in `acp.rs`, moved from `opencode.rs` with their doc comments and unit tests, generalised only where shown):
  - `fn request(id: u64, method: &str, params: Value) -> Value` and `fn cancel(session: &str) -> Value` (the `session/cancel` notification).
  - `fn permission_reply(wire: &Value, allow: bool, fallback: (&str, &str)) -> Value`: opencode's `answer_wire`; `fallback` is the option ID used when the request lists no `allow_once` / `reject_once` option (opencode passes `("once", "reject")`).
  - `fn stray_reply(value: &Value, fallback: (&str, &str)) -> Option<Value>`: the body of `opencode_stray_reply`.
  - `fn client_capability(method: &str) -> bool`, `fn error_text(error: &Value) -> String`.
  - `fn option<'a>(config_options: &'a Value, id: &str) -> &'a Value`, `fn current<'a>(config_options: &'a Value, id: &str) -> Option<&'a str>`.
  - `fn catalog(config_options: &Value) -> Vec<ModelInfo>`, `fn commands(list: &Value) -> Vec<SkillInfo>`, `fn content_text(content: &Value) -> String`, `fn frame_target(update: &Value) -> Option<String>`.
  - `fn approval(tool_call: &Value, known: Option<&str>, vendor: &str) -> (String, ApprovalSummary)`: `vendor` replaces the word `opencode` in `"Read a file opencode did not name"`.
  - `async fn image_blocks(core: &mut Core, vendor: &str, images: &[ImageAttachment]) -> Result<Option<Vec<Value>>, DriverError>`: the image half of opencode's `send_blocks` (read with `limits.image_read`, `check_inline`, one `{"type":"image","mimeType","data"}` block each). `Ok(None)` means it already failed the turn with `core.finish_turn(Outcome::Failed, …)`.
- `opencode_stray_reply` keeps its name, signature and export.

- [ ] **Step 1: Move the functions and their tests**; opencode calls `acp::…`. No new behaviour, so no new failing test: the existing opencode unit and live tests are the check.
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine opencode` → PASS, same test count as before for opencode's live tests (`git stash; cargo test … 2>&1 | tail -3` before the change gives the number).
- [ ] **Step 3: Run** `make rust-check` → PASS.
- [ ] **Step 4: Commit** `git commit -m "ACP helpers shared by opencode and the next ACP provider"`

### Task 2: Row fields, `Config` fields and driver hooks every provider gets

**Files:**
- Modify: `crates/octet-engine/src/live/mod.rs` (`Provider`, `Config`, `no_prepare`, `prepare`), `protocol.rs` (`Protocol::holds_turn_for_switch`), `driver.rs` (`run`, `must_hold`, `mode_unknown_for`)
- Modify: `crates/octet-engine/src/live/{codex,claude,opencode,demo}.rs` (rows)
- Modify: `crates/octet-core/src/model.rs` (`Selection::configure`), `crates/octet/src/main.rs` (`run`), `crates/octet-tui/src/models.rs` (`Prober`), `crates/octet-tui/src/lib.rs` (`attach_models`)

**Interfaces:**
- Produces:
  - `Provider.forks: bool` (doc: "The vendor can open a session as a copy of another (`/fork`)"): `true` for Codex, Claude and opencode, `false` for the demo.
  - `Provider.carries_others_models: bool` (doc: "Lists models that are other providers' own, under the same bare IDs (Antigravity lists Claude and GPT models); a name another provider also matches goes to that provider"): `false` for all four existing rows. Task 9 reads it.
  - `Provider.prepare: fn(&Config) -> Result<Vec<String>, String>` (doc: "Runs before the CLI starts: `Ok` lists notices to show; `Err` is why the session cannot start"). `pub(super) fn no_prepare(_: &Config) -> Result<Vec<String>, String>` for the four existing rows.
  - `pub fn prepare(config: &Config) -> Result<Vec<String>, String>` in `live`, calling the row's function: the gate shares it with `launch_args` and `launch_env`.
  - `Config.data_dir: Option<PathBuf>` (doc: "Octet's data directory, for a provider that keeps files of its own there (Antigravity: its settings, and the workspaces the user trusted)") and `Config.trust_workspace: bool` (doc: "Start although the workspace's own vendor configuration is not trusted (`--trust-workspace-config`)"). `Config::new` sets `None` and `false`.
  - `Protocol::holds_turn_for_switch(&self) -> bool`, default `false` (doc: "A turn sent while a mode change is pending waits for it although the provider's switch reaches a running turn: the vendor answers requests in no fixed order").
- Driver:
  - `run` returns `Err(format!("{title} cannot fork a session"))` when `config.fork && !provider.forks`, before anything starts.
  - `run` calls the row's `prepare` before `Process::spawn` (`Err` ends the session with that text) and emits each notice as `Event::Notice` before `initialize`.
  - `must_hold` and `mode_unknown_for` replace `provider.mode_applies_next_turn` with `provider.mode_applies_next_turn || self.protocol.holds_turn_for_switch()`.
- `Selection::configure` copies `data_dir` and `trust_workspace` from `current`. `main.rs` sets `config.data_dir = Some(directory.clone())` once `directory` is known. `Prober` gains `data_dir: Option<PathBuf>`, filled from `config.data_dir` in `attach_models` and put in the probe's `Config`.

- [ ] **Step 1: Write the failing tests.** In `live/mod.rs` tests: `every_vendor_but_the_demo_forks` (`forks` is true for Codex, Claude, opencode; false for the demo) and `a_provider_with_nothing_to_prepare_has_no_notices` (`prepare(&Config::new(Engine::CODEX, "codex", "/w")) == Ok(vec![])`). In `octet-core/src/model.rs` tests: `configure_keeps_the_data_directory_and_the_trust_flag` (a `current` with `data_dir: Some("/d")`, `trust_workspace: true` gives the same two in the result, for the same provider and for another).
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine --lib every_vendor_but` → FAIL: no field `forks`.
- [ ] **Step 3: Implement** the fields, the hook and the driver changes above.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Provider rows say whether they fork and what to prepare; sessions carry Octet's data directory"`

### Task 3: Octet's own Antigravity settings file

**Files:**
- Create: `crates/octet-engine/src/live/antigravity.rs` (this task: `mod settings;`, `mod trust;` comes in Task 4, the protocol in Task 6)
- Create: `crates/octet-engine/src/live/antigravity/settings.rs`
- Modify: `crates/octet-engine/src/live/mod.rs` (`mod antigravity;`)

**Interfaces:**
- Produces (in `settings.rs`, `pub(super)`):
  - `fn cli_dir(data_dir: &Path, mode: Mode) -> PathBuf`: `<data_dir>/antigravity/proceed` for `Mode::FullAccess`, `<data_dir>/antigravity/review` for the other three. Two folders, so sessions in different modes never share a file.
  - `fn merged(user: Option<&str>, mode: Mode) -> (String, Option<&'static str>)`: the JSON text of Octet's file, and a notice when `user` could not be used.
  - `fn user_file() -> Option<PathBuf>`: `home_dir()` joined with `.gemini/antigravity-cli/settings.json`.
  - `fn write(data_dir: &Path, mode: Mode, user_file: Option<&Path>) -> Result<Vec<String>, String>`: reads the user's file, writes `<cli_dir>/settings.json`, returns the notices.
  - `const USER_SETTINGS_NOTICE: &str = "Octet could not read your Antigravity settings file, so this session runs without your own Antigravity settings"`.
  - `const USER_SETTINGS_LIMIT: u64 = 1024 * 1024`.
- Rules for `merged`:
  - Start from the user's object. `user` that is absent gives `{}` and no notice; `user` that is not a JSON object gives `{}` and the notice.
  - Remove `agentMode`. Remove `permissions.allow`; a `permissions` that is not an object is removed whole.
  - Set `toolPermission` to `"always-proceed"` for `FullAccess`, else `"request-review"`.
  - Every other key is kept as it was.
- Rules for `write`:
  - A missing user file is no settings and no notice. One that cannot be read, is over `USER_SETTINGS_LIMIT`, or is not UTF-8 gives the notice.
  - The folder is created owner-only (`0o700`); the file is written to a temporary name in that folder (`0o600`) and renamed over `settings.json`, replacing whatever `agy` saved there.
  - Failing to create or write gives `Err("Octet could not write its Antigravity settings (<path>): <error>")`. The user's file is never opened for writing.

- [ ] **Step 1: Write the failing tests** in `settings.rs`:
  - `the_asking_modes_pin_request_review_and_full_access_proceeds`: for `Ask`, `AcceptEdits`, `Auto` the parsed `toolPermission` is `"request-review"`; for `FullAccess` it is `"always-proceed"`.
  - `the_users_way_of_skipping_requests_is_dropped_and_the_rest_kept`: `user = {"toolPermission":"always-proceed","agentMode":"accept-edits","permissions":{"allow":["command(*)"],"deny":["command(rm)"],"ask":["mcp(*)"]},"modelProvider":"x"}` in `Ask` gives `toolPermission == "request-review"`, no `agentMode`, no `permissions.allow`, `permissions.deny == ["command(rm)"]`, `permissions.ask == ["mcp(*)"]`, `modelProvider == "x"`, notice `None`.
  - `settings_that_are_not_an_object_are_replaced_with_a_notice`: `user = "[1]"` and `user = "{broken"` each give only `toolPermission` and `Some(USER_SETTINGS_NOTICE)`.
  - `permissions_that_are_not_an_object_are_dropped`: `{"permissions":"all"}` gives no `permissions` key.
  - `the_two_pins_live_in_different_folders`: `cli_dir(d, Ask) == cli_dir(d, Auto)` and `!= cli_dir(d, FullAccess)`, both under `d/antigravity`.
  - `the_file_is_written_fresh_and_the_users_is_untouched`: in a `TempDir`, a user file with `always-proceed`; put a stale `settings.json` holding `{"stale":true}` in `cli_dir`; `write` then leaves Octet's file with `request-review` and no `stale`, and the user's file byte-for-byte as it was.
  - `an_oversized_or_unreadable_users_file_gives_the_notice_and_the_pin` (a file of `USER_SETTINGS_LIMIT + 1` bytes; a directory where the file should be).
  - `a_folder_that_cannot_be_written_stops_the_launch`: `data_dir` is a regular file → `Err` starting `Octet could not write its Antigravity settings`.
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine antigravity::settings` → FAIL: not defined.
- [ ] **Step 3: Implement** the functions above.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Antigravity: the settings file Octet launches it with"`

### Task 4: Trusting a workspace's `.agents/` configuration

**Files:**
- Create: `crates/octet-engine/src/live/antigravity/trust.rs`
- Modify: `crates/octet-engine/src/live/antigravity.rs` (`mod trust;`, re-exports), `crates/octet-engine/src/live/mod.rs` and `crates/octet-core/src/lib.rs` (re-exports)
- Modify: `crates/octet-engine/Cargo.toml` (`sha2 = "0.11.1"`, the version `octet-hub` uses)

**Interfaces:**
- Produces:
  - `pub struct WorkspaceConfig { pub files: Vec<String>, pub fingerprint: String }` (`Debug, Clone, PartialEq, Eq`): the files found, as paths relative to the workspace, sorted; and the SHA-256, in hex, of every file's relative path and bytes in that order.
  - `pub fn antigravity_workspace_config(cwd: &Path) -> Result<Option<WorkspaceConfig>, String>`: `None` when the workspace has none of `.agents/hooks.json`, `.agents/mcp_config.json`, or a file anywhere under `.agents/plugins/`.
  - `pub fn antigravity_trusted(data_dir: &Path, cwd: &Path, found: &WorkspaceConfig) -> bool`.
  - `pub fn antigravity_trust(data_dir: &Path, cwd: &Path, found: &WorkspaceConfig) -> Result<(), String>`.
  - `pub(super) fn check(config: &Config, data_dir: &Path) -> Result<(), String>`: `Ok` when nothing is found, the workspace is trusted, or `config.trust_workspace`; else the refusal.
- Rules:
  - Links are followed. The walk under `plugins/` stops with `Err("Octet cannot check this workspace's .agents/plugins: more than 2000 files, or folders nested deeper than 16")` past `MAX_FILES = 2000` or `MAX_DEPTH = 16`, and with a like error past `MAX_BYTES = 64 MiB` read in all. A file that exists but cannot be read is an `Err` naming it.
  - The record is `<data_dir>/antigravity/trusted-workspaces.json`: an array of `{"workspace": <canonical path>, "fingerprint": <hex>}`, newest last, one entry per workspace, at most `MAX_TRUSTED = 1024` (the oldest dropped). Written like Task 3's file (temporary name, rename, `0o600`). A record that cannot be parsed counts as empty.
  - `--trust-workspace-config` trusts for that run only; it records nothing.
  - The refusal, verbatim: `"Antigravity would load this workspace's .agents configuration, which can run commands without asking: {files}. Review it, then type /trust in Octet's terminal interface, or pass --trust-workspace-config"`. `{files}` is the first eight paths joined with `", "`, then `" and N more"`.

- [ ] **Step 1: Write the failing tests** in `trust.rs`, each in a `TempDir`:
  - `a_workspace_without_agents_configuration_needs_no_trust` (`Ok(None)`, also for an `.agents/` holding only `notes.md`, and for an empty `.agents/plugins/`).
  - `hooks_mcp_servers_and_plugins_are_found_and_listed_in_order` (files `[".agents/hooks.json", ".agents/mcp_config.json", ".agents/plugins/p/run.sh"]`).
  - `trust_is_remembered_for_the_workspace_and_its_contents`: not trusted; after `antigravity_trust`, trusted; another workspace with the same files is not.
  - `a_changed_file_must_be_trusted_again` (edit `hooks.json`).
  - `an_added_plugin_file_must_be_trusted_again`.
  - `a_change_behind_a_link_is_seen`: `.agents/plugins` is a link to `../shared`; editing `shared/run.sh` changes the fingerprint.
  - `a_plugins_folder_too_deep_or_too_full_cannot_be_trusted` (17 nested folders → `Err`).
  - `trusting_again_keeps_one_entry_and_the_list_stays_bounded` (trust 1,025 workspaces through the record's own helper with fake paths; the file holds 1,024 and not the first).
  - `an_untrusted_workspace_is_refused_naming_its_files`: `check` errs with the refusal holding `.agents/hooks.json`; `Ok` with `trust_workspace: true`; `Ok` once trusted.
  - `more_than_eight_files_are_summarised` (ten plugin files → `"… and 2 more"`).
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine antigravity::trust` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Antigravity: a workspace's .agents configuration is trusted before it loads"`

### Task 5: Fake Antigravity vendor

**Files:**
- Create: `crates/octet-testkit/src/bin/protocol-child/antigravity.rs`
- Modify: `crates/octet-testkit/src/bin/protocol-child/main.rs` (`mod antigravity;`; `"acp" if env::args().any(|arg| arg == "--cli_dir_name") => antigravity::run()`, before opencode's `"acp"` arm)
- Modify: `crates/octet-testkit/src/scenario.rs` (new constants, documented as "Antigravity: …")

**Interfaces:**
- Produces a fake the tests in Tasks 6–8 drive. It reads two files at start: `<the --cli_dir_name folder>/settings.json` (its `toolPermission`) and, if present, `fake-agy.json` in its working directory, with the optional keys `version` (default `"1.5.5"`), `signedOut`, `rejectSwitches`, `hangSwitches` (booleans).
  - `initialize` → `{"protocolVersion":1,"agentInfo":{"name":"antigravity","version":<version>},"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"audio":false,"embeddedContext":true},"sessionCapabilities":{"resume":{},"close":{},"load":{},"list":{}}}}`.
  - `session/new` and `session/resume` → `{"sessionId": "fixture-agy" | <the resumed ID>, "configOptions": [model, thought_level, mode]}`, then a `session/update` `available_commands_update` listing `fixture-agy-skill` ("An Antigravity fixture skill"), `compact`, `goal` and `plan`.
    - `model` options: `gemini-fixture-flash` ("Gemini Fixture Flash", current), `claude-fixture` ("Claude Fixture"), `plain-fixture` ("Plain Fixture").
    - `thought_level` options `low`, `medium`, `high`, current `high`; absent while the model is `plain-fixture`.
    - `mode` options `default` (current), `accept-edits`, `auto`, `plan`.
    - A resume starts at `default` and `high` again, as the real CLI does. Resuming `scenario::AGY_MISSING_SESSION` answers `{"code":-32603,"message":"Failed to read session missing-session"}`.
    - With `signedOut`, both answer `{"code":-32000,"message":"Authentication required"}`.
  - `session/fork` → `{"code":-32601,"message":"Method not found"}`.
  - `session/set_config_option` → the updated `configOptions`, then a `config_option_update` (and, for `mode`, a `current_mode_update`). An unknown mode answers `{"code":-32602,"message":"Unknown mode: <value>"}`. With `rejectSwitches`, every `mode` request after the first gets that error; with `hangSwitches`, every one after the first gets no reply.
  - `session/prompt`, scripted by the first text block. A turn "asks" when `toolPermission` is not `always-proceed` and the mode is `default`, or `accept-edits` for a command.
    - no script: a `user_message_chunk`, an `agent_thought_chunk`, `agent_message_chunk` `scenario::AGY_REPLY`, then `{"stopReason":"end_turn","usage":{"inputTokens":40,"outputTokens":2,"totalTokens":42}}`. Several text blocks are echoed joined by a newline instead.
    - `HOLD`: holds until `session/cancel`, then `{"stopReason":"cancelled"}`.
    - `APPROVAL`: `tool_call` (`toolCallId` `call-1`, `name` `run_command`, `kind` `execute`, `title` `echo fixture`, `rawInput` `{"command":"echo fixture"}`, `status` `in_progress`), an update `pending`, and when it asks a `session/request_permission` with `id` `"1"` and the two options. It then sends `in_progress`, `completed` (allowed, or not asked) or `failed` (denied), the text `allow`, `deny`, `cancelled` or `ran`, and `end_turn`.
    - `AGY_EDIT`: the same for `write_file`, `kind` `edit`, `title` `File fixture.txt`, one location `<cwd>/fixture.txt`; it asks only in `default`. Text `wrote` when not asked.
    - `APPROVALS_9`, `TOOL_FAIL`, `FAIL` (`{"code":-32603,"message":"Internal error: fixture failure"}`): as opencode's fake.
    - `AGY_UNASKED`: the `APPROVAL` tool call, never asking whatever the mode, text `ran unasked`; a `session/cancel` meanwhile ends it `cancelled`.
    - `AGY_SLOW_EDIT`: the `AGY_EDIT` tool call's first frame, then it waits for the next `session/set_config_option` to arrive, then `completed` with no request, `end_turn`.
    - `AGY_TO_PLAN`: `current_mode_update` and `config_option_update` naming `plan`, a text, `end_turn`.
    - `AGY_TO_AUTO`: the same naming `auto`, then it holds until `session/cancel` → `cancelled`.
    - `AGY_NEW_MODEL`: a `config_option_update` whose `model` is `claude-fixture`, a text, `end_turn`.
    - `AGY_SUBAGENT`: `tool_call` `invoke_subagent` (`kind` `other`) `in_progress` then `completed`, a text, `end_turn`.
    - `AGY_OTHER_SESSION`: a `session/request_permission` for session `other-ses`, then a normal reply.
    - `PARAMS`: the text is a JSON object `{"argv":[…],"settings":<the parsed settings file>,"opened":<the session method>,"sets":[[configId, value], …]}`.
    - an `image` block is echoed as `image <mimeType> <data length>`.
- New `scenario` constants: `AGY_REPLY = "Hello Antigravity"`, `AGY_SKILL = "fixture-agy-skill"`, `AGY_MISSING_SESSION = "missing-session"`, `AGY_EDIT = "agy-edit"`, `AGY_UNASKED = "agy-unasked"`, `AGY_SLOW_EDIT = "agy-slow-edit"`, `AGY_TO_PLAN = "agy-to-plan"`, `AGY_TO_AUTO = "agy-to-auto"`, `AGY_NEW_MODEL = "agy-new-model"`, `AGY_SUBAGENT = "agy-subagent"`, `AGY_OTHER_SESSION = "agy-other-session"`.

- [ ] **Step 1: Write the fake**, mirroring `protocol-child/opencode.rs`'s fixture struct and `run()` loop.
- [ ] **Step 2: Build it.** `scripts/rust-env.sh cargo build -p octet-testkit --bins` → OK. `make rust-check` → PASS.
- [ ] **Step 3: Commit** `git commit -m "Fake Antigravity ACP vendor for tests"` (exercised from Task 6).

### Task 6: Antigravity protocol: row, connect, models, effort, turns, resume, probe

**Files:**
- Modify: `crates/octet-engine/src/live/antigravity.rs` (the row, `AntigravityProtocol`, `start`, `prepare`, `launch_args`)
- Modify: `crates/octet-engine/src/live/mod.rs` (`Engine::ANTIGRAVITY`, `ALL = [CODEX, CLAUDE, OPENCODE, ANTIGRAVITY, DEMO]`, the doc of `Engine::parse`, the doc of `Event::Effort`, exports)
- Test: `crates/octet-engine/tests/live.rs`

**Interfaces:**
- Consumes: Task 1's `acp`, Task 2's row fields and `Config` fields, Task 3's `settings`, Task 4's `trust::check`.
- Produces:
  - `pub const ANTIGRAVITY: Engine`; `pub const ANTIGRAVITY_VERIFIED_VERSIONS: &[&str] = &["1.5.5"]`, exported from `live`.
  - The row: the Global Constraints `modes`; `steer: false`, `mode_applies_next_turn: false`, `inline_images: true`, `effort_live: true`, `takes_effort: true`, `ids_carry_name: false`, `efforts: &[]`, `shutdown_grace: Duration::from_secs(1)` (375 ms measured with a session open), `lists_models: true`, `commands_expand_shell: false`, `forks: false`, `carries_others_models: true`, `launch_env: super::no_env`.
  - `fn mode_id(mode: Mode) -> &'static str` (Global Constraints) and `fn mode_of(id: &str, pinned: Mode) -> Option<Mode>`: `default` is `pinned` when `pinned` is `FullAccess`, else `Ask`; `accept-edits` and `auto` map back; anything else is `None`.
  - `fn launch_args(config: &Config) -> Vec<OsString>`: `["acp", "--cli_dir_name", <settings::cli_dir(data_dir, mode)>]`, plus `["--gemini-dir", <data_dir>/antigravity/probe-home]` when `config.catalog_only`. Without a `data_dir` it is `["acp"]`, which never launches because `prepare` fails first.
  - `fn prepare(config: &Config) -> Result<Vec<String>, String>`, in this order: no `data_dir` → `Err("Antigravity needs Octet's data directory to keep its settings in")`; plugins switched off and not a probe → `Err(NO_PLUGIN_SWITCH)`; `trust::check`; for a probe, create `probe-home`; `settings::write(data_dir, config.mode, settings::user_file().as_deref())`.
  - `const NO_PLUGIN_SWITCH: &str = "Antigravity has no switch to turn plugins or their hooks off; start without --no-plugins and --no-plugin-hooks"`.
  - `const SIGN_IN: &str = "Sign in to Antigravity first: run `agy`"`.

Handshake (only moves forward; a reply out of order is `"Unexpected protocol initialization order"`, as opencode's `response` does):
1. `initialize` → `Phase::Handshaken`. When `agentInfo.version` is not in `ANTIGRAVITY_VERIFIED_VERSIONS` (absent counts as `unknown`) and this is not a probe: `Event::Notice("Octet has not been checked against Antigravity {version} (checked: 1.5.5). If a command or an edit runs without asking in ask or accept-edits, Octet cancels the turn and stops sending prompts")`.
2. `session/new {cwd, mcpServers: []}`, or `session/resume {sessionId, cwd, mcpServers: []}` when `config.resume` is set and this is not a probe.
   - An error whose `code` is `-32000`, or whose message holds `auth`, `sign in`, `log in` or `login` (any case), stops the session with `SIGN_IN`. Any other error: `"Antigravity could not open the session: {message}"`.
   - Success: `core.session` (the reply's, else the resumed ID), `Event::Models(acp::catalog(configOptions))`.
   - A probe then waits up to 2 s for `available_commands_update` (`Event::Skills`), then `Event::Ready { session: String::new() }`, as opencode's probe does. `stopping` removes `probe-home` for a probe (only a path ending `antigravity/probe-home`).
3. If `config.model` is set and differs from the `model` option's `currentValue`: set `model`. An error stops the session: `"Antigravity refused the model {model}: {message}"`.
4. If `config.effort` is `Some(level)`: when the options have a `thought_level` listing it, set it; when they list others, `Event::Notice("Antigravity takes effort {a, b or c} for this model; it keeps {current}")`; when there is no `thought_level`, `Event::Notice("This Antigravity model takes no thinking level")`. In the last two cases `core.config.effort = None`.
5. Set `mode` to `mode_id(core.mode)`, always. An error, or a reply whose `mode` `currentValue` is another ID, stops the session: `"Antigravity would not switch to mode {id} ({reason}); Octet stopped the session so Antigravity never acts outside the mode you chose"`.
6. `Phase::Idle`, `Event::Ready`, `Event::ModelSelected(model)`, `Event::Effort(level)` when step 4 set one, `core.adopt_mode(Some((Some(core.mode), id)))`.

A resumed session goes through steps 3–5 like a new one: that is what puts its mode, model and effort back.

Turn: `session/prompt {sessionId, prompt: [{type:"text", text}, …acp::image_blocks]}`.
- `agent_message_chunk` → `Event::Text`. `agent_thought_chunk`, `user_message_chunk` and `session_info_update` are not shown.
- `tool_call` / `tool_call_update` → `Event::Tool`: `id` is `toolCallId`; `name` is the first frame's `name`, else its `title`, else `"tool"`; `target` is `acp::frame_target`, else the title for kind `execute`; `phase` is `Done` for `completed`, `Failed` for `failed`, else `Running`; `detail` is `acp::content_text`. At most 256 calls are remembered per turn, each with its `kind`.
- `available_commands_update` → `Event::Skills(acp::commands(list))`, and the list is kept for `compact`.
- The prompt's reply → `Event::Usage("{totalTokens} tokens")` when present, then `finish_turn`: `end_turn` → `Completed` (`Interrupted` if a cancel was sent this turn), `cancelled` → `Interrupted`, another reason → `Outcome::Other`, an error reply → `Failed` with its message.
- `session/update` frames for another session, or while no turn runs, change nothing shown; `is_progress` is false for another session's frames.

`set_effort` (live): before the session is ready, record the level in `core.config.effort` for step 4. Ready: a level the model lists is set with `thought_level`, and its reply records it, emits `Event::Effort(Some(level))` and `driver::effort_notice`; a level it does not list, or a model without levels, gets step 4's notice and `Event::Effort(core.config.effort.clone())`; `None` sets the level the model had when the session opened, when that differs from the current one, else records `None` and emits `Event::Effort(None)` with the notice. It always returns `Ok(true)`.

- [ ] **Step 1: Write the failing tests** in `live.rs`, with a helper `fn antigravity(fixture: Value) -> (octet_testkit::TempDir, Config)`: a temporary folder holding `ws/` (the `cwd`, where `fixture` is written as `fake-agy.json` unless it is `Value::Null`) and `data/` (the `data_dir`).
  - `antigravity_connects_lists_models_and_skills`: `Ready{session:"fixture-agy"}`; `Models` holds `gemini-fixture-flash` named "Gemini Fixture Flash" with that ID; `Skills` holds `scenario::AGY_SKILL`; `ModeChanged(Mode::Ask)`.
  - `antigravity_turn_streams_text_and_usage`: two turns, each text `scenario::AGY_REPLY` (no `thinking`), `Usage("42 tokens")`, `Finished{Completed}`.
  - `antigravity_launches_against_octets_settings`: `PARAMS` gives `argv` `["acp","--cli_dir_name",<data>/antigravity/review]`, `settings.toolPermission == "request-review"`, no `settings.agentMode`, no `settings.permissions.allow`, and `sets` ending with `["mode","default"]`.
  - `antigravity_full_access_launches_with_always_proceed`: `c.mode = FullAccess` → `argv[2]` ends `antigravity/proceed`, `settings.toolPermission == "always-proceed"`, `ModeChanged(FullAccess)`.
  - `antigravity_requested_model_and_effort_are_set_and_confirmed`: `model = Some("claude-fixture")`, `effort = Some("low")` → `ModelSelected("claude-fixture")`, `Effort(Some("low"))`, and `sets` is `[["model","claude-fixture"],["thought_level","low"],["mode","default"]]`.
  - `antigravity_effort_on_a_model_without_levels_is_a_notice` (Review Focus): `model = Some("plain-fixture")`, `effort = Some("high")` → the notice `This Antigravity model takes no thinking level`, `Ready`, and no `thought_level` in `sets`.
  - `antigravity_effort_changes_live`: `SetEffort(Some("medium"))` → `Effort(Some("medium"))`; `SetEffort(Some("max"))` → a notice starting `Antigravity takes effort low, medium or high`.
  - `antigravity_resume_gets_its_mode_model_and_effort_back`: `resume = Some("ses-old")`, `mode = AcceptEdits`, `model = Some("claude-fixture")`, `effort = Some("low")` → `Ready{session:"ses-old"}`, `PARAMS` `opened == "session/resume"` and `sets` holds all three.
  - `antigravity_a_missing_session_is_an_error_not_a_hang`: `resume = Some(scenario::AGY_MISSING_SESSION)` → error starting `Antigravity could not open the session: Failed to read session`.
  - `antigravity_signed_out_says_how_to_sign_in`: fixture `{"signedOut":true}` → the error is `SIGN_IN`'s text.
  - `antigravity_an_unverified_version_is_noticed_once`: fixture `{"version":"9.9.9"}` → exactly one notice starting `Octet has not been checked against Antigravity 9.9.9` across two turns; none with the default fixture.
  - `antigravity_fork_is_refused_before_launch`: `resume = Some("ses-old")`, `fork = true` → error `Antigravity cannot fork a session`, and `data/antigravity` does not exist.
  - `antigravity_needs_a_data_directory`, `antigravity_refuses_the_plugin_switches` (`plugins.hooks = false` → `NO_PLUGIN_SWITCH`).
  - `antigravity_an_untrusted_workspace_does_not_start`: `ws/.agents/hooks.json` exists → the error holds `.agents/hooks.json`, and `PARAMS` is never reached; with `trust_workspace = true` it connects.
  - `antigravity_image_goes_inline`: a 1×1 PNG → turn text `image image/png <n>`.
  - `antigravity_probe_lists_models_and_skills`: `probe(config, PROBE_LIMIT)` gives three models and `AGY_SKILL`, and `data/antigravity/probe-home` is gone within 3 s of the probe returning.
  - Unit tests in `antigravity.rs`: `every_mode_has_a_mode_id_and_back` (`mode_of(mode_id(m), m) == Some(m)` for each `Mode::ALL`; `mode_of("default", Ask) == Some(Ask)`; `mode_of("plan", Ask) == None`); `the_launch_names_octets_settings_folder` (`["acp","--cli_dir_name","/d/antigravity/review"]` for `data_dir = "/d"` in `Ask`); `a_probe_runs_in_a_scratch_home` (with `catalog_only`, the arguments end `["--gemini-dir","/d/antigravity/probe-home"]`).
  - `antigravity_handshake_out_of_order_is_an_error`.
  - In `live/mod.rs`, extend `claude_has_time_to_exit_on_its_own` with Antigravity's grace, and fix tests that list every vendor: their expectations, not the code.
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine --test live antigravity_` → FAIL.
- [ ] **Step 3: Implement** `AntigravityProtocol` (state: `next_id`, `requests: HashMap<u64, Outstanding>`, `cancel_sent`, the last reported `configOptions` values (`model`, `mode`, the `thought_level` list, its current and opening values), `commands: Vec<String>` bounded by `MAX_SKILLS`, `tools: HashMap<String, Tool>`, `probe_wait`) with an `Outstanding` enum (`Initialize`, `Open(Option<String>)`, `SetModel`, `SetEffort{..}`, `SetMode`, `Prompt`), sending through a `request` helper as opencode's does.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Antigravity provider: handshake, models, effort, turns, resume and probe over ACP"`

### Task 7: Approvals, cancel, live mode switches, skills and compact

**Files:**
- Modify: `crates/octet-engine/src/live/antigravity.rs`
- Test: `crates/octet-engine/tests/live.rs`

**Interfaces:**
- Consumes: Task 6's protocol; `acp::permission_reply`, `acp::stray_reply`, `acp::approval`, `acp::cancel`.
- Produces:
  - `answer(wire, allow)`: `acp::permission_reply(wire, allow, ("allow", "deny"))`.
  - `pub fn antigravity_stray_reply(value: &Value) -> Option<Value>`: `acp::stray_reply(value, ("allow", "deny"))`, exported from `live`.
  - A `session/request_permission` for this session while `Phase::InTurn`: its `toolCall.toolCallId` is remembered as asked (Task 8 reads it), then `core.queue_approval(wire, detail, summary)` with `acp::approval(toolCall, known, "Antigravity")`. Any other request gets the stray reply: quietly for `fs/*` and `terminal/*`; with `"Antigravity asked for a permission {while its turn is being cancelled | outside this session's running turn}; Octet rejected it"` for a permission; else `"Request declined: {method}"`.
  - `interrupt`: `acp::cancel(session)`, `cancel_sent = true`.
  - `set_mode(target)`: `session/set_config_option {configId:"mode", value: mode_id(target)}` with a deadline of `limits.mode_confirm`; `mode_change_pending()` is true until the reply or the deadline; `holds_turn_for_switch()` is `true`.
    - A reply naming `mode_id(target)` (or carrying no options): `core.mode = target`, `Event::ModeChanged(target)`.
    - An error: `Event::Notice("Antigravity did not switch to {label}: {message}")`, `Event::ModeChanged(core.mode)`.
    - A reply naming the mode already in force: the same notice with `Antigravity reported mode {id}`. A reply naming any other mode stops the session with step 5's error.
    - The deadline: `Event::Notice("Antigravity has not confirmed the switch to {label}")`, `Event::ModeChanged(core.mode)`, `take_switch_timed_out()` true once, and `mode_unsettled()` is the target until its reply comes; a late reply is applied as above with `"Antigravity confirmed the switch to {label} late"`. At most 8 unanswered switches are kept; a ninth is refused with `"Antigravity has not confirmed 8 earlier mode switches; wait or reconnect"`.
  - `send_skill`: one prompt whose text is `/{name} {args}` (trimmed), after `context` and a blank line when there is context.
  - `compact`: the prompt `/compact` when the last command list holds `compact`; otherwise `finish_turn(Outcome::Failed, Some("Antigravity does not offer compaction in this session"))`.

- [ ] **Step 1: Write the failing tests:**
  - `antigravity_approval_allowed_sends_allow` (turn text `allow`, the tool ends `Done`); `antigravity_approval_denied_sends_deny` (`deny`, `Failed`); `antigravity_approval_times_out_as_denied` (`approval_timeout = 200 ms`); `antigravity_nine_approvals_deny_the_ninth`.
  - `antigravity_an_edit_asks_and_names_its_file`: `AGY_EDIT` → an `Approval` whose summary action is `Edit files` with `fixture.txt` among its files.
  - `antigravity_cancel_is_interrupted` (`HOLD`, `interrupt()` → `Finished{Interrupted}`); `antigravity_cancel_denies_a_waiting_approval`.
  - `antigravity_live_switch_sets_the_mode`: `SetMode(AcceptEdits)` → `ModeChanged(AcceptEdits)`; then `AGY_EDIT` gives text `wrote` and no `Approval`; `PARAMS` `sets` ends `["mode","accept-edits"]`.
  - `antigravity_auto_runs_a_command_unasked_without_tripping` (`SetMode(Auto)`, `APPROVAL` → text `ran`, `Finished{Completed}`).
  - `antigravity_refused_switch_keeps_the_mode`: fixture `{"rejectSwitches":true}` → a notice starting `Antigravity did not switch to accept-edits`, no `ModeChanged(AcceptEdits)` within 1 s.
  - `antigravity_a_prompt_waits_for_an_unanswered_switch_and_is_not_sent`: fixture `{"hangSwitches":true}`, `quick()` limits; `SetMode(AcceptEdits)` then a prompt → the notice `Waiting for Antigravity to confirm the switch to accept-edits before sending your prompt`, then an error starting `Not sent: `, and a later prompt is refused with `its mode is unknown`.
  - `antigravity_another_sessions_permission_is_rejected_not_shown`: `AGY_OTHER_SESSION` → no `Approval`, a notice starting `Antigravity asked for a permission outside`.
  - `antigravity_skill_runs_as_a_slash_prompt` (text `/fixture-agy-skill go`; with context, the context comes first); `antigravity_compact_uses_its_command` (text `/compact`, then the notice `Context compacted`).
  - `antigravity_failed_turn_shows_the_message`; `antigravity_tool_error_is_a_failed_call`.
  - Unit test in `antigravity.rs`: `antigravity_stray_requests_are_refused` (`fs/read_text_file` → `-32601`; a permission request → `deny`; an `id` that is a string comes back a string).
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine antigravity_` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Antigravity provider: approvals, cancel, live mode switches, skills and compact"`

### Task 8: The tripwire, Antigravity changing its own mode or model, and subagents

**Files:**
- Modify: `crates/octet-engine/src/live/antigravity.rs`
- Test: `crates/octet-engine/tests/live.rs`

**Interfaces:**
- Consumes: Task 7's asked set, tool table and switch state.
- Produces:
  - **The tripwire.** `fn tripped(kind: &str, loosest: Mode) -> Option<&'static str>`: `Some("a command")` for kind `execute` when `loosest` is `Ask` or `AcceptEdits`; `Some("an edit")` for kinds `edit`, `delete` and `move` when `loosest` is `Ask`; else `None`. `loosest` is the loosest mode in force since the turn began (`core.mode` at its start, raised by each switch confirmed during it, order `Ask < AcceptEdits < Auto < FullAccess`).
    - It is checked when a tool call's frame says `completed` or `failed` and no permission request named its `toolCallId` this turn.
    - When it fires: `session/cancel` is sent once, and the turn ends `Failed` with `"Antigravity ran {what} without asking while Octet shows {label}: {target}. Octet's settings did not take effect with this agy, so ask and accept-edits cannot be relied on. Octet cancelled the turn and sends no more prompts in this session. What ran cannot be undone"`.
    - From then on every prompt, skill and compaction ends `Failed`, unsent, with `"{NOT_SENT}Antigravity acted without asking earlier in this session, so Octet sends it nothing more. Start a new session"`.
  - **Antigravity changes the mode** (a `current_mode_update` or `config_option_update` naming a mode other than `mode_id(core.mode)` while none of Octet's switches is unanswered):
    - Looser than Octet's during a turn (`accept-edits` or `auto` under `Ask`; `auto` under `AcceptEdits`): `session/cancel`, the mode is set back at once, and the turn ends `Failed` with `"Antigravity moved to its {id} mode while Octet shows {label}. Octet cancelled the turn and set {label} back"`.
    - Anything else (`plan`, a tighter mode, or any change while idle): `Event::Notice("Antigravity moved to its {id} mode. Octet sets {label} back before the next prompt")`, and the mode is set back when the turn ends, or at once when idle.
    - Setting it back is a `session/set_config_option` that counts as a pending switch (so the driver holds the next prompt). Its confirmation shows nothing; an error or another mode stops the session with `"Antigravity would not go back to mode {id} ({reason}); Octet stopped the session so Antigravity never acts outside the mode you chose"`. `Event::ModeChanged` is not emitted for Antigravity's own change: the header keeps Octet's mode.
  - **Antigravity changes the model** (a `config_option_update` naming another `model`): `Event::ModelSelected(model)`, and the thinking levels are read again from that update.
  - **Subagents.** The first `tool_call` named `invoke_subagent` in a turn whose mode is `Ask` or `AcceptEdits`: `Event::Notice("Antigravity started a subagent. Its approval requests never reach Octet, so what it must ask for waits and does not run. In auto, Antigravity's own checker decides")`. Once a turn; the turn goes on.

- [ ] **Step 1: Write the failing tests:**
  - Unit, in `antigravity.rs`: `the_tripwire_covers_commands_in_both_asking_modes_and_edits_in_ask` (the table above, including `None` for `read`, `other`, `think`, and for every kind under `Auto` and `FullAccess`).
  - `antigravity_a_command_that_runs_unasked_stops_the_session`: `AGY_UNASKED` in `Ask` → `Error` starting `Antigravity ran a command without asking while Octet shows ask: echo fixture`, `Finished{Failed}`; the next prompt → `Error` starting `Not sent: Antigravity acted without asking`, and the fake receives no second `session/prompt` (a later `PARAMS` is refused too).
  - `antigravity_an_unasked_command_trips_in_accept_edits_too`; `antigravity_an_unasked_edit_is_fine_in_accept_edits` (`AGY_EDIT` → `wrote`, `Completed`).
  - `antigravity_an_edit_begun_under_accept_edits_does_not_trip_after_a_switch_to_ask` (Review Focus): `mode = AcceptEdits`, `AGY_SLOW_EDIT`, then `SetMode(Ask)` → `ModeChanged(Ask)`, `Finished{Completed}`, no error.
  - `antigravity_a_denied_command_does_not_trip` (`APPROVAL` denied ends `failed` with a request → `Completed`).
  - `antigravity_moving_itself_to_plan_is_put_back_before_the_next_prompt`: `AGY_TO_PLAN` → the notice, `Finished{Completed}`, no `ModeChanged`; then `PARAMS` shows `sets` ending `["mode","default"]`.
  - `antigravity_moving_itself_to_a_looser_mode_cancels_the_turn`: `AGY_TO_AUTO` in `Ask` → `Error` starting `Antigravity moved to its auto mode while Octet shows ask`, `Finished{Failed}`, and `sets` ends `["mode","default"]`.
  - `antigravity_a_model_it_changes_itself_reaches_the_header` (Review Focus): `AGY_NEW_MODEL` → `ModelSelected("claude-fixture")`.
  - `antigravity_a_subagent_is_reported_once_in_the_asking_modes`: `AGY_SUBAGENT` in `Ask` → one notice starting `Antigravity started a subagent`, `Completed`; none in `Auto`.
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-engine antigravity_` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Antigravity provider: stop when it acts unasked; follow its own mode and model changes; report subagents"`

### Task 9: Model names keep going to the provider they belong to

Antigravity lists `claude-sonnet-4-6`, `claude-opus-4-6-thinking` and `gpt-oss-120b-medium` beside its Gemini models. Without this task, `/model claude-sonnet-4-6` typed on Codex turns ambiguous once Claude lists it too, and `/model gpt-6-astra` typed on Claude stops reaching Codex, because two providers now share the `gpt` and `claude` leading words.

**Files:**
- Modify: `crates/octet-core/src/model.rs` (`Selection::resolve`)

**Interfaces:**
- Consumes: `Provider.carries_others_models` (Task 2; `true` only for Antigravity, Task 6).
- Produces: in `resolve`, for the exact owners and again for the prefix owners of a bare name: when the current provider is not among them and more than one matches, providers whose row says `carries_others_models` are set aside if that leaves at least one. The rest of the precedence is unchanged: `default` → the current provider if it lists the name → one exact owner → ambiguous → one prefix owner → suffix owners → current.

- [ ] **Step 1: Write the failing tests** in `model.rs`, with catalogs where Claude lists `claude-sonnet-4-6`, Codex lists `gpt-5.5`, and Antigravity lists `gemini-3.8-flash`, `claude-sonnet-4-6` and `gpt-oss-120b-medium`:
  - `a_name_claude_and_antigravity_both_list_goes_to_claude` (from Codex: `CLAUDE`, `Found::Listed`).
  - `on_antigravity_that_name_stays_on_antigravity` (from Antigravity: `ANTIGRAVITY`, `Found::Listed`).
  - `a_gpt_name_still_reaches_codex_by_its_leading_word` (from Claude, `gpt-6-astra`: `CODEX`, `Found::Prefix("gpt")`); `a_claude_name_still_reaches_claude_by_its_leading_word` (from Codex, `claude-opus-5-5`: `CLAUDE`).
  - `antigravitys_own_models_resolve_to_it` (from Codex: `gemini-3.8-flash` → `ANTIGRAVITY`, `Listed`; `gemini-9` → `ANTIGRAVITY`, `Prefix("gemini")`; `gpt-oss-120b-medium` → `ANTIGRAVITY`, `Listed`).
  - `naming_antigravity_picks_its_copy` (`antigravity/claude-sonnet-4-6` and `antigravity claude-sonnet-4-6` → `ANTIGRAVITY` with model `claude-sonnet-4-6`).
  - The existing ambiguity tests stay as they are.
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-core model::` → FAIL (`Ambiguous`, and `Found::Current` for the leading-word cases).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "A model name another provider owns still goes to that provider when Antigravity lists it too"`

### Task 10: The command line and the terminal interface

**Files:**
- Modify: `crates/octet/src/args.rs` (`--trust-workspace-config`), `crates/octet/src/main.rs` (help text)
- Modify: `crates/octet-tui/src/registry.rs` (`Cmd::Trust`), `crates/octet-tui/src/commands.rs` (`trust_command`, `fork_command`)

**Interfaces:**
- Consumes: `antigravity_workspace_config`, `antigravity_trust` (Task 4), `Provider.forks` (Task 2), `Config.data_dir`.
- Produces:
  - `--trust-workspace-config` sets `Args.trust_workspace`, which `Args::config` copies to `Config.trust_workspace`. Help gains one usage line and: `"Antigravity: a workspace's .agents configuration (hooks, MCP servers, plugins) loads only once you trust it: /trust in the interface, or --trust-workspace-config for one run."`
  - `/trust` (`Requires::NoTurn`; usage `"/trust: trust this workspace's .agents configuration for Antigravity, then connect"`; palette label `"Trust workspace configuration"`):
    - another provider → note `"Only Antigravity loads a workspace's .agents configuration; nothing to trust"`;
    - nothing found → note `"This workspace has no .agents configuration to trust"`;
    - an error from either function → that error as a note;
    - trusted → note `"Trusted: {files}. Octet asks again if these change"` and `Action::Exit(Exit::Reconnect)`.
  - `/fork` on a provider whose row has `forks: false` → note `"{title} cannot fork a session"`, no reconnect.
  - The phone's command set (`PHONE_HELP`, `CommandName`) is unchanged, so a phone cannot trust a workspace.

- [ ] **Step 1: Write the failing tests:**
  - `args.rs`: `the_trust_flag_reaches_the_configuration` (off by default; on with the flag).
  - `octet-tui` (next to the existing `/fork` and command tests): `fork_is_refused_where_the_provider_cannot_fork` (an Antigravity app with a session → the note, `Action::Continue`); `trust_records_the_workspace_and_reconnects` (a temp workspace with `.agents/hooks.json` → `Exit::Reconnect`, and `antigravity_trusted` is then true); `trust_says_when_there_is_nothing_to_trust`; `trust_is_antigravitys_alone` (a Codex app → the note).
  - `octet-tui`: `a_listed_goal_command_does_not_replace_octets_goal`: with Antigravity's skills holding `goal` and `compact`, `/goal x` and `/compact` still run Octet's own commands. If an existing test already proves this for a Claude-listed command, extend it instead.
  - `crates/octet/tests/headless.rs`: `print_refuses_an_untrusted_antigravity_workspace_naming_its_files` (exit 1, stderr holds `.agents/hooks.json` and `--trust-workspace-config`; with the flag, the fake's reply is printed).
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet -p octet-tui trust` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "/trust and --trust-workspace-config for Antigravity workspaces; /fork says when a provider cannot"`

### Task 11: Hub, phone app and docs

**Files:**
- Modify: `crates/octet/src/service.rs` (`TOOLS: [&str; 5]` gains `"agy"` last; the doc at `:285`; the test at `:716`)
- Modify: `apps/mobile/app/new.tsx:28`, `apps/mobile/src/protocol.ts:11`, `apps/mobile/src/model/enums.ts:5`, `apps/mobile/src/recent.ts:7`, `apps/mobile/src/sessions.ts:28` (`antigravity`, shown as `"Antigravity"`)
- Modify: `docs/rust/adding-a-provider.md` (`forks`, `prepare`, `holds_turn_for_switch`, `acp.rs`), `docs/tui.md` and `docs/rust/tui-features.md` (Antigravity in the provider lists; `/trust`; what its modes mean; no `/fork`, no `/steer`; tokens-only usage), `docs/remote-control.md` (`agy` among the tools `octet hub install` records)

- [ ] **Step 1: Extend the failing test** at `service.rs:716`: a fake `agy` in a directory of its own is found after `tmux`, `claude`, `codex` and `opencode`.
- [ ] **Step 2: Run it** → FAIL.
- [ ] **Step 3: Implement** the `TOOLS` entry, the five phone-app lines and the docs.
- [ ] **Step 4: Run** `make rust-check` and `make mobile-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Hub and phone app offer Antigravity; docs"`

### Task 12: Gate scenarios against the real CLI, and real runs

**Files:**
- Create: `crates/octet-gate/src/antigravity.rs` (the ACP client: `initialize`, `open`, `set`, `prompt`, permission answers, evidence; modelled on `Opencode<'a>` in the gate binary)
- Modify: `crates/octet-gate/src/lib.rs` (`pub mod antigravity;`), `crates/octet-gate/src/bin/protocol-gate.rs` (`ANTIGRAVITY_SCENARIOS`, `run_antigravity`, the `vendor_launch` branch)
- Modify: `docs/rust/protocol-gate.md` (an Antigravity column and its notes)

**Interfaces:**
- Consumes: `launch_args`, `prepare`, `antigravity_stray_reply`, `ANTIGRAVITY_VERIFIED_VERSIONS`.
- Produces: `const ANTIGRAVITY_SCENARIOS: [&str; 10] = ["initialize", "simple", "interrupt", "approval-allow", "approval-deny", "approval-interrupt", "resume", "compact", "lockdown", "subagent"]`, on model `gemini-3.8-flash` at `thought_level` `low`.
  - Every run makes a second temporary folder beside the workspace (removed on every exit path) holding `octet-data/` (the `Config.data_dir`) and `gemini-home/`. It calls `prepare`, then launches with `launch_args` plus `--gemini-dir <gemini-home>`.
  - **`lockdown`**: `gemini-home/antigravity-cli/settings.json` holds `{"toolPermission":"always-proceed","agentMode":"accept-edits","permissions":{"allow":["command(*)","write_file(*)"]}}`. In mode `default` it asks for `echo fixture` and for a file `gate.txt`. It passes only if each brought a `session/request_permission` (which it denies) and `gate.txt` does not exist.
  - **`subagent`**: asks the model to delegate `touch gate-sub.txt` with `invoke_subagent`. It passes if, 30 s after the turn ends, `gate-sub.txt` does not exist and no `execute` tool call completed on the connection.
  - **`resume`**: a second process resumes; the evidence records the mode and thinking level the reply reports (`default`, `high` on 1.5.5), and the turn recalls a word from the first.
  - **`approval-interrupt`**: `session/cancel` while a request waits; the turn ends `cancelled`.
  - The evidence records `agentInfo.version`.

- [ ] **Step 1: Write the scenarios.**
- [ ] **Step 2: Run each.** `make gate ENGINE=antigravity SCENARIO=<name>` → `status` starts with `passed`. If `lockdown` or `subagent` fails, stop and tell the user what ran unasked: the spec's anti-goal is at stake, and Octet must not ship around it.
- [ ] **Step 3: Check what the plan assumed** against the real CLI, and fix the code (not the check) where it differs:
  - a tool call's `rawInput` key for the command, so `ToolCall.target` and the approval's command read well;
  - the exit time when stdin closes in the middle of a turn; set `shutdown_grace` to that plus margin;
  - whether `session/new` accepts a stdio MCP server in `mcpServers`; if it does, add an `mcp` scenario that passes only if the MCP tool asks in `default`; if it cannot be tested, say in `docs/tui.md` that Octet has not checked MCP tools against `ask`;
  - the reply of a signed-out `agy` cannot be checked without signing out, which is not Octet's to do: Task 6's rule stays, and the doc says the wording is unconfirmed;
  - which user settings keys beyond those in Task 3 change how Antigravity asks (`trustedWorkspaces` is the candidate): run `lockdown` with each present; one that lets an action through joins Task 3's removed keys, with a test.
- [ ] **Step 4: A real run**, with `--binary` naming a wrapper (`exec agy "$@" --gemini-dir <scratch>`) so nothing reaches the user's history, in a throwaway workspace:
  - TUI (the tmux driver in memory `rust-tui-live-testing`): `--engine antigravity --model gemini-3.8-flash --effort low` completes a turn; in `ask`, "create hello.txt" asks, and denying leaves no file; Shift+Tab to `accept-edits` and the edit goes ahead while a command still asks; `/effort medium` is confirmed; `/reconnect` resumes and remembers; `/fork` is refused; `/model codex` (or `claude`) and back carries the conversation; `/model list` shows the clash labels `(claude)` and `(antigravity)` for a Claude model both list.
  - In a workspace with `.agents/hooks.json`: the session refuses, `/trust` connects, and editing the file refuses again.
  - Headless: `--print "say hi"` prints a reply; `--rpc` runs a prompt and an approval.
- [ ] **Step 5: The existing providers.** Rerun the sanity set for Claude and Codex (the `sanity/` scripts named in memory `rust-tui-live-testing`; Codex on `gpt-5.6-terra`) → all pass as before.
- [ ] **Step 6: Run** `make rust-check`, then lint and tests for Linux (memory `linux-ci-parity`) → PASS.
- [ ] **Step 7: Commit** `git commit -m "Protocol gate: Antigravity scenarios"`
