# opencode as a third provider: design

**Date:** 2026-10-21.

## Why

opencode gives free access to models Octet cannot reach today: DeepSeek,
GLM, MiniMax, Nemotron and others under its `opencode/*` provider, at $0 per
turn. It also routes to any provider the user has signed into through
opencode (here, `openai/*`). The user wants to pick these models in Octet's
`/model`, as they pick Claude's and Codex's models, and work in them under
Octet's permission modes.

## Goals

- `opencode` is a provider like `claude` and `codex`: `--engine opencode`,
  `/model`, `/mode`, the sidebar, the journals, handoff between providers,
  skills, and the hub and phone app.
- `/model` lists **every** model opencode reports, not only the free ones.
- A free model (for example `opencode/deepseek-v4.1-flash-free` or
  `opencode/glm-5.3-flash-free`) runs a full turn in Octet: streamed text,
  tool calls, approvals, usage, cancel.
- opencode follows Octet's four permission modes (see "Permission modes").
- **Name clashes:** where the same model is reachable through more than one
  provider, the `/model` list and Tab completion show the provider in
  brackets, as `GPT-5.5 (codex)` and `GPT-5.5 (opencode)`.

## Non-goals

- No hard-coded model list. As with Claude and Codex, the list comes from
  the CLI (`docs/superpowers/specs/2026-10-10-provider-agnostic-models-design.md`).
- No signing in, setting up credentials or handling API keys in Octet. opencode
  authenticates itself (`opencode auth login`); Octet never reads its
  credentials.
- No filtering or badging of free models beyond what opencode's own names
  already say ("… Free").

## Anti-goals

- opencode editing files or running commands without asking while Octet
  shows `ask`. That would be a failure even if everything else worked.
- Octet answering an opencode approval by itself.
- Breaking the existing `/model` forms for Claude and Codex.

## What was verified (opencode 1.18.25, 2026-10-21)

The integration point is `opencode acp`: Agent Client Protocol, JSON-RPC 2.0,
one JSON object per line on stdin/stdout. That is close to how Codex's
app-server is driven. Observed:

- `initialize` returns `agentCapabilities` with `loadSession: true`,
  `promptCapabilities.image: true`, and `sessionCapabilities` with `close`,
  `fork`, `list` and `resume`.
- `session/new {cwd, mcpServers: []}` returns `sessionId` and
  `configOptions`. One option has `id: "model"`, a `currentValue` (default
  `opencode/big-pickle`) and `options: [{value, name}]`, where `value` is
  `provider/model` (`opencode/glm-5.3-flash-free`, `openai/gpt-5.5`) and
  `name` is `"OpenCode/GLM-5.3-Flash Free"`. Another has `id: "mode"` with
  `build` (default) and `plan` (edits disallowed).
- `session/set_config_option {sessionId, configId: "model", value}` switches
  the model and returns the updated `configOptions`.
- After `session/new`, a `session/update` notification with
  `sessionUpdate: "available_commands_update"` lists commands and skills
  (`init`, `review`, the user's skills…).
- `session/prompt {sessionId, prompt: [{type: "text", text}]}` streams
  `session/update` notifications and then returns
  `{stopReason: "end_turn", usage: {inputTokens, outputTokens, totalTokens, cachedReadTokens}}`.
  The updates seen were:
  - `agent_message_chunk` (`content.text`);
  - `tool_call` and `tool_call_update` (`toolCallId`, `title`, `kind`,
    `status` pending → in_progress → completed/failed, `locations`,
    `rawInput`, `content`);
  - `usage_update` (`used`, `size`, `cost`).
- **Permissions.** With no override, opencode's default configuration wrote a
  file without asking. Launched with the environment variable
  `OPENCODE_CONFIG_CONTENT='{"permission":{"edit":"ask","bash":"ask","webfetch":"ask"}}'`,
  it sent a request (it has an `id`) `session/request_permission
  {sessionId, toolCall: {toolCallId, title, kind, locations, rawInput}, options}`.
  The reply `{"outcome":{"outcome":"selected","optionId":"reject"}}` stopped the
  write; the tool call ended `failed` with "The user rejected permission…",
  and the turn ended normally.
- `opencode/deepseek-v4.1-flash-free` and `opencode/glm-5.3-flash-free` both
  completed turns, with `cost.amount: 0`.
- `opencode models` prints the same IDs, one per line, without starting a session.

The fake vendor and the protocol gate should record the exact shape of
`options` in `session/request_permission`, `session/cancel`, and
`session/resume`/`session/load`. These were not captured above.

## Decisions

### Made by the user

1. opencode is "just another provider", wired like Claude and Codex.
2. It follows Octet's permission modes.
3. `/model` lists all its models.
4. A clash is **the same model through different providers**. When there is
   one, the display adds the provider name in brackets.
5. **`auto` means what it means for Claude and Codex: the vendor decides on
   its own.** opencode has no AI reviewer, so in `auto` Octet applies no
   permission override and no lockdown (`ask`/`accept-edits` lockdown below
   does not apply): the user's own opencode configuration decides. The user
   accepted that, with opencode's default configuration, this lets everything
   through. The `/mode` description must say so plainly, for example "opencode's
   own permission settings decide (its defaults ask for nothing)".
6. **The bracket display** is as proposed (see decision 12 for the rule):

   ```
   GPT-5.5 (codex)
   GPT-5.5 (opencode · openai)
   GLM-5.3-Flash Free · opencode          ← no clash, unchanged
   ```

### Claude's calls (the user may override)

7. **Protocol:** `opencode acp`, not `opencode serve` (HTTP) and not
   `opencode run --format json` (one process per turn, with no approvals).
8. **Permission modes,** passed as `OPENCODE_CONFIG_CONTENT` at launch,
   merged over the user's own opencode configuration:

   | Octet mode | opencode permission |
   | --- | --- |
   | `ask` | `edit`, `bash`, `webfetch`: `ask` |
   | `accept-edits` | `edit`: `allow`; `bash`, `webfetch`: `ask` |
   | `auto` | no override: the user's own opencode permission settings decide (decision 5) |
   | `full-access` | `"permission": "allow"` |

   If the user already sets `OPENCODE_CONFIG_CONTENT`, the two must be merged
   so that the permission keys come from Octet. The ACP `mode` option
   (`build`/`plan`) stays `build`.

   **Strict, as with Claude.** Claude launches with `--setting-sources=` and
   `--strict-mcp-config` "so Octet still answers the approvals"
   (`claude.rs`). opencode follows that line. In `ask` and `accept-edits`,
   Octet's settings must win over everything that could let opencode act
   without asking:
   - per-pattern rules;
   - agent-level `agent.<name>.permission` blocks;
   - a project `opencode.json` or `.opencode/` in the workspace;
   - MCP servers from the user's configuration;
   - permission keys not named above (`external_directory`, `task`,
     `websearch`…);
   - plugins with permission hooks.

   So the override uses a `"*": "ask"` wildcard (with `edit: "allow"` for
   `accept-edits`), applies the same to every agent, and turns off whatever
   project config, MCP and plugin loading opencode allows to be turned off.
   `--no-plugins` maps to `--pure`. A gate scenario proves each of these
   cannot bypass `ask`. Anything opencode does not let Octet turn off is
   reported to the user, not silently accepted.
9. **Mode switches reconnect.** opencode reads its permission settings at
   launch, so `/mode` and Shift+Tab end the process and start a new one that
   resumes the same session. The user sees a short pause, not an instant
   switch. The `/mode` descriptions say so.
10. **What a model is to Octet.** `ModelInfo.selection` and `ModelInfo.id`
   are the full opencode value (`opencode/glm-5.3-flash-free`), and `name`
   is opencode's name. The model is set with `session/set_config_option` after
   `session/new` or a resume, and `Event::Ready` reports the confirmed
   `currentValue`.
11. **The `opencode/` prefix.** `Selection::parse` already reads
   `PROVIDER/NAME` as provider plus model. For opencode, `opencode/X` must
   keep the whole string as the model, so that `/model opencode/glm-5.3-flash-free`
   selects that model rather than `glm-5.3-flash-free`. A bare name such as
   `/model openai/gpt-5.5` resolves through the catalog as today; so do
   `/model opencode openai/gpt-5.5` and `/model opencode` (opencode's
   default). A short name typed with no `/` (`glm-5.3-flash-free`) may resolve
   by matching the part after the slash, when exactly one ID in opencode's
   list ends with it.

   **Precedence (no existing name moves).** First, an exact match in any
   list, as today: `/model gpt-5.5` still goes to Codex when Codex lists it,
   and still stays with the current provider when that provider lists it.
   Only when no list has the name exactly does the suffix match apply. It
   ranks above the leading-word rule (`prefix_owners`), which keeps its place
   after it.
12. **Clash detection.** Two entries from different providers are the same
    model when their IDs match after dropping opencode's `provider/` part
    and comparing case-insensitively: `openai/gpt-5.5` and Codex's `gpt-5.5`.
    The same goes for any future provider whose IDs carry a sub-provider.
    Two opencode sub-providers serving the same model (`openai/gpt-5.5` and
    some `other/gpt-5.5`) are a clash too.

    Every clashing entry, and only those, is shown as `Name (engine)`. Where
    opencode's name carries its sub-provider (`OpenAI/GPT-5.5`), that part
    moves into the bracket: `GPT-5.5 (opencode · openai)`. This replaces the
    trailing ` · engine` on those rows, on every screen that shows a model
    row: the `/model` list (`app.rs` `show_models`), the picker
    (`composer.rs` `picker_items`) and the Tab popup. Each row's "Select"
    line still gives the selection that reaches its provider
    (`/model openai/gpt-5.5` for opencode, `/model codex gpt-5.5` for Codex).
    Rows without a clash stay as they are today.
13. **Provider row:** `name: "opencode"`, `title: "opencode"`,
    `default_binary: "opencode"`, `steer: false`, `inline_images: true`
    (ACP image content blocks are base64), `effort_live: false`,
    `efforts: &[]` (see "Left to the builder"), and `lists_models: true`.
    The probe is a short `initialize` + `session/new` that reads `configOptions`
    and the commands, then closes the session; `opencode models` is the fallback.
    The daily probe must not leave empty sessions in the user's opencode history.
    The gate checks whether `session/close` removes the session. If it does not,
    the probe deletes it if opencode allows that; if it can't, it lists with
    `opencode models` instead, and the display names fall back to the IDs.
14. **Skills** come from `available_commands_update`, like Claude's and
    Codex's skill lists. A skill run sends `/name args` as the prompt text.
15. **`/compact`** sends opencode's own compact command if it lists one. If it
    does not, it is refused with a clear note.
16. **Hub and phone:** the PATH discovery in `crates/octet/src/service.rs`
    (`TOOLS`) adds `opencode`, which installs to `~/.opencode/bin`, so a
    launchd-run hub can find it. The phone app's engine list
    (`apps/mobile/app/new.tsx`, `ENGINES`) adds `opencode`. `--engine`'s
    default stays `codex`.

## Constraints

- Follow `docs/rust/adding-a-provider.md`: one protocol file
  (`crates/octet-engine/src/live/opencode.rs`), one row in `Engine::ALL`,
  with the shared driver doing the rest. Add an `Engine::OPENCODE` constant
  only if code must name it.
- The rules in that guide hold: never answer an approval yourself, keep every
  buffer bounded, never read vendor credentials, follow
  `.claude/skills/rust-engineer/SKILL.md`, and pass `make rust-check`.
- Linux CI parity: run lint and tests for Linux too, as the repo's habit is.
- Stray requests from opencode (a permission request outside a turn, an
  unknown method) get a reply from an `opencode_stray_reply`, as Claude's and
  Codex's do. Requests for client capabilities Octet does not offer
  (`fs/*`, `terminal/*`) are refused; `initialize` advertises none of them.

## Testing

- The fake vendor (`crates/octet-testkit/src/bin/protocol-child/`) learns
  enough ACP for the scenarios in the guide: connect, a turn, an approval
  allowed, denied and timed out, a cancel, a mode switch (here a reconnect),
  and a vendor error. Add live tests in `crates/octet-engine/tests/live.rs`.
- Unit tests for the clash display, and for the resolution of
  `opencode/X`, `openai/X`, `opencode openai/X` and a bare suffix.
- Gate scenarios against the real `opencode acp`, recording the shapes listed
  as unverified above.
- **A real run:** in the TUI (the tmux driver in the `rust-tui-live-testing`
  notes), `/model opencode/deepseek-v4.1-flash-free` and
  `/model opencode/glm-5.3-flash-free` each complete a turn, ask before an
  edit in `ask`, and take the conversation along from a Claude or Codex
  session.

## Left to the builder

- The exact `session/request_permission` reply for allow and deny: which
  `optionId` (`once` versus `always`; Octet never picks "always"), read from
  the request's `options`.
- How a resume after a mode switch is done (`session/resume` or
  `session/load`), and what replayed history, if any, is ignored.
- Mapping `tool_call` kinds and titles to Octet's `ToolCall` name and
  target, and `usage_update` to `Event::Usage`.
- Effort: whether opencode's `--variant` can reach an ACP session (the
  `opencode acp` command has no such flag; a config key may exist). If not,
  `/effort` says opencode does not take it.
- The `shutdown_grace`, timed against the real CLI.
- Whether `/model` header counts need anything new for a list of a few dozen
  models (they should not).
