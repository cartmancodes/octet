# Plugins and skills in Octet: research

**Date:** 2026-10-12.
**CLIs:** Claude Code 2.1.289, codex-cli 0.154.0.
**Question:** how can Octet work with Claude Code and Codex plugins, such as obra/superpowers, and how do pi and opencode handle plugins?

## Summary

pi and opencode **own their agent loop**, so they host plugins themselves. Each has an extension API (TypeScript modules with lifecycle events, custom tools and commands) and native Agent Skills support.

Octet is different. It drives the vendors' own CLIs, so **the vendor CLI is the plugin host**. Octet's job is not to run plugins but to:

1. not block them;
2. show what they provide, in one provider-agnostic way;
3. invoke them;
4. keep Octet the approval authority.

Both CLIs already expose what that needs, and every mechanism below was tested on this machine.

## What Octet does today

- **Claude** starts with `--setting-sources=` and `--strict-mcp-config` ([claude.rs](../../crates/octet-engine/src/live/claude.rs)). That turns off user, project and local settings, so it also turns off **every plugin, settings hook and MCP server**.
  - Measured on this machine: 56 commands with Octet's flags, against 157 with normal settings.
  - The 101 missing include `superpowers:brainstorming` (and its 14 siblings) and `code-review:code-review`.
  - This is deliberate: [docs/tui.md](../tui.md) says the Claude preview "does not yet expose the existing hook/plugin/resource surface", and the [parity matrix](../rust/parity-matrix.md) wants "a security design first".
- **Codex** starts as a plain `codex app-server`, so it loads its normal configuration, plugins and skills. Here, `skills/list` returns 13 skills, among them plugin skills such as `pdf:pdf` and `visualize:visualize`.

## What the vendors expose (tested)

### Claude Code

| Need | Mechanism | Evidence |
|---|---|---|
| Load only chosen plugins, without the user's permission rules, hooks or default mode | Keep `--setting-sources=` and add `--settings '{"enabledPlugins":{"superpowers@claude-plugins-official":true}}'`, or `--plugin-dir <path>` | Both gave 71 commands, including the 15 `superpowers:*` ones. Plugins not listed stay off |
| Plugin hooks still run in that mode | Plugin `hooks/hooks.json` | A probe plugin's `SessionStart:startup` hook ran during the handshake, before any prompt |
| See hooks running | `--include-hook-events` | `system/hook_started` and `system/hook_response` on stdout |
| List commands and skills | The `initialize` reply's `commands` | Entries carry `name`, `description`, `argumentHint` and `aliases` (`superpowers:brainstorming`, alias `brainstorming`) |
| Invoke a skill | Send `/<plugin>:<skill> args` as ordinary prompt text over stream-json | A probe skill replied `PROBE_SKILL_RAN` (about $0.06 on Haiku) |
| Install and manage | `claude plugin install/enable/disable/marketplace …` | CLI help |

**Plugin components** ([reference](https://code.claude.com/docs/en/plugins-reference)):
- **Instructions only:** skills, commands, agents and output styles.
- **Run code:** hooks, MCP and LSP servers, monitors, and `bin/` executables placed on the Bash tool's `PATH`.

A plugin's `PreToolUse` hook can decide a permission. That is the security question: it can allow a tool call without Octet's approval dialog.

### Codex

The app-server protocol Octet already speaks has the whole surface:

- `skills/list` returns name, description, path, scope (`user`, `repo`, `system`, `admin`) and `pluginId`.
- `skills/changed` notifies when skills change.
- `skills/extraRoots/set` adds more skill folders; `skills/config/write` changes skill configuration.
- `plugin/list`, `plugin/installed`, `plugin/read`, `plugin/install` and `plugin/uninstall`, plus `marketplace/add`, `marketplace/remove` and `marketplace/upgrade`.
- `hooks/list`.
- A turn can **name a skill**: `UserInput {"type":"skill","name":…,"path":…}`, alongside a `mention` type.
- Skill folders ([docs](https://learn.chatgpt.com/docs/build-skills)): `.agents/skills` from the working directory up to the repository root, `~/.agents/skills`, `/etc/codex/skills`, and the bundled skills.
- `codex plugin add|list|remove|marketplace` on the command line.

## How others do it

**pi** ([skills](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/skills.md), [extensions](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/extensions.md), [packages](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/packages.md)):

- **Extensions** are TypeScript modules running in pi's own process.
  - They register tools, commands, shortcuts, providers and MCP servers.
  - They subscribe to events (`session_start`, `before_agent_start`, `tool_call` (which can block or rewrite a call), `context`, compaction).
  - They can rewrite the system prompt.
- **Skills** follow Agent Skills.
  - Found in pi's folders plus `~/.agents/skills` and `.agents/skills`.
  - Listed by name and description in the system prompt; forced with `/skill:name`.
- **Packages** bundle extensions, skills, prompts and themes.
  - Installed with `pi install npm:…|git:…|./path`, pinned, personal or per project.
  - Project packages load only after the project is trusted.
  - Per-resource filters: `pi config`.

**opencode** ([plugins](https://opencode.ai/docs/plugins/), [skills](https://opencode.ai/docs/skills/)):

- **Plugins** are JS/TS modules from `.opencode/plugins/`, `~/.config/opencode/plugins/`, or npm packages listed in `opencode.json`.
  - They provide event hooks (`tool.execute.before/after`, `permission.asked`, `session.compacted`, `experimental.session.compacting`) and custom tools.
- **Skills** are native, loaded on demand through a `skill` tool.
  - Read from `.opencode/skills`, **`.claude/skills`**, `.agents/skills` and their user-level equivalents.
  - Governed by `permission.skill` rules (`allow`, `deny`, `ask`).

**obra/superpowers** ([README](https://github.com/obra/superpowers)) is one `skills/` library with a thin adapter per harness:

| Harness | Adapter |
|---|---|
| Claude Code | `.claude-plugin/` plugin; a `SessionStart` hook (`startup\|clear\|compact`) injects `using-superpowers` as extra context |
| Codex | `.codex-plugin/plugin.json` (`"skills": "./skills/"`, no hooks) |
| pi | An extension that injects the same bootstrap at session start and after compaction |
| opencode | Its own plugin |
| All | Per-harness tool-name maps (`references/codex-tools.md`, `pi-tools.md`, …) |

**Agent Skills** ([agentskills.io](https://agentskills.io)) is the shared format: a folder with `SKILL.md` (frontmatter `name` and `description`) and optional `scripts/`, `references/` and `assets/`. Claude, Codex, pi and opencode all read it, and **`.agents/skills`** is the folder Codex, pi and opencode share.

## Options for Octet

1. **Pass plugins through.**
   - Claude loads the plugins **you** enabled, via an Octet-built `--settings '{"enabledPlugins":…}'` merged from your Claude user and project settings.
   - Octet keeps `--setting-sources=`, so your Claude permission rules, settings hooks and default mode still don't override Octet's approvals.
   - `--include-hook-events` journals hook activity.
   - Codex already **loads** its plugins; nothing changes there. (Skills on Codex
     still need work; see option 2.)
2. **Surface skills, provider-agnostic.**
   - One list built from Claude's `commands` and Codex's `skills/list`, labelled by provider.
   - A `/skills` picker in the style of `/model`, Tab completion and the palette.
   - Invoke: Claude gets `/name args` as the prompt text; Codex gets a `skill` input item.
   - Codex needs new work for this: discovery (`skills/list`), structured `skill` input, and
     `skills/changed` handled before the driver's per-thread filter, which today drops any
     notification whose `threadId` is not the session's
     ([codex.rs](../../crates/octet-engine/src/live/codex.rs)).
   - Picking a skill only the other provider has could offer a switch, as `/model` does.
3. **One Octet-managed skill library for both.**
   - Codex already reads `.agents/skills` and `~/.agents/skills`. `skills/extraRoots/set` can also add `~/.claude/skills`;
     it **replaces** the process's extra roots, so send the complete list, with absolute paths.
   - Sharing skills does not share a bootstrap: superpowers' Codex manifest declares `"hooks": {}`.
   - For Claude, Octet can pass `--plugin-dir` for a small generated wrapper plugin over a shared folder.
4. **Octet's own extension layer**, as in pi and opencode.
   - Octet doesn't own the model loop, so model-callable Octet tools would have to be an **MCP server** Octet gives both CLIs (Claude `--mcp-config`, Codex `-c mcp_servers…`).
   - Octet-side hooks could act on the event stream it already journals.
   - This is a larger subsystem: a later phase, if wanted.

**Recommended:** 1, then 2, then 3. Option 1 alone makes superpowers work in Octet's Claude sessions, and its bootstrap hook verifiably runs. Option 2 is the user-visible part, built from data both CLIs already return. Option 3 fixes "installed for Claude but not for Codex", which matters because Octet switches providers mid-conversation. Option 4 only if a real use appears.

## Security design (the matrix's open item)

- **Trust:**
  - Load only plugins you've already enabled in the vendor CLI, which is the trust you gave by enabling them there.
  - Never auto-enable or install.
  - **Enabling a plugin and trusting its hooks are separate decisions.** Mirroring Claude's
    `enabledPlugins` runs those plugins' hooks, exactly as `claude` itself does; Octet only
    observes them. A separate switch should turn hooks off: Claude's `disableAllHooks` setting
    and Codex's `--disable hooks` (stable feature flag). That also turns off bootstraps such as
    superpowers' `SessionStart`.
  - A `--no-plugins` flag restores isolation on both providers: Claude keeps
    `--setting-sources=` with no `enabledPlugins`; Codex gets `--disable plugins` (verified: 13
    skills became 6, and none from plugins). Tests and fake vendors use it.
- **Approvals:**
  - Octet answers only the approvals a vendor asks for, and that depends on the mode: in `ask`
    and `accept-edits` Octet's dialog decides; in `auto` the vendor's own reviewer or classifier
    decides; `full-access` never asks.
  - Codex requests Octet does not support yet: `item/tool/requestUserInput` and
    `mcpServer/elicitation/request` get a "not supported" error, and
    `item/permissions/requestApproval` an empty grant. Plugins that need them load but fail when
    used, so this work promises **skills**, not arbitrary plugin compatibility.
  - A plugin hook that decides a permission (`PreToolUse` allow or deny) shows up in Octet's transcript and journal via `--include-hook-events`.
  - Octet could list which enabled plugins bring hooks, MCP servers or `bin/` executables (`claude plugin details`, Codex `plugin/read` and `hooks/list`), and confirm the first time.
- **Installing:** do it through the vendor (`claude plugin install`, `codex plugin add`), never by Octet writing plugin files.
  The app-server's `plugin/list`, `plugin/read`, `plugin/install` and `plugin/uninstall` are
  documented as still under development, so any management through them belongs behind a version
  check or an experimental switch; skills (`skills/list`) do not depend on them.

## Open questions

- Should Octet merge project-scoped `enabledPlugins` (from `.claude/settings*.json`), which requires reading those files, or only user scope?
- Should the shared library be `.agents/skills` (already Codex's, pi's and opencode's) or an Octet folder?
- Superpowers' bootstrap adds context to every Claude session. Opt-in or default?
- Codex gets no `SessionStart` injection from superpowers' Codex plugin (it has no hooks). Should Octet give both providers the same bootstrap?
