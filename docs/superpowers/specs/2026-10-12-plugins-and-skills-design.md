# Plugins pass through, and one skills list: design

**Date:** 2026-10-12. **Branch:** `feat/plugins-skills`.
**Research:** [docs/research/2026-10-12-plugins-and-skills.md](../../research/2026-10-12-plugins-and-skills.md),
including a reviewed and verified correction pass.

## The problem

- Octet launches Claude with `--setting-sources=` and `--strict-mcp-config`. That switches off
  every plugin, hook and MCP server.
  - Your enabled `superpowers` and `code-review` plugins never load in Octet's Claude sessions:
    56 commands in Octet against 157 in `claude`.
  - Superpowers' start-up bootstrap never runs.
- Codex loads its plugins, but Octet cannot list or invoke their skills:
  - it sends only text and images;
  - it rejects slash commands it does not know ("Unknown command …").

## Goal

- **Step 1, pass-through:** a Claude session in Octet loads the plugins your Claude settings
  enable for that folder, and their hooks run as they do in `claude`.
  - Octet shows hook activity that matters.
  - Switches turn plugins, or only their hooks, off on both providers.
- **Step 2, one skills list:**
  - `/skills` lists every provider's skills, the current provider's first.
  - Up/Down and Enter run one.
  - Typing a listed skill's name runs it.
  - Picking another provider's skill switches to that provider, carrying the conversation, and
    runs it there.

**What this promises:** plugins load, and their **skills** work on both providers. It does not
promise that every plugin works. Some Codex plugins rely on requests Octet does not support yet
(`item/tool/requestUserInput`, MCP elicitation); they load but fail when used.

## Decisions (agreed)

1. **Which plugins:** mirror Claude.
   - The `enabledPlugins` maps from your Claude user, project and local settings files are
     merged with Claude's precedence: local over project over user.
2. **Default on.**
   - `--no-plugins` restores today's isolation on both providers.
   - `--no-plugin-hooks` keeps plugins and skills but stops their hooks.
3. **Hooks:** every hook run is journaled. The transcript shows a hook only when it failed,
   blocked something, or decided a tool approval.
4. **Invoking:**
   - The `/skills` picker.
   - Typing a skill's name or alias as a command runs it, when the current provider lists it.
   - Octet's own commands win any name clash, and other unknown `/words` still get the helpful
     error.
5. **Cross-provider:**
   - The picker lists both providers' skills, the other's from a cache fetched in the
     background as the model lists are.
   - Picking the other provider's skill switches provider, then runs it.

## Behaviour

### Loading (step 1)

| Setting | Claude launch | Codex launch |
|---|---|---|
| Default | `--setting-sources=` `--strict-mcp-config` (as today), plus `--settings '{"enabledPlugins":{…merged…}}'` and `--include-hook-events` | `app-server` (as today) |
| `--no-plugin-hooks` | As the default, with `"disableAllHooks": true` in `--settings` | `app-server --disable hooks` |
| `--no-plugins` | As today: no `--settings`, no hook events | `app-server --disable plugins` |

- **Settings files read**, all three optional:
  - `~/.claude/settings.json`;
  - `<workspace>/.claude/settings.json`;
  - `<workspace>/.claude/settings.local.json`.
- **Reading them:**
  - Only the `enabledPlugins` object is read; every other setting stays ignored.
  - An entry is `"name@marketplace": true|false`; a later file's value for a name replaces an
    earlier one's. Only names that end `true` are passed.
- **When a file is missing or unreadable:**
  - A missing file is no plugins from it.
  - An unreadable or malformed file is no plugins from it, plus a one-line notice at connect
    that names the file. It never stops the session.
- **What does not change:**
  - Your Claude permission rules, settings hooks, default mode and MCP servers stay off.
  - Octet answers the approvals a vendor asks for, and what a vendor asks depends on the mode.
    In `ask` and `accept-edits`, Octet's dialog decides. In `auto`, the vendor's reviewer or
    classifier decides. In `full-access`, nothing asks.
  - Plugin hooks run as in `claude`. A hook that decides a permission does so before Octet is
    asked; Octet reports it but does not override it. `--no-plugin-hooks` is the control.
- **Reach:** the switches apply to every launch: the TUI, `--print`, `--rpc`, and the
  model-list probes.

### Hooks

- **Events:** Claude's `system` frames `hook_started` and `hook_response` become
  `Event::Hook { name, event, outcome, detail }`. The journal record type is `hook`.
- **Shown in the transcript** only when it matters:
  - a hook that failed (non-zero exit, or an error outcome);
  - a hook that blocked;
  - a hook that decided a tool approval (`permissionDecision` in its output). Example:
    "Hook PreToolUse:Bash allowed the tool (Octet was not asked)".

  Start-up and other quiet hooks are journal-only.
- Codex hooks are out of scope: Octet sees no hook events from Codex.

### Skills (step 2)

- **Discovery:**
  - **Claude:** from the handshake's `commands`: `name`, `description`, `argumentHint`,
    `aliases`.
  - **Codex:** from `skills/list`: `name`, `description`, `path`, `scope`, `pluginId`. Only
    entries with `enabled: true` are kept.
  - Both drivers emit `Event::Skills(Vec<SkillInfo>)` once the session is ready.
  - Codex emits it again on `skills/changed`. That notification has no `threadId`, so it is
    handled before the driver's per-thread filter.
- **Cache:**
  - `skills.json` beside `models.json`, one list per provider and **workspace**, since skills
    depend on the folder.
  - The same atomic, private save and merge-with-newer rules as `models.json`.
- **Background fetch:** the model-list probe also asks for skills, at no extra launch.
  - Claude's handshake already carries `commands`.
  - Codex's catalog-only mode adds `skills/list` after `model/list`, still opening no thread.
  - It goes stale after 24 hours, as the model lists do.
- **`/skills` picker:** the `/model` picker's pattern, a popup above the prompt.
  - Each line is `name · provider · description` (cleaned), the current provider's skills
    first.
  - Typing after `/skills ` filters by name, alias or description.
  - Up/Down move, Enter runs the highlighted skill, Tab inserts `/name `, and Esc closes.
- **Typed names:** a draft `/NAME rest…` is a skill invocation when all of these hold:
  - `NAME` is not an Octet command;
  - the current provider lists `NAME` as a skill name or alias;
  - the draft is not a path (an existing rule).

  Octet commands are matched first; an unlisted name still gets "Unknown command …", now
  adding "/skills lists what each provider offers".
- **Running a skill:** a new engine `Command::Skill { name, path, args, display }`.
  - **Claude:** sent as prompt text `/name args`. A plugin skill runs this way over stream-json;
    this was tested.
  - **Codex:** sent as `turn/start` input `[{"type":"skill","name":…,"path":…},
    {"type":"text","text": args}]`, with the text item left out when `args` is empty.
  - The transcript and the journal's `user` record show `/name args`.
  - It is a turn like any prompt: it queues behind a running turn, can be steered on Codex, and
    can be interrupted.
- **Another provider's skill**, from the picker:
  - The pick is a provider switch to that provider's default model. The skill to run waits on
    the interface, and the pending conversation handoff applies as for any cross-provider switch.
  - The skill runs as the first turn once the new session is ready.
  - With a handoff pending:
    - **Claude:** gets `/name args`, then a blank line, then the transcript block, so the slash
      command still leads the message.
    - **Codex:** gets the skill item, then a text item of transcript and `args`.
  - If the switch fails, the skill is dropped with a note.
- **Stale entries:** a skill the cache lists but the vendor no longer has goes to the vendor
  anyway, and the vendor's own error is shown.

## Components

- **`octet-engine`:**
  - `Config.plugins: Plugins { load: bool, hooks: bool }` (default both `true`) and
    `Config.claude_plugins: Vec<String>` (the merged enabled names, filled by the caller).
  - The Claude launch flags as in the table.
  - The Codex `app-server` with `--disable plugins` or `--disable hooks` as in the table.
  - New types: `SkillInfo { name, description, aliases, argument_hint, path: Option<PathBuf> }`,
    `Event::Skills`, `Event::Hook`, and `Command::Skill`.
  - Claude: hook frames to `Event::Hook`, and `Event::Skills` from the handshake.
  - Codex: `skills/list` after the thread opens and in catalog-only mode; `skills/changed`;
    skill input items.
- **`octet-core`:**
  - `claude_settings::enabled_plugins(home, workspace) -> (Vec<String>, Vec<String> warnings)`,
    a pure merge over the three files, unit-tested.
  - A `skills` cache module alongside `catalog`.
  - The `hook` journal record.
- **`octet` (CLI):** the `--no-plugins` and `--no-plugin-hooks` flags.
- **`octet-tui`:**
  - The models state gains a skills list per provider.
  - The `/skills` picker (a new `composer::Kind::Skill`).
  - Typed-name routing in `submit_draft`.
  - Cross-provider pick: `Exit::Model`, plus a pending skill run on the kept interface.
  - Hook notes.
  - Docs and help.

## Error handling

| Case | Behaviour |
|---|---|
| A Claude settings file is malformed | Its plugins are skipped; a one-line notice at connect; the session starts |
| An enabled plugin is missing or fails to load | Claude's own error or warning is shown as it reports it; the session continues |
| A hook fails | Journaled; one transcript note naming the hook |
| A skill is stale or unknown to the vendor | The vendor's error is shown; no Octet-side retry |
| The cross-provider switch fails | The skill run is dropped with a note |
| `skills/list` fails | No Codex skills listed; the cached list stays; the session is unaffected |
| A Codex plugin needs `requestUserInput` or elicitation | As today: "Request declined"; documented as not yet supported |

## Testing

- **Unit:**
  - The settings merge: precedence, `false` overriding `true`, missing and malformed files.
  - The Claude launch flags for each switch setting.
  - The Codex launch flags.
  - Hook frames to events, and "notable" classification.
  - The skill cache round trip.
  - Typed-name routing: Octet command first, alias, unknown name, path-like draft.
  - Claude and Codex skill wire forms, with and without a handoff.
- **Engine (fake vendors):**
  - `Event::Skills` from both fakes.
  - `--disable plugins`/`hooks` in Codex's argv.
  - The fake Claude echoes `--settings` and `--include-hook-events`.
  - A Codex `skill` input item reaches the fake.
- **TUI:**
  - The `/skills` picker: open, filter, Enter runs, Tab inserts, other provider's skill
    switches and runs.
  - A notable hook shows a note; a quiet one does not.
- **Live (real CLIs, tiny prompts):**
  - Superpowers loads in Octet's Claude session: its `superpowers:*` skills are listed and its
    `SessionStart` hook runs.
  - A skill runs from the picker on each provider.
  - `--no-plugins` lists no plugin skills on either provider.
  - `--no-plugin-hooks` stops the hook.

## Out of scope

- Installing or managing plugins. The app-server's `plugin/*` calls are documented as still in
  development.
- A shared skill library (research step 3).
- Octet's own extension layer.
- Codex hook visibility.
- `requestUserInput` and MCP elicitation support (a follow-up feature).
- Managed (enterprise) Claude settings.
