# opencode Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `opencode` a third Octet provider, next to `claude` and `codex`, driven over `opencode acp`. Its models (the free `opencode/*` ones included) appear in `/model`, and it follows Octet's four permission modes.

**Architecture:** One new protocol file (`crates/octet-engine/src/live/opencode.rs`) and one row in `Engine::ALL`; the shared driver does the rest, as `docs/rust/adding-a-provider.md` describes. Octet's modes become three opencode agents defined in `OPENCODE_CONFIG_CONTENT`, plus opencode's own `build` agent for `auto`. A mode switch is a live ACP `session/set_config_option {configId:"mode"}`. The launch needs environment variables, so `octet-proc` learns to set them. In the TUI, a small module labels models that clash across providers.

**Tech Stack:** Rust (workspace crates `octet-proc`, `octet-engine`, `octet-core`, `octet-tui`, `octet-testkit`, `octet-gate`, `octet`), tokio, serde_json; the Expo phone app (`apps/mobile`, TypeScript).

**Spec:** `docs/superpowers/specs/2026-10-21-opencode-provider-design.md` (amended 2026-10-21: decisions 5 and 9).

## Global Constraints

- The provider's name is `opencode`, its title `opencode`, and its default binary `opencode`.
- Launch: `opencode acp` (with `--cwd <config.cwd>`). Wire: JSON-RPC 2.0, one object per line, every frame Octet sends carries `"jsonrpc":"2.0"`.
- The agents are `octet-ask` (`ask`), `octet-accept-edits` (`accept-edits`), `build` (`auto`, opencode's own) and `octet-full-access` (`full-access`).
- Never answer a vendor approval yourself; keep every buffer bounded; never read vendor credentials (from `docs/rust/adding-a-provider.md`).
- Follow `.claude/skills/rust-engineer/SKILL.md`. Match the surrounding code's comment density and naming: doc comments on every item, `pub(super)` inside `live/`.
- Every task ends with `make rust-check` passing (fmt, clippy `-D warnings`, docs `-D warnings`, tests).
- Linux parity: before the final task's PR, run lint and tests for Linux (see the `linux-ci-parity` memory: rustup target or Docker `rust:1.98.1`).
- `/mode` copy for opencode, verbatim, in `Mode::ALL` order:
  1. `"opencode asks before edits, commands and other actions (agent octet-ask)"`
  2. `"Edits proceed; commands and other actions ask (agent octet-accept-edits)"`
  3. `"opencode's own permission settings decide (agent build; its defaults ask for nothing)"`
  4. `"No permission checks at all (agent octet-full-access)"`
- A clash label is `Name (engine)` or `Name (engine · sub)`, as in `GPT-5.5 (codex)` and `GPT-5.5 (opencode · openai)`.

## Review Focus

- **The user already sets `OPENCODE_CONFIG_CONTENT`:** their keys are kept and Octet's `agent.octet-*` keys win. JSON that isn't an object is replaced, with a notice. Test in Task 2.
- **A resumed session replays old updates** (`user_message_chunk`, `tool_call`…) before any turn: none may show as transcript text, tools or approvals. Test in Task 4.
- **Cancel ends with `stopReason: "end_turn"`** (opencode 1.18.25 does this): it must still be reported as `Interrupted`. Test in Task 5.
- **`/model opencode/glm-5.3-flash-free`** must select that model and not `glm-5.3-flash-free`, while `/model claude/sonnet` keeps working. Test in Task 7.
- **A clash row picked from the picker or the Tab popup** must reach that row's provider, even though its text is now `Name (engine…)`. Test in Task 8.

---

### Task 1: Launch environment for vendor processes

**Files:**
- Modify: `crates/octet-proc/src/lib.rs` (`ProcessConfig`, `Process::spawn`)
- Modify: `crates/octet-engine/src/live/mod.rs` (`Provider`, `vendor_process`, new `launch_env`)
- Modify: `crates/octet-engine/src/live/{codex,claude,demo}.rs` (rows)
- Modify: `crates/octet-engine/src/live/driver.rs:39`
- Modify: `crates/octet-gate/src/lib.rs:100-112`, `crates/octet-gate/src/bin/protocol-gate.rs` (callers)
- Modify: `crates/octet-testkit/src/bin/protocol-child/main.rs` (new mode `env`)
- Test: `crates/octet-proc/tests/transport.rs`

**Interfaces:**
- Produces:
  - `ProcessConfig.env: Vec<(OsString, OsString)>`, set on the child on top of the inherited environment.
  - `Provider.launch_env: fn(&Config) -> Vec<(OsString, OsString)>`.
  - `pub fn launch_env(config: &Config) -> Vec<(OsString, OsString)>` in `live`.
  - `pub fn vendor_process(engine: Engine, executable: PathBuf, args: Vec<OsString>, env: Vec<(OsString, OsString)>, cwd: PathBuf) -> ProcessConfig`.
  - `pub(super) fn no_env(_: &Config) -> Vec<(OsString, OsString)>` for Codex, Claude and the demo.
  - `GateProcess::spawn(engine, executable, args, env, cwd, deadline)`.

- [ ] **Step 1: Write the failing test** `child_receives_the_configured_environment` in `transport.rs`. Use `config("env")` with `env: vec![("OCTET_TEST_ENV".into(), "set".into())]`, then read one frame and assert it equals `{"env":"set"}`.
- [ ] **Step 2: Run it.** `scripts/rust-env.sh cargo test -p octet-proc child_receives_the_configured_environment` → FAIL: no field `env`.
- [ ] **Step 3: Implement.** Add the field (doc: "Extra environment variables for the child, on top of the inherited ones") and apply it with `Command::envs` in `Process::spawn`. Add the `env` mode to `protocol-child`: emit `{"env": $OCTET_TEST_ENV or null}`. Add `launch_env` to `Provider` and the three rows, add `launch_env()`, and thread `P::launch_env(&config)` into `vendor_process` in `driver.rs` and into the gate. Every existing `ProcessConfig { … }` literal gets `env: Vec::new()`.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Vendor processes can be launched with extra environment variables"`

### Task 2: opencode launch configuration (pure functions)

**Files:**
- Create: `crates/octet-engine/src/live/opencode.rs` (this task: launch helpers and their unit tests only; the protocol comes in Task 4)
- Modify: `crates/octet-engine/src/live/mod.rs` (`mod opencode;` only; the row is added in Task 4)

**Interfaces:**
- Consumes: `Config`, `Mode`, `Plugins` (existing); the `Provider.launch_env` signature from Task 1.
- Produces (all `pub(super)` in `opencode.rs`):
  - `fn agent(mode: Mode) -> &'static str`: `octet-ask`, `octet-accept-edits`, `build`, `octet-full-access`.
  - `fn mode_of(agent: &str) -> Option<Mode>`: its inverse; `None` for any other agent (`plan`, a user's agent).
  - `fn launch_args(config: &Config) -> Vec<OsString>`: `["acp", "--cwd", <cwd>]`.
  - `fn launch_env(config: &Config) -> Vec<(OsString, OsString)>`: reads `std::env::var("OPENCODE_CONFIG_CONTENT")` and calls `launch_env_from`.
  - `fn launch_env_from(config: &Config, user: Option<&str>) -> (Vec<(OsString, OsString)>, Option<String>)`: the variables, and a notice when the user's content was not a JSON object and was replaced.
  - `const AGENT_NOTICE: &str = "OPENCODE_CONFIG_CONTENT is not a JSON object; Octet replaced it for this session"`.

The agent block Octet merges into `agent` (exact values):

```json
{
  "octet-ask": {"mode": "primary", "description": "Octet: ask before edits and commands",
    "permission": {"*": "ask", "read": "allow", "glob": "allow", "grep": "allow", "list": "allow",
                   "lsp": "allow", "todoread": "allow", "todowrite": "allow", "skill": "allow"}},
  "octet-accept-edits": {"mode": "primary", "description": "Octet: edits proceed, other actions ask",
    "permission": {"*": "ask", "edit": "allow", "read": "allow", "glob": "allow", "grep": "allow",
                   "list": "allow", "lsp": "allow", "todoread": "allow", "todowrite": "allow", "skill": "allow"}},
  "octet-full-access": {"mode": "primary", "description": "Octet: no permission checks",
    "permission": {"*": "allow"}}
}
```

The variables: `OPENCODE_CONFIG_CONTENT` (the user's object with `agent.<these three>` set over it), `OPENCODE_DISABLE_PROJECT_CONFIG=1` always, and `OPENCODE_PURE=1` when `!config.plugins.load || !config.plugins.hooks || config.catalog_only`. A top-level `permission` key the user set is left alone, since `auto`/`build` uses it.

- [ ] **Step 1: Write the failing tests** in `opencode.rs`'s `mod tests`:
  - `every_mode_has_an_agent_and_back`: for each `Mode::ALL`, `mode_of(agent(m)) == Some(m)`; `mode_of("plan") == None`.
  - `the_launch_is_acp_in_the_workspace`: `launch_args` is `["acp","--cwd","/w"]` for `cwd = "/w"`.
  - `octet_agents_win_over_the_users_and_the_rest_is_kept`: `user = {"agent":{"octet-ask":{"permission":{"*":"allow"}},"mine":{}},"model":"x","permission":{"bash":"allow"}}`. The parsed `OPENCODE_CONFIG_CONTENT` has `agent.octet-ask.permission["*"] == "ask"`, `agent.mine` kept, `model == "x"`, `permission.bash == "allow"`, and the notice is `None`.
  - `content_that_is_not_an_object_is_replaced_with_a_notice`: `user = "[1]"` gives the notice `Some(AGENT_NOTICE)` and content holding only `agent`.
  - `project_config_is_always_off_and_plugins_follow_the_switches`: `OPENCODE_DISABLE_PROJECT_CONFIG=1` in every case; `OPENCODE_PURE` absent for `Plugins::default()` and present for `load: false`, for `hooks: false`, and for `catalog_only`.
- [ ] **Step 2: Run them.** `scripts/rust-env.sh cargo test -p octet-engine opencode::` → FAIL: not defined.
- [ ] **Step 3: Implement** the functions above, building the agent block with `serde_json::json!`.
- [ ] **Step 4: Run** `make rust-check` → PASS. Add `#[cfg_attr(not(test), expect(dead_code, reason = "used by the protocol in the next task"))]` only if clippy needs it, and remove it in Task 4.
- [ ] **Step 5: Commit** `git commit -m "opencode: launch arguments, agents and environment"`

### Task 3: Fake opencode ACP vendor

**Files:**
- Create: `crates/octet-testkit/src/bin/protocol-child/opencode.rs`
- Modify: `crates/octet-testkit/src/bin/protocol-child/main.rs` (`"acp" => opencode::run()`, `mod opencode;`)
- Modify: `crates/octet-testkit/src/scenario.rs` (new constants, documented as "opencode: …")

**Interfaces:**
- Produces: a fake that the tests in Tasks 4–6 drive. It answers:
  - `initialize`: `{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true},"sessionCapabilities":{"resume":{},"fork":{},"close":{}}}}`.
  - `session/new`, `session/resume` and `session/fork`: `{"sessionId": "fixture-ses" | <resumed id> | "fixture-fork", "configOptions": [model, mode]}`.
    - Model options: `opencode/fixture-free` ("OpenCode/Fixture Free", the current value), `openai/gpt-fixture` ("OpenAI/GPT Fixture").
    - Mode options: `build` (current), `plan`, `octet-ask`, `octet-accept-edits`, `octet-full-access`.
    - Then a `session/update` `available_commands_update` with `[{"name":"fixture-opencode-skill","description":"An opencode fixture skill"},{"name":"compact","description":"Compact the session"}]`.
    - On `session/resume` it first replays one `user_message_chunk`, one `agent_message_chunk` (`"REPLAYED"`) and one `tool_call`.
  - `session/set_config_option`: echoes the `configOptions` with the new `currentValue`. It accepts any model value. Once the model has been set to `scenario::OPENCODE_REJECT_MODE`, every later `mode` switch gets an error `{"code":-32602,"message":"unknown agent"}`; the mode set during the handshake still succeeds.
  - `session/prompt`: scripts chosen by the first text block:
    - no script: `agent_message_chunk` `scenario::OPENCODE_REPLY`, a `usage_update`, and `{"stopReason":"end_turn","usage":{"totalTokens":42}}`;
    - `HOLD` holds until `session/cancel`, then replies `end_turn` (as opencode 1.18.25 does);
    - `APPROVAL` sends `tool_call` (kind `execute`, title `echo fixture`), then `session/request_permission` (id 0, the three real options `once`/`always`/`reject`), and echoes the chosen `optionId` as a text chunk before `end_turn`;
    - `APPROVALS_9` sends nine requests at once;
    - `TOOL_FAIL` sends a tool call that ends `failed`;
    - `scenario::FAIL` gives an error reply `{"code":-32603,"message":"Internal error: fixture failure"}`;
    - `scenario::PARAMS` echoes, as text, a JSON object `{"argv":…,"env":{"OPENCODE_CONFIG_CONTENT":…,"OPENCODE_PURE":…},"modes":[every mode value set so far],"model":…,"opened":<the session method>}`;
    - an `image` content block in the prompt is echoed as `"image <mimeType> <data length>"`.
  - `session/cancel` (a notification) ends a held turn.
- New `scenario` constants: `OPENCODE_REPLY = "Hello opencode"`, `OPENCODE_REJECT_MODE = "reject-mode"` (as a model, it makes later mode switches fail), `OPENCODE_SKILL = "fixture-opencode-skill"`.

- [ ] **Step 1: Write the fake**, mirroring `codex.rs`'s fixture struct and `run()` loop.
- [ ] **Step 2: Build it.** `scripts/rust-env.sh cargo build -p octet-testkit --bins` → OK.
- [ ] **Step 3: Commit** `git commit -m "Fake opencode ACP vendor for tests"` (it is exercised in Task 4).

### Task 4: opencode protocol: connect, model, turn, resume

**Files:**
- Modify: `crates/octet-engine/src/live/opencode.rs` (the row, `OpencodeProtocol`, `start`)
- Modify: `crates/octet-engine/src/live/mod.rs` (`Engine::OPENCODE`, `ALL = [CODEX, CLAUDE, OPENCODE, DEMO]`, the doc of `Engine::parse`)
- Test: `crates/octet-engine/tests/live.rs` (helper `fn opencode() -> Config`)

**Interfaces:**
- Consumes: Task 2's helpers; the `Protocol` trait (`live/protocol.rs`).
- Produces:
  - `pub const OPENCODE: Engine`.
  - The row: `steer: false`, `inline_images: true`, `effort_live: false`, `efforts: &[]`, `shutdown_grace: Duration::from_millis(500)` (retime against the real CLI in Task 10), `lists_models: true`, `launch_args`, and `launch_env` (Task 2; the notice is emitted by `initialize`).
  - The `modes` are the Global Constraints copy.
  - `pub fn opencode_stray_reply(value: &Value) -> Option<Value>`, exported from `live` like `codex_stray_reply`. It replies `-32601 "Octet does not support <method>"` to any request with an `id` (`fs/*` and `terminal/*` included) and denies a `session/request_permission` outside a turn (`optionId` of its `reject_once` option).
  - `fn catalog(config_options: &Value) -> Vec<ModelInfo>`: `selection` and `id` are the option's `value`, `name` is its `name`, and `description` is empty. Values failing `valid_identifier` are dropped and the list is capped at 256 entries.

Handshake (only moves forward; a reply out of order is `"Unexpected protocol initialization order"`):
1. `initialize {protocolVersion:1, clientCapabilities:{}}` → `Phase::Handshaken`.
2. `session/new {cwd, mcpServers:[]}`, `session/resume {sessionId, cwd, mcpServers:[]}` (when `config.resume`), or `session/fork` (with `config.fork`) → store `core.session`, emit `Event::Models(catalog)`.
3. If `config.model` differs from the model's `currentValue`: `session/set_config_option {configId:"model", value}`.
4. `session/set_config_option {configId:"mode", value: agent(core.mode)}`.
5. On the last reply: `core.phase = Phase::Idle`, `Event::Ready{session}`, `Event::ModelSelected(currentValue)`, and `core.adopt_mode(Some((mode_of(current mode value), value)))`.

`available_commands_update` for this session emits `Event::Skills` (`SkillInfo` with name and description only).

Turn: `session/prompt {sessionId, prompt:[{type:"text",text}, …images as {type:"image",mimeType,data}]}`. Images are read with `tokio::time::timeout(core.limits.image_read, image.read_base64())`, as `claude.rs:254` does.
- `agent_message_chunk` → `Event::Text`.
- `tool_call` / `tool_call_update` → `Event::Tool(ToolCall{id: toolCallId, name: title on the first frame, target: first location path else rawInput.command else "", phase: pending|in_progress→Running, completed→Done, failed→Failed, detail: the content text})`.
- The prompt's reply → `Event::Usage(format!("{total} tokens"))` from `usage.totalTokens` when present, then `core.finish_turn`:
  - `end_turn` → `Completed`, or `Interrupted` if a cancel was sent this turn;
  - `cancelled` → `Interrupted`;
  - any other reason → `Outcome::Other(reason)`;
  - an error reply → `Failed`, with `error.message`.
- `session/update` frames arriving while no turn runs (the replay, after a resume) are ignored; `is_progress` is false for another session's frames.

- [ ] **Step 1: Write the failing tests** in `live.rs`:
  - `opencode_connects_lists_models_and_skills`: `Ready{session:"fixture-ses"}`; `Models` holds `opencode/fixture-free` named "OpenCode/Fixture Free"; `Skills` holds `fixture-opencode-skill`; `ModeChanged(Mode::Ask)`.
  - `opencode_turn_streams_text_and_usage`: two turns, each with text `scenario::OPENCODE_REPLY`, a `Usage("42 tokens")` and `Finished{Completed}`.
  - `opencode_launches_acp_with_octet_agents`: `PARAMS` gives `argv` beginning with `acp`, an env with `octet-ask` in `OPENCODE_CONFIG_CONTENT`, and `modes == ["octet-ask"]`.
  - `opencode_requested_model_is_set_and_confirmed`: `c.model = Some("openai/gpt-fixture")` gives `ModelSelected("openai/gpt-fixture")`.
  - `opencode_resume_ignores_the_replayed_history`: `c.resume = Some("ses-old")` gives `Ready{session:"ses-old"}`, then a prompt whose turn text is `OPENCODE_REPLY`, with no `"REPLAYED"` text and no `Tool` event before `Started`.
  - `opencode_fork_opens_a_new_session`: `c.fork = true` → `Ready{session:"fixture-fork"}`.
  - `opencode_image_goes_inline`: a 1×1 PNG in a temp dir gives the turn text `image image/png <n>`.
  - `opencode_handshake_out_of_order_is_an_error`: a `script_vendor` answering `id 2` before `id 1` gives `"Unexpected protocol initialization order"`.
- [ ] **Step 2: Run them.** `scripts/rust-env.sh cargo test -p octet-engine --test live opencode_` → FAIL.
- [ ] **Step 3: Implement** `OpencodeProtocol` (state: `next_id`, `requests: HashMap<u64, Outstanding>`, `cancel_sent: bool`, `in_flight_mode: Option<ModeRequest>`, `tool_names: HashMap<String, String>`) with an `Outstanding` enum (`Initialize`, `Open`, `SetModel`, `SetMode(Mode)`, `Prompt`). Send each frame through a `request(core, kind, method, params)` helper like Codex's.
- [ ] **Step 4: Run** `make rust-check` → PASS. `Engine::ALL` is now longer, so existing tests that list every vendor need opencode added: fix their expectations, not the code.
- [ ] **Step 5: Commit** `git commit -m "opencode provider: handshake, models, turns and resume over ACP"`

### Task 5: opencode approvals, cancel, live mode switch and errors

**Files:**
- Modify: `crates/octet-engine/src/live/opencode.rs`
- Test: `crates/octet-engine/tests/live.rs`

**Interfaces:**
- Consumes: Task 4's `OpencodeProtocol`, `agent`, `mode_of`.
- Produces:
  - `answer(wire, allow)`: `{"jsonrpc":"2.0","id":wire.id,"result":{"outcome":{"outcome":"selected","optionId":X}}}`. `X` is the `optionId` of the request's option whose `kind` is `allow_once` (allow) or `reject_once` (deny); the fallbacks are `"once"` and `"reject"`. Never `allow_always`.
  - `set_mode` sends `session/set_config_option {configId:"mode", value: agent(target)}`, armed with `deadline_after(core.limits.mode_confirm)`. `mode_change_pending()` is true until the reply.
    - A success reply emits `ModeChanged(mode_of(currentValue))`.
    - An error reply emits `Event::Notice("opencode did not switch to <label>: <message>")` and keeps the mode.
    - A timeout notices `"opencode has not confirmed the switch to <label>"` and still applies a late reply, as Claude does (`claude.rs` `ModeRequest`).
  - `interrupt` sends the `session/cancel {sessionId}` notification and sets `cancel_sent`.
  - Approval: `session/request_permission` for this session during a turn goes to `core.queue_approval(wire, detail, summary)`. The detail is `kind · title` plus `rawInput` pretty-printed. The summary `ApprovalSummary::new(action)` has action `"Run a shell command"` (kind `execute`, `command` = title), `"Edit files"` (kind `edit`, `files` = location paths), or `"Use a tool"` otherwise.

- [ ] **Step 1: Write the failing tests:**
  - `opencode_approval_allowed_sends_allow_once` (turn text contains `once`);
  - `opencode_approval_denied_sends_reject` (contains `reject`);
  - `opencode_approval_times_out_as_denied` (`approval_timeout = 200 ms` → `reject`, then `ApprovalClosed`);
  - `opencode_nine_approvals_deny_the_ninth` (eight `Approval` events and one notice);
  - `opencode_cancel_is_interrupted_though_it_says_end_turn` (`HOLD`, then `interrupt()` → `Finished{Interrupted}`);
  - `opencode_live_switch_sets_the_agent` (`SetMode(AcceptEdits)` → `ModeChanged(AcceptEdits)`; `PARAMS` `modes` ends with `octet-accept-edits`);
  - `opencode_auto_is_the_build_agent` (`SetMode(Auto)` → `modes` ends with `build`);
  - `opencode_refused_switch_keeps_the_mode` (`c.model = Some(scenario::OPENCODE_REJECT_MODE)`, then `SetMode(AcceptEdits)` → a `Notice` starting `opencode did not switch to accept-edits`, and no `ModeChanged(AcceptEdits)` within 1 s);
  - `opencode_full_access_launches_with_its_agent` (`c.mode = FullAccess` → `modes == ["octet-full-access"]`);
  - `opencode_failed_turn_shows_the_message` (`FAIL` → `Error("Internal error: fixture failure")`, `Finished{Failed}`);
  - `opencode_stray_requests_are_refused`: a unit test in `opencode.rs` showing `fs/read_text_file` gets `-32601` and an out-of-turn permission request gets `reject`.
- [ ] **Step 2: Run them.** `scripts/rust-env.sh cargo test -p octet-engine opencode_` → FAIL.
- [ ] **Step 3: Implement** the methods above.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "opencode provider: approvals, cancel and live mode switches"`

### Task 6: opencode probe, skills and compact

**Files:**
- Modify: `crates/octet-engine/src/live/opencode.rs`
- Test: `crates/octet-engine/tests/live.rs`

**Interfaces:**
- Consumes: Task 4's protocol; `probe()` in `live/mod.rs`.
- Produces:
  - **The probe.** With `config.catalog_only`, the protocol runs `initialize` and `session/new`, emits `Models`, then waits for `available_commands_update`. It emits `Skills` then `Ready{session: String::new()}`, or `Ready` alone 2 s after the session reply (the protocol `deadline`). Then it deletes the probe's session by running `<config.binary> session delete <id>` with `tokio::process::Command` (env from `launch_env`, stdin, stdout and stderr null, `kill_on_drop(true)`, 10 s timeout, fire-and-forget on a spawned task).
  - **`send_skill`** sends one `session/prompt` whose text is `"/{name} {args}"` (trimmed), with `context` first and a blank line between when given.
  - **`compact`** sends `/compact` the same way if the last command list holds `compact`. Otherwise it ends the turn: `finish_turn(Outcome::Failed, Some("opencode does not offer compaction in this session"))`.

- [ ] **Step 1: Write the failing tests:**
  - `opencode_probe_lists_models_and_skills`: `probe(opencode(), PROBE_LIMIT)` gives two models and skill `fixture-opencode-skill`.
  - `opencode_probe_deletes_its_session`: a `script_vendor` that logs its argv to a file; after `probe` returns, poll for at most 3 s until the log holds `session delete fixture-ses`.
  - `opencode_skill_runs_as_a_slash_prompt`: the fake echoes the prompt text for `/fixture-opencode-skill`, giving the text `/fixture-opencode-skill go`.
  - `opencode_compact_uses_its_command`: the turn text is `/compact`.
- [ ] **Step 2: Run them** → FAIL.
- [ ] **Step 3: Implement** the pieces above.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "opencode provider: model probe, skills and compact"`

### Task 7: Selecting opencode models; effort

**Files:**
- Modify: `crates/octet-core/src/model.rs` (`Selection::parse`, `Selection::resolve`)
- Modify: `crates/octet-core/src/catalog.rs` (new `suffix_owners`)
- Modify: `crates/octet-engine/src/live/mod.rs` (`Provider.takes_effort`, `Engine::check_effort`)
- Modify: `crates/octet-tui/src/commands.rs:176` (`effort_command`)

**Interfaces:**
- Produces:
  - **`Selection::parse("opencode/X", _)`** gives `{provider: OPENCODE, model: Some("opencode/X")}`. A new row field, `Provider.ids_carry_name: bool` (`true` only for opencode; doc: "Its model IDs can start with its own name, so `NAME/X` keeps the whole string as the model"), makes `PROVIDER/NAME` keep the whole string for that provider. `/model opencode opencode/X` gives the same.
  - **`claude/sonnet`** still gives `{CLAUDE, Some("sonnet")}`.
  - **`Catalogs::suffix_owners(&self, name: &str) -> Vec<(Engine, String)>`** returns the providers and the full ID, for listed IDs of the form `sub/<name>` (an exact, case-sensitive match after the last `/`).
  - **The precedence in `resolve`:** `default` → an exact owner (as today) → exactly one suffix owner (`Found::Listed`, the selection's model being the full ID) → `prefix_owners` → current. Two suffix owners are ambiguous (`SelectionError::Ambiguous`).
  - **`Provider.takes_effort: bool`** is `true` for Codex and Claude and `false` for opencode and the demo; keep the demo's current behavior by checking `offline` first in `effort_command`. `check_effort` on an engine whose row has `takes_effort == false` returns `Err(format!("{title} does not take a reasoning effort level"))`. `Selection::configure` already filters effort through `check_effort`, so a carried effort is dropped.

- [ ] **Step 1: Write the failing tests** in `model.rs` and `catalog.rs`:
  - `opencode_slash_form_keeps_the_whole_id`;
  - `claude_slash_form_is_unchanged`;
  - `a_bare_sub_provider_id_goes_to_opencode` (`openai/gpt-fixture` with opencode's list cached → OPENCODE, `Found::Listed`);
  - `an_exact_owner_beats_a_suffix_match` (Codex lists `gpt-5.5` and opencode lists `openai/gpt-5.5`; from Claude, `/model gpt-5.5` → CODEX);
  - `a_unique_suffix_resolves_to_the_full_id` (`glm-5.3-flash-free` → `opencode/glm-5.3-flash-free`);
  - `two_suffix_owners_are_ambiguous`;
  - `opencode_takes_no_effort` (`check_effort` errs with "opencode does not take a reasoning effort level"; `configure` from Codex with `effort: Some("high")` to opencode gives `effort: None`).
- [ ] **Step 2: Run them.** `scripts/rust-env.sh cargo test -p octet-core` → FAIL.
- [ ] **Step 3: Implement** the changes above.
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Model selection reaches opencode's provider/model IDs; opencode takes no effort"`

### Task 8: Clash labels in /model, the picker and Tab

**Files:**
- Create: `crates/octet-tui/src/clash.rs` (with `mod tests`)
- Modify: `crates/octet-tui/src/lib.rs` (`mod clash;`)
- Modify: `crates/octet-tui/src/app.rs` (`show_models` row text)
- Modify: `crates/octet-tui/src/composer.rs` (`picker_items`, `model_tab`)
- Modify: `crates/octet-tui/src/input.rs:315` (`model_text` reads both row forms)

**Interfaces:**
- Produces:
  - `pub(crate) fn labels(entries: &[(Engine, ModelInfo)]) -> Vec<Option<String>>`, one per entry: `Some(label)` for an entry that clashes and `None` otherwise.
    - An entry's **base** is its `id` (else `selection`) after the last `/`, lowercased.
    - Its **route** is `(engine, sub)`, where `sub` is the ID part before the last `/` (lowercased) or none.
    - It **clashes** when another entry has the same base and a different route.
    - Its **label** is `"{display} ({engine})"` or `"{display} ({engine} · {sub})"`, where `display` is `clean(name)` with a leading `"<anything>/"` removed.
  - `pub(crate) fn row_route(item: &str) -> (&str, Option<Engine>)` parses a picker or popup row back to its selection and provider. It accepts `sel · engine …` (today's rows) and `sel (engine…)` or `sel · Name (engine…)` (clash rows).
  - Row text for clashing entries:
    - `show_models`: the first line is `{label}` instead of `{name} · {engine}`;
    - `picker_items`: `{selection} · {label}`;
    - the Tab popup: `{name} ({engine})`, where `model_tab` needs the clash set (give it a `clashing: &HashSet<String>` of selections, built by the caller from `labels`).
  - Rows without a clash are unchanged, character for character.

- [ ] **Step 1: Write the failing tests** in `clash.rs`:
  - `the_same_model_through_two_providers_is_labelled`: codex `gpt-5.5` "GPT-5.5" and opencode `openai/gpt-5.5` "OpenAI/GPT-5.5" give `["GPT-5.5 (codex)", "GPT-5.5 (opencode · openai)"]`.
  - `two_opencode_sub_providers_clash`: `openai/gpt-5.5` and `azure/gpt-5.5` give `(opencode · openai)` and `(opencode · azure)`.
  - `different_models_are_not_labelled`: `opencode/glm-5.3-flash-free` is `None`.
  - `case_is_ignored`: `GPT-5.5` and `openai/gpt-5.5` clash.
  - `rows_route_back_to_their_provider`: `row_route("openai/gpt-5.5 · GPT-5.5 (opencode · openai)") == ("openai/gpt-5.5", Some(OPENCODE))`, `row_route("gpt-5.5 (codex)") == ("gpt-5.5", Some(CODEX))`, and `row_route("gpt-6-astra · codex · GPT-6 Astra") == ("gpt-6-astra", Some(CODEX))`.

  And in `composer.rs` tests: `a_clash_row_in_the_popup_names_its_provider_in_brackets`. Keep the existing `tab("/model gpt")` assertion passing, since its rows don't clash.
- [ ] **Step 2: Run them.** `scripts/rust-env.sh cargo test -p octet-tui clash` → FAIL.
- [ ] **Step 3: Implement** `clash.rs`, then wire it into the three screens and `model_text` (via `row_route`).
- [ ] **Step 4: Run** `make rust-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "/model labels the same model through different providers with the provider in brackets"`

### Task 9: Hub, phone app and docs

**Files:**
- Modify: `crates/octet/src/service.rs:22` (`TOOLS` gains `"opencode"`, with the doc at `:285` and its test at `:716-720`)
- Modify: `apps/mobile/app/new.tsx:28` (`ENGINES = ["claude", "codex", "opencode"]`) and any engine-name type in `apps/mobile` (`protocol.ts`'s `Engine` union, if one exists)
- Modify: `docs/rust/adding-a-provider.md` (mention `launch_env`, `takes_effort`, `ids_carry_name`), `docs/rust/protocol-gate.md` (an opencode column), `docs/tui.md` (opencode in the provider list, if it has one)
- Test: the existing `service.rs` test, extended; `make mobile-check`

- [ ] **Step 1: Extend the failing test** at `service.rs:716`: write a fake `opencode` into a directory like `~/.opencode/bin`, and assert that its directory is found after `tmux`, `claude` and `codex`.
- [ ] **Step 2: Run it** → FAIL.
- [ ] **Step 3: Implement**: the `TOOLS` entry, the phone `ENGINES` entry, and the docs.
- [ ] **Step 4: Run** `make rust-check` and `make mobile-check` → PASS.
- [ ] **Step 5: Commit** `git commit -m "Hub and phone app offer opencode; docs"`

### Task 10: Gate scenarios against the real CLI, and a real TUI run

**Files:**
- Modify: `crates/octet-gate/src/bin/protocol-gate.rs` (an `opencode` branch: `opencode_handshake`, `opencode_turn`, plus scenarios)
- Modify: `docs/rust/protocol-gate.md` (scenario table)

**Interfaces:**
- Consumes: `launch_args`, `launch_env`, `opencode_stray_reply`, and `vendor_process` with env (Task 1).
- Produces: the scenarios `initialize`, `simple`, `interrupt`, `approval-allow`, `approval-deny`, `resume`, `fork` and `lockdown`, using the free model `opencode/deepseek-v4.1-flash-free` (set by `session/set_config_option`).
  - **`lockdown`** runs in a workdir holding a project `opencode.json` with `{"permission":"allow","agent":{"octet-ask":{"permission":{"*":"allow"}}}}`. With `OPENCODE_CONFIG` pointed at a fixture file holding `{"permission":{"bash":{"echo *":"allow"}}}`, it asks the model to run `echo fixture` in `octet-ask`. It passes only if a `session/request_permission` arrives.

- [ ] **Step 1: Write the scenarios.**
- [ ] **Step 2: Run each.** `make gate ENGINE=opencode SCENARIO=<name>` → `status` starts with `passed`. If `lockdown` fails, stop and report which config source bypassed `ask`. Don't paper over it: the spec says anything Octet can't turn off is reported to the user.
- [ ] **Step 3: Time the shutdown.** Close stdin on a real `opencode acp` and time the exit. Set `shutdown_grace` to that plus margin, and update the test `claude_has_time_to_exit_on_its_own` in `live/mod.rs` with an opencode assertion.
- [ ] **Step 4: Do a real TUI run** with the tmux driver (memory `rust-tui-live-testing`):
  - `--engine opencode`, then `/model opencode/deepseek-v4.1-flash-free`: a turn completes;
  - in `ask`, "create hello.txt" asks, and the file isn't written when denied;
  - Shift+Tab to `accept-edits`: the switch is instant and the edit goes ahead;
  - `/model opencode/glm-5.3-flash-free` and `/model codex` keep the conversation, with the handoff note;
  - `/model list` shows `GPT-5.5 (codex)` and `GPT-5.5 (opencode · openai)` when both lists are cached.
- [ ] **Step 5: Run** `make rust-check` and the Linux lint/test (Global Constraints) → PASS.
- [ ] **Step 6: Commit** `git commit -m "Protocol gate: opencode scenarios"`
