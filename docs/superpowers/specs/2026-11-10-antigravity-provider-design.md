# Antigravity as a fourth provider: design

**Date:** 2026-11-10.

## Why

The user wants to work with Gemini models inside Octet, signed in with their
Google account, the way they work with Claude and Codex today. Google's
Gemini CLI stopped serving personal Google accounts on 2026-06-18; its
replacement for those accounts is the Antigravity CLI (`agy`). `agy` has a
built-in Agent Client Protocol server (`agy acp`), the protocol Octet already
speaks for opencode. So the provider is **Antigravity**, and Gemini models
(the Flash family first) are what it brings.

## Goals

- `antigravity` is a provider like `claude`, `codex` and `opencode`:
  `--engine antigravity`, `/model`, `/mode`, `/effort`, the sidebar, the
  journals, handoff between providers, skills, goals, queued prompts,
  headless `--print` and `--rpc`, and the hub and phone app.
- `/model` lists every model Antigravity reports for the account. On the
  user's account today: Gemini 3.8 Flash, Claude Sonnet 4.6, Claude Opus 4.6
  and GPT-OSS 120B.
- A Gemini Flash model runs a full turn in Octet: streamed text, tool calls,
  approvals, usage, cancel.
- Antigravity follows Octet's four permission modes (see decision 3).
- **Parity:** everything Octet supports for Claude and Codex works for
  Antigravity where `agy acp` makes it possible. Where it does not, Octet
  says so in plain words and changes nothing (see "Parity table").
- A protocol-gate scenario set for Antigravity, like the other vendors have.

## Non-goals

- The Gemini CLI (`gemini --acp`) as a provider. It needs a paid API key or
  a company licence; the user has neither in mind.
- Signing in, credentials or API keys in Octet. `agy` authenticates itself;
  Octet never reads its credentials.
- Antigravity's `plan` mode. Octet has no matching mode. Its `/plan` command
  still reaches the user as a skill, like any other listed command.
- A hard-coded model list. The list comes from the CLI.
- Antigravity's remote agents, projects, sandbox flag, browser agent and
  schedules as Octet features. Whatever of these arrives as an ordinary
  listed command or tool call is passed through; Octet adds no interface
  for them.

## Anti-goals

- Antigravity editing a file or running a command without asking while Octet
  shows `ask` (or running a command unasked in `accept-edits`). That is a
  failure even if everything else works.
- Octet answering an Antigravity approval by itself.
- Octet editing the user's Antigravity settings
  (`~/.gemini/antigravity-cli/settings.json`) or anything else under
  `~/.gemini`.
- Test or probe sessions piling up in the user's Antigravity history.
- Breaking `/model`, `/mode` or `/effort` for the existing providers.

## What was verified (agy 1.5.5, macOS, 2026-11-10)

All turns ran on Gemini 3.8 Flash at the `low` thinking level, in a scratch
workspace. The account's tier is reported as "Google AI Plus".

**Launch and handshake**

- `agy acp` speaks JSON-RPC 2.0, one object per line, on stdin/stdout.
- `initialize` (protocol version 1) returns `agentCapabilities` with
  `loadSession: true`, `promptCapabilities: {image: true, audio: false,
  embeddedContext: true}` and `sessionCapabilities` with `resume`, `close`,
  `load`, `list` and `additionalDirectories`. There is **no `fork`**:
  `session/fork` answers `-32601 Method not found`.
- `authMethods` lists `oauth-personal`, `oauth-business`, `gemini-api-key`
  and `agent-platform`. `agy acp --login` signs in interactively and exits.
- **`agy acp` ignores the top-level flags.** `agy --settings … acp`,
  `agy --mode accept-edits acp` and `agy --model … acp` all open a session
  in mode `default` on the default model. `agy acp --settings …` is refused
  as an unknown flag. The flags `agy acp` does take include `--gemini-dir`,
  `--app-data-dir`, `--cli_dir_name`, `--dangerously-skip-permissions`,
  `--sandbox`, `--add-dir`, `--agent`, `--log-file` and
  `--include-default-customizations`.

**Session**

- `session/new {cwd, mcpServers: []}` returns:
  - `sessionId` (a UUID);
  - `models: {availableModels: [{modelId, name}], currentModelId}` where each
    Gemini level is its own ID (`gemini-3.8-flash-high`, `-medium`, `-low`);
  - `modes: {currentModeId: "default", availableModes}` with `default`,
    `accept-edits`, `auto` and `plan`;
  - `configOptions`: `model` (values are families: `gemini-3.8-flash`,
    `claude-sonnet-4-6`, `claude-opus-4-6-thinking`, `gpt-oss-120b-medium`),
    `thought_level` (`low`, `medium`, `high`; category `thought_level`) and
    `mode` (the four modes);
  - `_meta.antigravity.tier` (`name`, `id`, `upgradeUri`).
- `session/set_config_option {sessionId, configId, value}` returns the
  updated `configOptions` at once, for `thought_level`, `mode` and `model`.
  After `model` was set to `gpt-oss-120b-medium`, the `thought_level` option
  was gone: not every model has one.
- `session/set_mode {modeId}` returns `{}`, but the `current_mode_update`
  notification confirming it arrived only after the next prompt was sent. An
  unknown mode answers `-32602 Unknown mode: …`. `session/set_model
  {modelId}` returns `{}`.
- After `session/new`, an `available_commands_update` lists commands and
  skills: `goal`, `compact`, `schedule`, `browser`, `plan`, `grill-me`,
  `learn`, `agy-customizations` and more.

**Turns**

- `session/prompt {sessionId, prompt: [{type: "text", text}]}` streams
  `session/update` notifications and returns `{stopReason, usage:
  {inputTokens, outputTokens, cachedReadTokens, cachedWriteTokens,
  thoughtTokens, totalTokens}}`. There is no cost and no context size.
- Updates seen: `user_message_chunk` (the prompt echoed back),
  `agent_message_chunk`, `agent_thought_chunk`, `tool_call` and
  `tool_call_update` (`toolCallId`, `name`, `kind`, `status`, `title`,
  `locations`, `rawInput`, `rawOutput`, `content`), `session_info_update`
  (`title`), `config_option_update`, `current_mode_update` and
  `available_commands_update`.
- Tool names and kinds seen: `write_file` (`edit`), `run_command`
  (`execute`), `read_file` (`read`). A status goes `in_progress` → `pending`
  (waiting for permission) → `in_progress` → `completed` or `failed`.
- `session/cancel` (a notification) during a turn ends it with
  `stopReason: "cancelled"`.
- `/compact` sent as the prompt text ran a turn and ended `end_turn`. The
  session still remembered an earlier turn afterwards.

**Permissions**

- In mode `default`, a file write and a shell command each sent a request
  `session/request_permission {sessionId, toolCall, options}` with
  `options: [{optionId: "allow", kind: "allow_once"}, {optionId: "deny",
  kind: "reject_once"}]`. The request's `id` is a string (`"1"`, `"2"`…).
  A file write carries a `diff` in `toolCall.content`.
- Reply `{"outcome": {"outcome": "selected", "optionId": "deny"}}`: the tool
  call ended `failed`, no file appeared, the turn ended `end_turn`. With
  `"allow"`, the file was written. Reply `{"outcome": {"outcome":
  "cancelled"}}`: the tool call failed and the turn ended `end_turn`.
- A workspace file read ran without a request.
- In mode `accept-edits`, a file write ran without a request, and a shell
  command still asked.
- **`allow` rules do not skip requests over ACP.** With
  `{"permissions": {"allow": ["command(echo)", "write_file(*)"]}}` in the
  settings file, `echo` and a write both still asked.
- **`toolPermission: "always-proceed"` skips every request.** With that key
  in the settings file, the command ran and the file was written with no
  request, while the session reported mode `default`. Adding `ask` rules
  (`command(*)`, `write_file(*)`, `mcp(*)`, `read_url(*)`, `execute_url(*)`)
  did **not** bring the requests back.
- **`--cli_dir_name NAME` moves the settings file.** `agy acp --help` calls
  it the "Subdirectory under app_data_dir for CLI settings and logs"; the
  app data folder is `<gemini dir>/antigravity-cli`. With the user's
  settings file saying `always-proceed`, and `agy acp --cli_dir_name NAME`,
  both actions asked again. Conversations stay in the shared place.

  *Corrected 2026-11-12.* This said an absolute `DIR` names that folder.
  It does not: `agy` joins an absolute name under its app data folder,
  dropping the leading `/` (its log: `Failed to load CLI settings, using
  defaults: open <gemini dir>/antigravity-cli/<DIR>/settings.json: no such
  file or directory`), makes `cache` and `log` there, and runs on its
  defaults. The requests above came back only because the defaults ask.
  Verified now (scratch `--gemini-dir`): a name of 32 `../` (24 works too)
  followed by the folder's absolute path without its leading `/` resolves
  to the folder itself; `agy` reads `settings.json` there (a malformed file
  is reported, a valid one is not) and makes its `cache` and `log` inside
  it, and nothing for it under its home. Switching model, thinking level
  and mode over ACP did not rewrite the file.
- A session opened under a settings file with `agentMode: "accept-edits"`
  still started in mode `default`.

**Sessions across processes**

- `session/resume {sessionId, cwd, mcpServers}` in a new process returns the
  same shape as `session/new` and replays nothing. The next turn remembered
  the earlier ones. **Mode and thinking level came back as `default` and
  `high`**, not as they were left.
- `session/load` replays the conversation as `session/update`s, then returns
  that shape.
- `session/resume` of an unknown ID answers `-32603 Failed to read session
  …`.
- `session/list {cwd}` returns `sessions: [{sessionId, cwd, title,
  updatedAt}]`. `session/close` returns `{}` and does not delete the
  session. No delete method was found.
- **`--gemini-dir DIR`** (absolute) runs `agy acp` against a fresh data
  folder. Turns ran there on the existing sign-in, with no new login, and
  its sessions never reach the user's history.
- `initialize` reports the CLI's version in `agentInfo.version` (`1.5.5`).
  `agy` updates itself in the background.

**`auto` under Octet's settings**

- With `DIR/settings.json` saying `request-review`, a live switch to mode
  `auto` was confirmed at once. Then `echo`, a file write, `curl -sI
  https://example.com` and `rm -rf` of a path that did not exist all ran
  with no request. `auto` asks rarely.

**Workspace hooks**

- A new, never-opened workspace held `.agents/hooks.json` with a
  `SessionStart` and a `PreInvocation` command hook. Both commands ran on
  the first prompt, with no request and no trust question, in mode
  `default` under Octet's settings.
- `--include-default-customizations=false` did not stop them, and neither
  did `hooks` or `disableHooks` keys in the settings file. No switch was
  found.
- Google's hook documentation says a `PreToolUse` hook may answer
  `auto_approve`, which approves a tool call that would otherwise ask.

**Checked while planning (2026-11-11)**

All under `--gemini-dir` (a scratch folder) and `--cli_dir_name` (Octet-style
settings saying `request-review`) together, which work in one launch.

- In mode `default`, `ls`, `pwd` and `git status` each sent a permission
  request. No shell command ran unasked.
- **A live mode switch reaches the running turn.** A turn was started in
  `default`; while its first command ran, the mode was set to
  `accept-edits`; the file write that followed ran with no request. Its
  status still went `in_progress` → `pending` → `in_progress` →
  `completed`, so `pending` does not mean a request was sent.
- **Subagents wait unseen.** Asked to delegate, the model called
  `invoke_subagent` (kind `other`), which completed at once with no
  request, and the turn ended. The subagent runs as its own conversation.
  Its command did not run: `manage_subagents` reported it
  `waiting_for_input`, waiting for an approval. That request never arrived
  on Octet's connection in 60 s.
- `agy acp` exits 11 ms after its input closes before a session is open,
  and 375 ms after with one open.

**From Google's documentation, not tested here**

- Hooks, MCP servers and plugins can also come from the workspace:
  `<workspace>/.agents/hooks.json`, `.agents/mcp_config.json` and
  `.agents/plugins/`. User-wide ones live under `~/.gemini/config/`.
- `agy` rewrites its settings file at startup, dropping keys that hold
  their default value, and saves "always allow" choices into it.
- `--dangerously-skip-permissions` is the same as `toolPermission:
  "always-proceed"`. (Running `agy` with that flag was not possible in this
  session; the settings key was tested instead.)
- Antigravity's `auto` mode runs commands in its terminal sandbox and checks
  commands that leave the sandbox, asking only for risky ones.

## Decisions

### Made by the user

1. The provider is the Antigravity CLI, signed in with the user's Google
   account, not the Gemini CLI.
2. It supports what Claude and Codex support in Octet, wherever the CLI
   makes that possible.

   Decisions 19 and 20 below are also the user's.

### Claude's calls (the user saw these in summary and said to proceed)

3. **Permission modes.**

   | Octet mode | Antigravity |
   | --- | --- |
   | `ask` | mode `default`, with Octet's settings pinning `toolPermission: "request-review"` |
   | `accept-edits` | mode `accept-edits`, same pin |
   | `auto` | mode `auto` (Antigravity's own checker and sandbox decide), same pin |
   | `full-access` | chosen at launch only, as for every provider: Octet's settings say `toolPermission: "always-proceed"` |

   The `/mode` descriptions say what each does in Antigravity's terms,
   including that reads inside the workspace never ask.

4. **Octet's own settings file closes the `always-proceed` gap.** Octet
   launches `agy acp --cli_dir_name <NAME>`, where `<NAME>` names a folder
   `<DIR>` in Octet's own data directory, never under `~/.gemini`: 32 `../`
   (to climb out of `agy`'s app data folder to the root), then `<DIR>`'s
   absolute path without its leading `/`. *Corrected 2026-11-12:* this
   said `<DIR>` itself; `agy` joins an absolute name under
   `~/.gemini/antigravity-cli` and then runs on its defaults, never reading
   Octet's file (see "What was verified"). Octet writes
   `<DIR>/settings.json` fresh before every launch, so nothing `agy` saved
   there carries over. The user's own settings file is read, never written.
   What Octet's file holds:
   - `toolPermission`, as the table in decision 3 says;
   - no `agentMode`, and no `permissions.allow` rules (they are the user's
     way of skipping requests, and Octet's modes decide that instead, as
     Claude's `--setting-sources=` does);
   - the user's other keys, copied from their settings file, so that their
     `permissions.deny` and `permissions.ask` rules, `modelProvider` and the
     like still apply.

   If Octet cannot write its settings file, the session does not start.

5. **Strict, as with Claude and opencode.** In `ask` and `accept-edits`,
   nothing may let Antigravity act without asking. The gate proves each seam
   closed, or Octet reports it to the user instead of silently accepting it:
   - the user's settings file (`always-proceed`, `proceed-in-sandbox`,
     `allow` rules, `agentMode`): closed by decision 4;
   - workspace configuration under `.agents/` (hooks, MCP servers,
     plugins): it cannot be turned off, so decision 19 applies;
   - subagents (the browser agent, any agent that runs tools);
   - MCP tools.

   For a seam the gate can neither prove closed nor test (subagents and MCP
   tools may be such), the tripwire in decision 20 is the backstop, and the
   `/mode` description names the seam.

   The rule from the opencode work holds: **when the mode is unknown, refuse
   to send.** A prompt never goes out while a mode switch is unconfirmed.

6. **Mode switches are live** and use `session/set_config_option
   {configId: "mode"}`, because its reply confirms the new mode at once.
   `session/set_mode` is not used: its confirmation arrives late.

7. **What a model is to Octet.** A model is a value of the `model` config
   option (`gemini-3.8-flash`), with its `name`. The thinking level is
   Octet's effort: `/effort low|medium|high` sets `thought_level` live with
   `session/set_config_option`. For a model with no `thought_level` option,
   `/effort` says the model takes none. `Event::Ready` reports the confirmed
   model.

8. **A resumed session gets its settings back.** After `session/resume`,
   Octet re-applies the mode, the model and the effort before the first
   prompt, because Antigravity resets them. `session/resume` is used, not
   `session/load`: nothing is replayed.

9. **Fork is not offered.** `/fork` and `--resume ID` with a fork answer
   "Antigravity cannot fork a session" and change nothing. Octet does not
   fake a fork by copying text.

10. **Compact** sends `/compact` as the prompt, when the command list has
    it; otherwise it is refused with a clear note.

11. **Skills** come from `available_commands_update`. A skill run sends
    `/name args` as the prompt text.

12. **Images** go inline as base64 ACP image blocks, under Octet's existing
    inline limits.

13. **Usage** reports tokens. Cost and context size are absent, so they are
    not shown.

14. **Not signed in.** When `agy` reports that it needs authentication,
    Octet says: "Sign in to Antigravity first: run `agy`". Octet starts no
    sign-in.

15. **Name clashes.** Antigravity lists Claude models that the Claude
    provider may list too. The existing clash display handles this:
    `Claude Sonnet 4.6 (claude)` and `Claude Sonnet 4.6 (antigravity)`.
    Existing names keep resolving as they do today.

16. **Provider row:** `name: "antigravity"`, `title: "Antigravity"`,
    `default_binary: "agy"`, `steer: false`, `inline_images: true`,
    `effort_live: true`, `takes_effort: true`, `ids_carry_name: false`,
    `efforts: &[]` (the levels depend on the model),
    `commands_expand_shell: false` unless the builder finds otherwise.

17. **Tests never touch the user's history.** The protocol gate and any
    real-CLI test run `agy acp` with `--gemini-dir` pointing at a temporary
    folder, removed afterwards. The daily model probe must not leave a
    session in the user's history either.

18. **Hub and phone:** `agy` joins the hub's tool discovery
    (`crates/octet/src/service.rs`, `TOOLS`; it installs to
    `~/.local/bin`), and `antigravity` joins the phone app's engine list
    (`apps/mobile/app/new.tsx`, `ENGINES`). `--engine`'s default stays
    `codex`.

### Made by the user after the builder check (2026-11-10)

19. **A workspace's `.agents/` configuration is trusted once per
    workspace.** Before Octet starts Antigravity in a workspace that has
    `.agents/hooks.json`, `.agents/mcp_config.json` or anything under
    `.agents/plugins/`, it lists what it found and starts only after the
    user says they trust it. This applies in every mode, since a hook runs
    whatever the mode.
    - Octet remembers the answer for that workspace, in its own data
      directory, and asks again when those files change.
    - Headless (`--print`, `--rpc`) and hub-started sessions cannot ask.
      They refuse with a message naming the files, unless the workspace is
      already trusted or a trust flag is passed.
    - A workspace with no such files starts as usual, with no question.
    - Hooks, MCP servers and plugins in the user's own `~/.gemini/config/`
      are the user's and load as Antigravity loads them.

20. **An unverified `agy` version warns; a tripwire guards every version.**
    - When `agentInfo.version` is not a version Octet was checked against,
      Octet shows one notice per session saying so, and keeps working.
    - **The tripwire:** in `ask`, a command or an edit that Antigravity
      runs without having sent a permission request means Octet's settings
      did not take effect. In `accept-edits` the same holds for a command.
      Octet then cancels the turn, refuses further prompts in that session,
      and tells the user what ran and that the asking modes cannot be
      relied on with this `agy`. The action that tripped it cannot be
      undone, and the message says so.
    - The tripwire does not apply in `auto` and `full-access`.

### Claude's call after the builder check

21. **Antigravity changing its own mode.** Its `/plan` command, for one,
    moves the session to `plan`. Octet's header keeps showing Octet's mode.
    - Before the next prompt, Octet sets its own mode back and sends only
      once that is confirmed.
    - If Antigravity reports a looser mode than Octet's during a turn
      (`accept-edits` or `auto` while Octet shows `ask`; `auto` while Octet
      shows `accept-edits`), Octet cancels the turn, says why, and sets its
      mode back. A move to `plan` is let run to the turn's end, with a
      notice.

### Claude's calls while planning (2026-11-11)

22. **Subagents are reported, not stopped.** A subagent cannot act unasked
    in `ask` or `accept-edits`: it waits for an approval Octet never sees.
    When a turn starts one in those modes, Octet says so once: its approval
    requests never reach Octet, so what it must ask for waits and does not
    run. Cancelling the turn would not end the subagent, so Octet does not.

23. **A mode switch applies to the running turn**, so the provider row says
    `mode_applies_next_turn: false`. A prompt sent while a switch is
    unconfirmed is still held until the switch is confirmed or refused
    (decision 5), through a new hook on the protocol.

24. **The tripwire reads requests, not statuses.** A command or an edit
    trips it when it ends (`completed` or `failed`) and no permission
    request named its tool call. It is judged against the loosest mode in
    force while that tool call ran, from its first frame to its end. So an
    edit begun under `accept-edits` does not trip it after a switch to
    `ask`, and a command begun after a switch from `auto` to `ask` does.
    (Amended after Astra's review: the first wording judged by the whole
    turn, which let a command begun after tightening run unnoticed.)

25. **A shared model name goes to the provider that owns it.** Antigravity
    lists Claude and GPT models under the same bare IDs their own providers
    use, and its `gpt-oss` and `claude` IDs share a leading word with
    Codex's and Claude's. So that `/model claude-sonnet-4-6` and
    `/model gpt-6-astra` keep reaching Claude and Codex (decision 15),
    Antigravity is set aside when another provider also matches a bare
    name, unless Antigravity is the current provider or is named
    (`/model antigravity/claude-sonnet-4-6`).

26. **How the trust question is asked.** An untrusted workspace's session
    does not start; the message lists the files and says to type `/trust`
    (which records the trust and connects) or to pass
    `--trust-workspace-config` for one run. The phone has no `/trust`.

### After Astra's review of the plan (2026-11-11)

27. **The `.agents/` configuration is checked again before every prompt.**
    Octet records what the workspace held when the session started. A
    prompt, skill or compaction is not sent when that has changed or
    configuration has appeared; the message names the files and points to
    `/trust`. The reason: in `accept-edits` Antigravity may itself write
    `.agents/hooks.json` without asking, and a hook runs a command with the
    next prompt. `--trust-workspace-config` trusts what was there at the
    start, not later changes.
    - **Not closed:** a file that changes in the moment between Octet's
      check and Antigravity reading it. Closing it would need Antigravity
      to load a copy Octet controls, and it reads `<workspace>/.agents`
      only. Whoever can time such a change can already change any file the
      user's tools run.

28. **Only a reply naming the mode confirms a switch.** A reply to a mode
    switch that names no mode stops the session, like one naming another
    mode: Octet no longer knows the mode (decision 5).

29. **Octet's settings folder is never inside `~/.gemini`,** not through a
    link and not through `--journal-dir`. Octet resolves the folder before
    writing and refuses to start otherwise (decision 4).

30. **The tripwire's memory is bounded and fails safe.** Octet remembers
    1,024 tool calls a turn. In `ask` and `accept-edits`, a turn that makes
    more is cancelled with a message; the session goes on. When Octet
    itself cancels a turn, the turn ends only with Antigravity's reply to
    it, so a later prompt cannot be ended by that reply.

Declined from that review: carrying a model Antigravity moved to by itself
into a reconnect. A reconnect asks for the model the user chose, as for
every provider.

### After Astra's second review, and during the build (2026-11-11)

31. **What a session compares against is what was checked before the CLI
    started.** The fingerprint the trust check accepts is handed to the
    protocol. A baseline taken after the launch would accept configuration
    that changed between the check and the launch.

32. **Decision 29, tightened.** `~/.gemini` is forbidden by its path even
    when it does not exist yet, and is resolved again once Octet's folder
    exists. The folder must be the one Octet meant (no link from the
    `proceed` folder to the `review` one). Octet removes the settings that
    skip requests whatever the letter case of their keys.
    - **Not closed:** another process that swaps Octet's folder for a link
      in the moment between this check and the write. Closing it means
      creating and renaming through held directory handles, which Rust's
      standard library does not offer. Whoever can do that in the user's
      own Octet data folder can already write `~/.gemini` directly. Astra
      rated this Critical in both rounds; this is where the build does not
      follow it.

33. **The check before every prompt cannot hold Octet.** Only regular
    files are read (a named pipe is refused before it is opened) and the
    check has a deadline; past it the prompt is not sent.

34. **A probe's folder has one owner.** It is created exclusively; a
    second probe in the same process is refused and the first keeps its
    folder.

35. **A permission request rejected during a cancel still counts as
    asked,** so its tool call ending `failed` does not trip the session.

36. **The gate's subagent scenario proves a wait, not an absence.** It
    passes only when the subagent was started and was seen waiting for
    approval; anything less is `unproven`.

## Parity table

What the user gets, feature by feature.

| Octet feature | Antigravity |
| --- | --- |
| `--engine`, `/model`, provider handoff | Yes |
| `/effort` | Yes, where the model has a thinking level |
| `ask`, `accept-edits`, `auto`, `full-access` | Yes (decision 3) |
| Approvals: allow, deny, timeout, cancel | Yes |
| Cancel a turn | Yes |
| `--resume`, `/reconnect`, `/sessions`, `/resume` | Yes |
| `/fork` | No: refused with a clear note |
| `/compact` | Yes |
| Skills and commands | Yes |
| Images | Yes, inline |
| MCP tool calls | Yes, and they ask in `ask` |
| `/goal`, queued prompts | Yes (Octet's own) |
| `/steer` | No: the text is queued, as for opencode |
| `--print`, `--rpc`, journals | Yes |
| Hub and phone | Yes |
| `--no-plugins`, `--no-plugin-hooks` | Refused with a clear note unless the builder finds a switch: none was found for hooks |
| Usage | Tokens only |
| A workspace with `.agents/` configuration | Starts after the user trusts it (decision 19) |

## Constraints

- Follow `docs/rust/adding-a-provider.md`: one protocol file
  (`crates/octet-engine/src/live/antigravity.rs`) and one row in
  `Engine::ALL`. The shared driver does the rest.
- **Reuse the ACP code.** opencode's file already speaks ACP. Share what is
  common (framing, `session/update` handling, permission replies, tool-call
  mapping) instead of copying it; how to share is the builder's call, and
  opencode's behaviour and tests must not change.
- The guide's rules hold: never answer an approval yourself, keep every
  buffer bounded, never read vendor credentials, follow
  `.claude/skills/rust-engineer/SKILL.md`, and pass `make rust-check`.
- Linux parity: run lint and tests for Linux too, as the repo's habit is.
- Stray requests (a permission request outside a turn, an unknown method)
  get a reply from an `antigravity_stray_reply`. `initialize` advertises no
  `fs` or `terminal` capability, and such requests are refused.
- Record `agy` 1.5.5 as the verified version, the way opencode's verified
  versions are recorded.
- The Octet GitHub workflows stay disabled.

## Testing

- **Fake vendor:** `crates/octet-testkit/src/bin/protocol-child/` learns
  Antigravity's ACP dialect for the guide's scenarios: connect, a turn, an
  approval allowed, denied and timed out, a cancel, a live mode switch, a
  resume that re-applies settings, and a vendor error. Live tests go in
  `crates/octet-engine/tests/live.rs`.
  The fake also plays the failures: a command that completes with no
  request (the tripwire), a mode the vendor changes by itself, and an
  unverified version.
- **Unit tests:** the settings file Octet writes (the pin, what is dropped,
  what is copied), the model and effort mapping, the fork refusal, and the
  workspace trust check (found, trusted, changed, headless refusal).
- **Gate scenarios** against the real `agy acp`, on Gemini 3.8 Flash at
  `low`: `initialize`, `simple`, `interrupt`, `approval-allow`,
  `approval-deny`, `approval-interrupt`, `resume`, `compact`, and
  `lockdown` (with the user's settings file saying `always-proceed`, a
  command and an edit still ask under Octet's settings). Add `mcp` and
  `subagent` if the CLI makes them testable.
- **A real run** in the TUI (the tmux driver in the `rust-tui-live-testing`
  notes) and headless: Gemini 3.8 Flash completes a turn, asks before an
  edit in `ask`, writes in `accept-edits`, resumes, and takes a conversation
  over from a Claude or Codex session. Then the existing sanity set for
  Claude and Codex passes unchanged.

## Left to the builder

The implementation plan
(`docs/superpowers/plans/2026-11-11-antigravity-provider.md`) settles most
of these; what it leaves open it marks as a check against the real CLI.

- Which of the user's settings keys are safe to copy into Octet's file, and
  whether `trustedWorkspaces` matters over ACP. The `lockdown` scenario
  decides.
- What `agy` sends when it is not signed in, and the exact wording check
  for decision 14.
- Whether plugins can be turned off for `--no-plugins`
  (`--include-default-customizations=false` did not stop workspace hooks).
- The `shutdown_grace` in the middle of a turn (idle is timed above).
- Whether MCP tools ask in `ask`, if the CLI makes that testable.
- Whether Antigravity's `goal` command clashes with Octet's `/goal`; Octet's
  own wins.
