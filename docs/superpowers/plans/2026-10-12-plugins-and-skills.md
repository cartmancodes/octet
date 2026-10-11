# Plugins pass through, and one skills list: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:**

- Claude sessions load the plugins your Claude settings enable, and their hooks run. Notable
  hooks show; switches turn plugins, or only hooks, off on both providers.
- A `/skills` picker and typed skill names run either provider's skills.

**Architecture:**

- **Engine:**
  - It computes Claude's enabled plugins from the settings files at launch and adds the flags.
  - It turns Claude hook frames into `Event::Hook`, and both CLIs' skill lists into
    `Event::Skills`.
  - It sends a new `Command::Skill` in each vendor's form.
- **`octet-core`:** journals the new events and caches skills per provider and workspace.
- **TUI:**
  - Routes typed names.
  - Adds a `/skills` picker on the `/model` picker's pattern.
  - Runs another provider's skill after the provider switch.

**Tech stack:** Rust 1.98.1, edition 2024, tokio, serde_json, ratatui. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-12-plugins-and-skills-design.md`.
**Research:** `docs/research/2026-10-12-plugins-and-skills.md`.

## Global constraints

- **The gate:** cargo always runs through `scripts/rust-env.sh`. The gate is `make rust-check`,
  and it must pass after every task.
  - Commit only inside `if make rust-check …; then …; fi`, never after `; echo`.
- **Linux too:** after any task touching `libc`, processes or shells, also run
  `scripts/rust-env.sh cargo clippy --locked --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`.
  After pushing, check `gh run list`.
- **TDD:** each new test is watched failing before its implementation.
- **Lints:** exceptions are `#[expect(lint, reason = "…")]`, never a bare `#[allow]`. Product
  lines are at most 120 columns.
- **Commits:** each message ends with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Stage by path.
- **No hard-coded vendor plugin or skill names** in product code. Names come from the
  settings files and the CLIs.
- **Real vendors:** tiny prompts; Codex with `--model gpt-5.5`. Never edit the user's Claude or
  Codex configuration.
- **Ruling recorded up front:** the enabled-plugins merge lives in `octet-engine`
  (`live/claude_settings.rs`), not `octet-core` as the spec's components list says.
  - The launch arguments are built in the engine, and `octet-core` depends on the engine, not
    the reverse.
  - Behaviour is unchanged. Cost if wrong: one module move.

## Review focus

1. **A Claude settings file that is unreadable, malformed, or not an object** where
   `enabledPlugins` should be: the session must still start, with one notice. That file's
   plugins are skipped (Task 1 test).
2. **A skill name that is also an Octet command** (`/model`, `/help`) must run Octet's command,
   never the skill. A name only the *other* provider lists must give the hint, not a silent
   switch (Task 6 test).
3. **A cross-provider skill pick when the switch fails** (CLI missing): the skill must be
   dropped with a note, and must never run on the wrong provider later (Task 6 test).
4. **A Codex `skills/changed` arriving mid-turn:** it must refresh the list without being
   mistaken for turn progress or dropped by the per-thread filter (Task 3 test).
5. **A skill run with a conversation handoff pending:** on Claude the `/name` must still lead
   the message; on Codex the transcript goes in the text item (Task 4 test).

---

### Task 1: Engine and CLI — plugin switches and launch flags

**Files:**

- Create: `crates/octet-engine/src/live/claude_settings.rs`.
- Modify:
  - `crates/octet-engine/src/live/mod.rs`: `Plugins`, `Config.plugins`, `mod claude_settings`.
  - `crates/octet-engine/src/live/claude.rs`: `launch_args`; a notice at connect.
  - `crates/octet-engine/src/live/codex.rs`: `launch_args`.
  - `crates/octet-core/src/model.rs`: `configure` carries `plugins`.
  - `crates/octet/src/args.rs`: `--no-plugins`, `--no-plugin-hooks`.
  - `crates/octet/src/main.rs`: help text.
- Test: unit tests in `claude_settings.rs`, `claude.rs`, `codex.rs` and `args.rs`;
  `crates/octet-engine/tests/live.rs`.

**Interfaces:**

- **Produces:**

  ```rust
  // octet_engine::live
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct Plugins { pub load: bool, pub hooks: bool }   // Default: both true
  pub struct Config { /* … */ pub plugins: Plugins }
  // live::claude_settings
  pub(crate) fn enabled_plugins(home: Option<&Path>, cwd: &Path) -> (Vec<String>, Vec<String>)
      // (sorted enabled names, warnings naming unreadable files)
  ```

- [ ] **Step 1: Write the failing tests**

  In `claude_settings.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      fn dir(name: &str) -> octet_testkit::TempDir {
          let d = octet_testkit::TempDir::new(name);
          std::fs::create_dir_all(d.path().join(".claude")).unwrap();
          d
      }
      #[test]
      fn local_overrides_project_overrides_user() {
          let home = dir("octet-cs-home");
          let work = dir("octet-cs-work");
          std::fs::write(home.path().join(".claude/settings.json"),
              r#"{"enabledPlugins":{"a@m":true,"b@m":true,"c@m":false}}"#).unwrap();
          std::fs::write(work.path().join(".claude/settings.json"),
              r#"{"enabledPlugins":{"b@m":false,"c@m":true}}"#).unwrap();
          std::fs::write(work.path().join(".claude/settings.local.json"),
              r#"{"enabledPlugins":{"c@m":false,"d@m":true}}"#).unwrap();
          let (names, warnings) = enabled_plugins(Some(home.path()), work.path());
          assert_eq!(names, ["a@m", "d@m"]);
          assert!(warnings.is_empty());
      }
      #[test]
      fn missing_and_malformed_files_are_skipped_with_a_warning() {
          let home = dir("octet-cs-bad");
          let work = dir("octet-cs-bad-work");
          std::fs::write(home.path().join(".claude/settings.json"), "not json").unwrap();
          std::fs::write(work.path().join(".claude/settings.json"),
              r#"{"enabledPlugins":["not","an","object"]}"#).unwrap();
          let (names, warnings) = enabled_plugins(Some(home.path()), work.path());
          assert!(names.is_empty());
          assert_eq!(warnings.len(), 2, "{warnings:?}");
          assert!(warnings[0].contains("settings.json"));
          assert!(enabled_plugins(None, work.path()).0.is_empty());
      }
  }
  ```

  In `claude.rs` tests, the launch flags for each switch setting, with a home where `x@m` is
  enabled. `launch_args` takes the home from `HOME`, so set up a testable seam:
  `launch_args_with_home(config, home)`, called by `launch_args`.

  ```rust
  #[test]
  fn plugin_switches_shape_the_launch() {
      let home = octet_testkit::TempDir::new("octet-claude-flags");
      std::fs::create_dir_all(home.path().join(".claude")).unwrap();
      std::fs::write(home.path().join(".claude/settings.json"), r#"{"enabledPlugins":{"x@m":true}}"#).unwrap();
      let mut config = Config::new(Engine::CLAUDE, "claude", std::env::temp_dir());
      let args = |c: &Config| launch_args_with_home(c, Some(home.path()))
          .into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
      let on = args(&config);
      let settings = &on[on.iter().position(|a| a == "--settings").unwrap() + 1];
      assert_eq!(settings, r#"{"enabledPlugins":{"x@m":true}}"#);
      assert!(on.contains(&"--include-hook-events".to_owned()));
      config.plugins.hooks = false;
      let no_hooks = args(&config);
      let settings = &no_hooks[no_hooks.iter().position(|a| a == "--settings").unwrap() + 1];
      assert_eq!(settings, r#"{"disableAllHooks":true,"enabledPlugins":{"x@m":true}}"#);
      config.plugins.load = false;
      let off = args(&config);
      assert!(!off.contains(&"--settings".to_owned()) && !off.contains(&"--include-hook-events".to_owned()));
      assert!(off.contains(&"--setting-sources=".to_owned()));
  }
  ```

  In `codex.rs` tests:

  ```rust
  #[test]
  fn plugin_switches_reach_the_app_server() {
      let mut c = Config::new(Engine::CODEX, "codex", ".");
      let args = |c: &Config| launch_args(c).into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
      assert_eq!(args(&c), ["app-server"]);
      c.plugins.hooks = false;
      assert_eq!(args(&c), ["app-server", "--disable", "hooks"]);
      c.plugins.load = false;
      assert_eq!(args(&c), ["app-server", "--disable", "plugins"]);
  }
  ```

  In `args.rs` tests: `--no-plugins` gives `plugins.load == false`; `--no-plugin-hooks` gives
  `plugins.hooks == false`; neither flag leaves both `true`.

- [ ] **Step 2: Run them to see the expected failure**

  Run: `scripts/rust-env.sh cargo test -p octet-engine --lib -p octet` (expected: a compile
  failure on `Plugins`, `enabled_plugins` and `launch_args_with_home`).

- [ ] **Step 3: Implement**

  `claude_settings.rs`:

  ```rust
  //! Which Claude plugins are enabled for a folder, read as Claude reads them: the
  //! `enabledPlugins` maps of the user's, the project's and the project's local
  //! settings, later files overriding earlier ones. Nothing else in them is read.
  use serde_json::Value;
  use std::{collections::BTreeMap, path::Path};

  /// The enabled plugin names (sorted), and a warning for each file that exists but
  /// could not be read as settings.
  pub(crate) fn enabled_plugins(home: Option<&Path>, cwd: &Path) -> (Vec<String>, Vec<String>) {
      let files = home.map(|h| h.join(".claude/settings.json")).into_iter().chain([
          cwd.join(".claude/settings.json"),
          cwd.join(".claude/settings.local.json"),
      ]);
      let mut enabled = BTreeMap::new();
      let mut warnings = Vec::new();
      for file in files {
          let Ok(bytes) = std::fs::read(&file) else { continue };
          let map = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| match v.get("enabledPlugins") {
              None => Some(serde_json::Map::new()),
              Some(Value::Object(map)) => Some(map.clone()),
              Some(_) => None,
          });
          match map {
              Some(map) => {
                  for (name, on) in map {
                      if let Some(on) = on.as_bool() {
                          enabled.insert(name, on);
                      }
                  }
              }
              None => warnings.push(format!(
                  "Claude plugin settings in {} could not be read; its plugins are skipped",
                  file.display()
              )),
          }
      }
      (enabled.into_iter().filter(|(_, on)| *on).map(|(name, _)| name).collect(), warnings)
  }
  ```

  In `mod.rs`:
  - Add the `Plugins` type, with `impl Default` returning both `true`.
  - Add the field `Config.plugins` with doc "Load the vendor's plugins, and run their hooks".
  - `Config::new` sets `Plugins::default()`.
  - Add `catalog_only`-style `plugins: Plugins::default()` to any `Config { … }` literal the
    build lists (`Selection::configure` copies `current.plugins`).

  `claude.rs` `launch_args`: move the body into
  `fn launch_args_with_home(config: &Config, home: Option<&Path>) -> Vec<OsString>`. After the
  fixed flags:

  ```rust
  if config.plugins.load {
      let (names, _) = super::claude_settings::enabled_plugins(home, &config.cwd);
      let mut settings = serde_json::Map::new();
      if !config.plugins.hooks {
          settings.insert("disableAllHooks".into(), Value::Bool(true));
      }
      settings.insert(
          "enabledPlugins".into(),
          Value::Object(names.into_iter().map(|n| (n, Value::Bool(true))).collect()),
      );
      args.extend(["--settings".into(), Value::Object(settings).to_string().into()]);
      args.push("--include-hook-events".into());
  }
  ```

  `serde_json::Map` keeps keys sorted, which the expected string relies on.
  `launch_args(config)` becomes:

  ```rust
  launch_args_with_home(config, std::env::var_os("HOME").map(PathBuf::from).as_deref())
  ```

  The notice at connect: in `ClaudeProtocol::initialize`, before sending:

  ```rust
  if core.config.plugins.load {
      let home = std::env::var_os("HOME").map(PathBuf::from);
      for warning in super::claude_settings::enabled_plugins(home.as_deref(), &core.config.cwd).1 {
          core.emit(Event::Notice(warning))?;
      }
  }
  ```

  `codex.rs`: `launch_args(config)` returns `app-server`, then `--disable plugins` when
  `!config.plugins.load`, else `--disable hooks` when `!config.plugins.hooks`.

  `args.rs`:
  - Parse `--no-plugins` and `--no-plugin-hooks` into `Args.plugins: Plugins`.
  - `config()` sets `plugins: self.plugins`.

  `main.rs` help: add `[--no-plugins] [--no-plugin-hooks]` to the usage line, and the line
  "Plugins: your enabled Claude plugins load; --no-plugins or --no-plugin-hooks turn them, or
  their hooks, off".

- [ ] **Step 4: Engine test with a logged fake Codex**

  In `crates/octet-engine/tests/live.rs`, a script vendor writes its arguments to `argv.log`:
  - The script: `echo "$@" > "$(dirname "$0")/argv.log"; exec '<protocol_child>' "$@"`.
  - Spawn a Codex session with `config.plugins.load = false` and wait for `Ready`.
  - Assert `argv.log` is `app-server --disable plugins`.
  - Watch it fail before implementing (it will, at Step 2), then pass.

- [ ] **Step 5: Gate and commit**

  Run: `make rust-check` (expected: exit 0), then:

  ```bash
  git add crates/octet-engine crates/octet-core/src/model.rs crates/octet/src
  git commit -m "Engine and CLI: load the Claude plugins your settings enable; --no-plugins and --no-plugin-hooks

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
  ```

### Task 2: Hooks become events; notable ones show

**Files:**

- Modify:
  - `crates/octet-engine/src/live/mod.rs`: `Event::Hook`, `HookOutcome`.
  - `crates/octet-engine/src/live/claude.rs`: hook frames.
  - `crates/octet-core/src/lib.rs`: the `hook` journal record.
  - `crates/octet-tui/src/app.rs`: the `Event::Hook` arm.
  - `crates/octet/src/headless.rs`: if its match is exhaustive.
- Test: `claude.rs` unit tests; `crates/octet-tui/src/app/tests.rs`.

**Interfaces:**

- **Produces:**

  ```rust
  /// A plugin or settings hook ran.
  Event::Hook { name: String, event: String, outcome: HookOutcome, detail: String }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum HookOutcome { Ran, Failed, Blocked, Decided(String /* "allow" | "deny" | "ask" */) }
  impl HookOutcome { pub fn notable(&self) -> bool { !matches!(self, HookOutcome::Ran) } }
  ```

- [ ] **Step 1: Failing tests**

  In `claude.rs`, a pure `fn hook_event(frame: &Value) -> Option<Event>` reading
  `hook_response` frames. `hook_started` gives `None`, since only the response says what
  happened.

  ```rust
  #[test]
  fn hook_responses_say_what_a_hook_did() {
      let frame = |exit: i64, outcome: &str, output: &str| json!({"type":"system","subtype":"hook_response",
          "hook_name":"PreToolUse:Bash","hook_event":"PreToolUse","exit_code":exit,"outcome":outcome,
          "output":output,"stderr":"boom"});
      let outcome = |f: Value| match hook_event(&f) { Some(Event::Hook { outcome, .. }) => outcome, other => panic!("{other:?}") };
      assert_eq!(outcome(frame(0, "success", "{}")), HookOutcome::Ran);
      assert_eq!(outcome(frame(1, "error", "")), HookOutcome::Failed);
      assert_eq!(outcome(frame(2, "success", "")), HookOutcome::Blocked);
      assert_eq!(outcome(frame(0, "success", r#"{"decision":"block"}"#)), HookOutcome::Blocked);
      assert_eq!(
          outcome(frame(0, "success", r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#)),
          HookOutcome::Decided("allow".into())
      );
      assert!(hook_event(&json!({"type":"system","subtype":"hook_started","hook_name":"x"})).is_none());
  }
  ```

  In `crates/octet-tui/src/app/tests.rs`:

  ```rust
  #[test]
  fn only_notable_hooks_reach_the_transcript() {
      let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
      app.event(Event::Hook { name: "SessionStart:startup".into(), event: "SessionStart".into(),
          outcome: octet_core::HookOutcome::Ran, detail: String::new() });
      assert!(!app.entries_text().contains("SessionStart"));
      app.event(Event::Hook { name: "PreToolUse:Bash".into(), event: "PreToolUse".into(),
          outcome: octet_core::HookOutcome::Decided("allow".into()), detail: String::new() });
      assert!(app.entries_text().contains("Hook PreToolUse:Bash allowed the tool (Octet was not asked)"),
          "{}", app.entries_text());
  }
  ```

- [ ] **Step 2: Run them to see the expected failure**

  Expected: compile failures (`Event::Hook`, `HookOutcome`, `hook_event`).

- [ ] **Step 3: Implement**

  `hook_event`:

  ```rust
  /// A `hook_response` frame as an event: what the hook did.
  fn hook_event(frame: &Value) -> Option<Event> {
      if frame["subtype"] != "hook_response" {
          return None;
      }
      let output = serde_json::from_str::<Value>(frame["output"].as_str().unwrap_or("")).unwrap_or(Value::Null);
      let decision = output.pointer("/hookSpecificOutput/permissionDecision").and_then(Value::as_str);
      let outcome = if frame["exit_code"] == 2 || output["decision"] == "block" {
          HookOutcome::Blocked
      } else if frame["outcome"] != "success" || frame["exit_code"].as_i64().is_some_and(|c| c != 0) {
          HookOutcome::Failed
      } else if let Some(decision) = decision {
          HookOutcome::Decided(limited(decision))
      } else {
          HookOutcome::Ran
      };
      Some(Event::Hook {
          name: limited(frame["hook_name"].as_str().unwrap_or("hook")),
          event: limited(frame["hook_event"].as_str().unwrap_or("")),
          outcome,
          detail: limited(frame["stderr"].as_str().unwrap_or("")),
      })
  }
  ```

  In `ClaudeProtocol::on_frame`: when `kind == "system"`, emit any `hook_event`. Keep the
  existing `init` handling.

  `octet-core` `record()`:

  ```rust
  Event::Hook { name, event, outcome, detail } => ("hook", json!({"name":name,"event":event,"outcome":format!("{outcome:?}"),"detail":detail}))
  ```

  Re-export `HookOutcome` from `octet-core`.

  TUI `App::event` arm:

  ```rust
  Event::Hook { name, outcome, detail, .. } => {
      let what = match &outcome {
          HookOutcome::Ran => return,
          HookOutcome::Failed => format!("Hook {name} failed{}", if detail.is_empty() { String::new() } else { format!(": {detail}") }),
          HookOutcome::Blocked => format!("Hook {name} blocked the action"),
          HookOutcome::Decided(d) if d == "allow" => format!("Hook {name} allowed the tool (Octet was not asked)"),
          HookOutcome::Decided(d) => format!("Hook {name} answered the approval: {d}"),
      };
      self.note(what);
  }
  ```

  If `event` is a `match` expression rather than statements, use the existing pattern, not
  `return`. Headless: `Event::Hook { .. } => {}` if its match is exhaustive (it has `_ => {}`
  today).

- [ ] **Step 4: Run tests, gate, commit**

  Run: `scripts/rust-env.sh cargo test -p octet-engine --lib -p octet-tui --lib`, then
  `make rust-check`, then:

  ```bash
  git commit -m "Hooks: Claude's hook frames become events; ones that matter show in the transcript

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
  ```

  (Stage the modified files by path.)

### Task 3: Skills discovery on both providers, and the probe

**Files:**

- Modify:
  - `crates/octet-engine/src/live/mod.rs`: `SkillInfo`, `Event::Skills`, `Listing`, `probe`.
  - `crates/octet-engine/src/live/claude.rs`: `Event::Skills` from the handshake.
  - `crates/octet-engine/src/live/codex.rs`: `skills/list`, `skills/changed`, catalog-only.
  - `crates/octet-testkit/src/bin/protocol-child/{claude,codex}.rs`: fake commands and
    `skills/list`.
  - `crates/octet-core/src/lib.rs`: the `skills` record and re-exports.
  - `crates/octet-tui/src/{app.rs,models.rs}`: compile fixes. `conn.skills` stores the active
    provider's live list; the probe now returns a `Listing`.
- Test: `crates/octet-engine/tests/live.rs`.

**Interfaces:**

- **Produces:**

  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct SkillInfo { pub name: String, pub description: String, pub aliases: Vec<String>,
      pub argument_hint: String, pub path: Option<PathBuf> }
  Event::Skills(Vec<SkillInfo>)
  #[derive(Debug, Default)]
  pub struct Listing { pub models: Vec<ModelInfo>, pub skills: Vec<SkillInfo> }
  pub async fn probe(config: Config, limit: Duration) -> Result<Listing, String>
      // was probe(engine, binary, cwd, limit)
  ```

- [ ] **Step 1: Teach the fakes**

  Test fixtures are not product code, so this step is not TDD.
  - **Fake Claude:** its `initialize` reply also carries
    `"commands":[{"name":"fixture-skill","description":"A fixture skill","argumentHint":"[topic]","aliases":["fs"]}]`.
  - **Fake Codex:** answer `skills/list` with
    `{"id":…,"result":{"data":[{"cwd":".","errors":[],"skills":[{"name":"fixture-codex-skill","description":"A Codex fixture skill","enabled":true,"path":"/tmp/fixture-codex-skill/SKILL.md","scope":"user"}]}]}}`.
  - Add a scenario constant `SKILLS_CHANGED = "skills-changed"` to `scenario.rs`. On that prompt,
    the fake Codex emits `{"method":"skills/changed","params":{}}`, then a normal reply.

- [ ] **Step 2: Failing engine tests** (in `live.rs`)

  ```rust
  #[tokio::test]
  async fn both_providers_list_their_skills() {
      for (engine, name) in [(Engine::CLAUDE, "fixture-skill"), (Engine::CODEX, "fixture-codex-skill")] {
          let mut c = config();
          c.engine = engine;
          let (handle, mut events, task) = spawn(c);
          let skills = wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
          let Event::Skills(skills) = skills else { unreachable!() };
          assert_eq!(skills[0].name, name);
          stop(&handle, task).await;
      }
  }

  #[tokio::test]
  async fn a_codex_skills_change_refreshes_the_list_mid_turn() {
      let (handle, mut events, task) = spawn(config());
      wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
      handle.send(Command::Prompt(scenario::SKILLS_CHANGED.into())).unwrap();
      wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
      wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
      stop(&handle, task).await;
  }

  #[tokio::test]
  async fn a_probe_lists_models_and_skills() {
      let c = Config::new(Engine::CODEX, octet_testkit::protocol_child(), std::env::temp_dir());
      let listing = probe(c, PROBE_LIMIT).await.unwrap();
      assert!(!listing.models.is_empty());
      assert_eq!(listing.skills[0].name, "fixture-codex-skill");
  }
  ```

  Update the four existing probe tests to the new signature:
  - `probe(Config::new(engine, script, temp_dir), limit)`;
  - read `.models` where they read the list.

- [ ] **Step 3: Run them to see the expected failure**

  Expected: compile failures (`Event::Skills`, `Listing`, the new `probe` signature).

- [ ] **Step 4: Implement**

  - **Claude:** in the `octet-init` response handler, before `Event::Models`, emit
    `Event::Skills(commands(&v["response"]["response"]["commands"]))`.
    - `commands()` maps each entry with a valid `name` to a `SkillInfo`, with `path: None`.
    - Each text is put through `limited`; at most 512 entries.
  - **Codex:**
    - Add `Outstanding::SkillsList`.
    - In `opened()`, after `request_models`, request `skills/list` with
      `{"cwds":[cwd],"forceReload":false}`.
    - In catalog-only mode, request it after the last model page, and emit `Ready` only when
      it answers (move the `Ready` emission there).
    - The handler emits `Event::Skills` from `result.data[*].skills[*]` where `enabled` is
      true, with `path: Some(path)`. An error reply emits nothing, and in catalog-only mode
      still emits `Ready`.
    - In `on_frame`, **before** the `threadId` filter: on `method == "skills/changed"`, request
      `skills/list` with `"forceReload": true`, then return. It is not counted as progress.
  - **`probe`:**
    - Takes `config: Config` and sets `config.catalog_only = true`.
    - Collects `Skills`, keeping the last.
    - Returns `Listing { models, skills }` at `ready && models`. Claude emits `Skills` before
      `Models`, and Codex emits `Ready` after `Skills`, so both are present by then.
  - **`octet-core`:**
    - The record `Event::Skills(list)` becomes `("skills", json!(list of {name, description, aliases}))`.
    - Re-export `SkillInfo`, `Listing`.
  - **TUI:**
    - `Connection.skills: Vec<SkillInfo>`; the `Event::Skills` arm sets it.
    - `models.rs` `Prober` builds the `Config` (`Config { plugins: self.plugins, ..Config::new(engine, binary, cwd) }`),
      with a new `Prober.plugins` taken from the run `Config`.
    - It stores `listing.models` as before. Skills are cached in Task 5.

- [ ] **Step 5: Run tests, gate, commit**

  Run: `scripts/rust-env.sh cargo test -p octet-engine -p octet-tui --lib`, then
  `make rust-check`. Commit:

  ```text
  Skills: both providers list their skills; a probe reads them with the models

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

### Task 4: Running a skill, in each vendor's form

**Files:**

- Modify:
  - `crates/octet-engine/src/live/mod.rs`: `Command::Skill`.
  - `crates/octet-engine/src/live/protocol.rs`: `send_skill`.
  - `crates/octet-engine/src/live/{claude,codex}.rs`: `send_skill`.
  - `crates/octet-engine/src/live/driver.rs`: dispatch.
  - `crates/octet-tui/src/handoff.rs`: `Handoff::context`.
  - `crates/octet-tui/src/vendor.rs`: `begin_turn` wraps a skill.
- Test: `live.rs` (logged fakes); `handoff.rs` and `tests.rs` units.

**Interfaces:**

- **Produces:**

  ```rust
  Command::Skill { name: String, path: Option<PathBuf>, args: String, display: String, context: Option<String> }
  // Protocol
  fn send_skill(&mut self, core: &mut Core, name: &str, path: Option<&Path>, args: &str, context: Option<&str>)
      -> impl Future<Output = Result<(), DriverError>> + Send;
  // handoff.rs
  impl Handoff { pub(crate) fn context(&self) -> &str }   // preamble and transcript, without the "new message" line
  ```

- [ ] **Step 1: Failing tests**

  - **Engine, Claude:** the logged fake Claude (the `tee` wrapper from the earlier probe
    tests) is sent
    `Command::Skill { name: "fixture-skill", path: None, args: "go".into(), display: "/fixture-skill go".into(), context: None }`.
    Assert `sent.log` has a user message whose text is exactly `/fixture-skill go`, and that a
    `User("/fixture-skill go")` event arrives.
  - **Engine, Codex:** the logged fake Codex is sent
    `Command::Skill { name: "fixture-codex-skill", path: Some("/tmp/fixture-codex-skill/SKILL.md".into()), args: "go".into(), display: "/fixture-codex-skill go".into(), context: Some("EARLIER".into()) }`.
    Assert `sent.log`'s `turn/start` input is
    `[{"type":"skill","name":"fixture-codex-skill","path":"/tmp/fixture-codex-skill/SKILL.md"},{"type":"text","text":"EARLIER\n\ngo"}]`.
  - **`handoff.rs`:** `context()` starts with `[Octet handoff]`, ends with the closing tag, and
    has no "The user's new message:".
  - **`tests.rs`, review focus 5:** after a cross-provider switch with a handoff pending,
    `app.begin_turn(&vendor, Command::Skill { …, context: None }, By::User, "sending")` records
    a `Command::Skill` whose `context` is `Some(handoff.context())`. The pending handoff is
    taken and the note is shown.

- [ ] **Step 2: Run them to see the expected failure**

  Expected: compile failures (`Command::Skill`, `send_skill`, `context`).

- [ ] **Step 3: Implement**

  - **`Command::Skill`:**
    - `starts_turn` is true; `turn_display` is `display`;
    - `prompt_bytes` is `display.len()`; `wire_bytes` is `args.len() + context.len() + name.len()`.
  - **Driver:**

    ```rust
    Command::Skill { name, path, args, display, context } => {
        if self.begin_turn(display)? {
            self.protocol.send_skill(&mut self.core, &name, path.as_deref(), &args, context.as_deref()).await?;
        }
        Ok(())
    }
    ```

  - **Claude `send_skill`:** text `/{name}` plus ` {args}` when there are args, plus
    `\n\n{context}` when there is a context; then `self.send_prompt(core, &text, &[]).await`.
  - **Codex `send_skill`:**
    - The input is a `skill` item when `path` is `Some`, else a text item `$` + name.
    - Then a text item of `context` and `args` joined by a blank line, when non-empty.
    - The rest is `send_prompt`'s `turn/start` code, factored into
      `fn turn_start(&mut self, core, input: Vec<Value>)`, which `send_prompt` also uses.
  - **`Handoff`:**
    - Store `context: String` (preamble and block, ending at the closing tag, with a newline).
    - `text` = `context` + `"\n{NEW_MESSAGE}\n\n"`.
    - `wrap` is unchanged.
  - **`App::begin_turn`:** a new match arm `(Some(h), Command::Skill { context: None, .. })`
    that sets `context: Some(h.context().to_owned())` and reports `Some((h.turns, h.bytes()))`,
    like the prompt arms.

- [ ] **Step 4: Run tests, gate, commit**

  Run: `scripts/rust-env.sh cargo test -p octet-engine -p octet-tui --lib`, then
  `make rust-check`. Commit:

  ```text
  Skills: run one in each vendor's form; a pending handoff goes with it

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

### Task 5: The skills cache

**Files:**

- Create: `crates/octet-core/src/skills.rs`.
- Modify:
  - `crates/octet-core/src/catalog.rs`: factor out `pub(crate) fn write_private(path, bytes) -> io::Result<()>`.
  - `crates/octet-core/src/lib.rs`: `pub mod skills;`.
  - `crates/octet-tui/src/models.rs`: skills per provider and workspace.
- Test: `skills.rs` and `models.rs` units.

**Interfaces:**

- **Produces:**

  ```rust
  // octet_core::skills
  pub const FILE: &str = "skills.json";
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct SkillCatalogs { /* HashMap<(Engine, PathBuf), (Vec<SkillInfo>, SystemTime)> */ }
  impl SkillCatalogs {
      pub fn load(path: &Path) -> Self;
      pub fn save(&mut self, path: &Path) -> io::Result<()>;   // merges newer entries from disk, as Catalogs does
      pub fn set(&mut self, engine: Engine, workspace: &Path, skills: Vec<SkillInfo>, at: SystemTime);
      pub fn get(&self, engine: Engine, workspace: &Path) -> Option<&[SkillInfo]>;
  }
  // TUI Models
  pub(crate) fn live_skills(&mut self, engine: Engine, workspace: &Path, skills: Vec<SkillInfo>, now: SystemTime) -> Option<String>;
  pub(crate) fn skills(&self, engine: Engine, workspace: &Path) -> &[SkillInfo];
  ```

- [ ] **Step 1: Failing tests**
  - **`skills.rs`:**
    - a round trip is private (mode 0600);
    - two workspaces are kept apart;
    - a malformed file is empty;
    - a far-future time is skipped;
    - a save keeps another window's newer entry.

    These mirror `catalog.rs`'s tests, with entries keyed `"<provider>\t<workspace>"`.
  - **`models.rs`:**
    - `live_skills` saves to `skills.json`;
    - a probe's `Listing.skills` is cached for the probe's workspace;
    - `skills(engine, ws)` returns them.
- [ ] **Step 2: Run to see the expected failure**

  Expected: a compile failure.
- [ ] **Step 3: Implement**
  - `skills.rs` follows `catalog.rs`:
    - the same validation;
    - names through `valid_identifier`;
    - text cleaned and cut to 512/2048 bytes;
    - at most 512 entries per list.
  - `write_private` is the shared temp-file-and-rename writer, now used by both files.
  - `Models`:
    - gains `skill_catalogs: SkillCatalogs` and `skill_path`;
    - `attach` loads both;
    - `probed` stores `listing.skills` under `(engine, prober.cwd)`;
    - `App::event(Event::Skills)` calls `live_skills` and shows its save note if any.
- [ ] **Step 4: Run tests, gate, commit**

  Commit message:

  ```text
  Skills: cache each provider's list per workspace

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

### Task 6: Typed names, the `/skills` picker, and another provider's skill

**Files:**

- Modify:
  - `crates/octet-tui/src/registry.rs`: the `/skills` command.
  - `crates/octet-tui/src/commands.rs`: unknown names try the current provider's skills;
    `/skills`.
  - `crates/octet-tui/src/composer.rs`: `Kind::Skill`, `skills_query`, `skill_items`.
  - `crates/octet-tui/src/input.rs`: picker refresh, Enter and Tab.
  - `crates/octet-tui/src/view.rs`: popup title and empty text.
  - `crates/octet-tui/src/app.rs`: `pending_skill`; `skill_entries()`.
  - `crates/octet-tui/src/lib.rs`: run the pending skill on ready; drop it if the switch failed.
- Test: `tests/keys.rs`, `tests/slash.rs`, `composer.rs`.

**Interfaces:**

- **Consumes:** `Command::Skill` (Task 4); `Models::skills` (Task 5); `conn.skills` (Task 3).
- **Produces:**

  ```rust
  // app.rs
  pub(crate) struct SkillRun { pub(crate) engine: Engine, pub(crate) skill: SkillInfo, pub(crate) args: String }
  pub(crate) pending_skill: Option<SkillRun>      // on App
  pub(crate) fn skill_entries(&self) -> Vec<(Engine, SkillInfo)>   // current provider's (live), then others' (cached for this workspace)
  pub(crate) fn workspace(&self) -> PathBuf    // the session's cwd; Connection gains `cwd: PathBuf`, set from the Config
  pub(crate) fn run_skill(&mut self, vendor: &dyn Vendor, skill: &SkillInfo, args: &str) -> Action
  ```

- [ ] **Step 1: Failing tests**

  The first four tests in `tests/slash.rs`; the rest in `tests/keys.rs`.

  ```rust
  #[tokio::test]
  async fn a_typed_skill_name_runs_on_the_current_provider() {
      let mut app = app();   // Codex
      crate::test_support::idle(&mut app);
      app.conn.skills = vec![skill("imagegen", &[], Some("/s/imagegen/SKILL.md"))];
      let vendor = RecordingVendor::default();
      assert!(matches!(commands::try_command(&mut app, &vendor, "/imagegen a cat").await, Some(Action::Continue)));
      let sent = vendor.sent.borrow();
      assert!(matches!(&sent[0], Command::Skill { name, args, .. } if name == "imagegen" && args == "a cat"));
  }
  #[tokio::test]
  async fn an_alias_runs_its_skill_and_octet_commands_win() {
      let mut app = app();
      crate::test_support::idle(&mut app);
      app.conn.skills = vec![skill("superpowers:brainstorming", &["brainstorming"], None),
                             skill("model", &[], None)];
      let vendor = RecordingVendor::default();
      let _ = commands::try_command(&mut app, &vendor, "/brainstorming").await;
      assert!(matches!(&vendor.sent.borrow()[0], Command::Skill { name, .. } if name == "superpowers:brainstorming"));
      // `/model` is Octet's: it shows the picker or the list, never the skill.
      let before = vendor.sent.borrow().len();
      let _ = commands::try_command(&mut app, &vendor, "/model list").await;
      assert_eq!(vendor.sent.borrow().len(), before);
  }
  #[tokio::test]
  async fn another_providers_skill_typed_gives_the_hint() {
      let mut app = app();   // Codex
      let ws = app.workspace();
      app.models.skill_catalogs.set(Engine::CLAUDE, &ws, vec![skill("brainstorming", &[], None)], SystemTime::now());
      let action = commands::try_command(&mut app, &RecordingVendor::default(), "/brainstorming").await;
      assert!(action.is_none(), "the draft stays");
      assert!(app.entries_text().contains("/brainstorming is a claude skill; /skills runs it there"));
  }
  #[tokio::test]
  async fn an_unknown_name_still_gets_the_helpful_error() {
      let mut app = app();
      let action = commands::try_command(&mut app, &RecordingVendor::default(), "/nope").await;
      assert!(action.is_none());
      assert!(app.entries_text().contains("/skills lists what each provider offers"));
  }
  /// A Codex interface with a live Codex skill and a cached Claude skill.
  fn skills_app() -> App {
      let mut app = app();
      crate::test_support::idle(&mut app);
      app.conn.skills = vec![skill("imagegen", &[], Some("/s/imagegen/SKILL.md"))];
      let ws = app.workspace();
      app.models.skill_catalogs.set(Engine::CLAUDE, &ws, vec![skill("brainstorming", &[], None)], SystemTime::now());
      app
  }
  #[tokio::test]
  async fn the_skills_picker_lists_both_providers_and_enter_runs() {
      let mut app = skills_app();
      let vendor = RecordingVendor::default();
      type_keys(&mut app, &vendor, "/skills").await;
      key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
      let picker = app.composer.completion.as_ref().expect("picker open");
      assert_eq!(picker.kind, composer::Kind::Skill);
      assert!(picker.items[0].starts_with("imagegen · codex"), "{:?}", picker.items);
      assert!(picker.items[1].starts_with("brainstorming · claude"), "{:?}", picker.items);
      key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
      assert!(matches!(&vendor.sent.borrow()[0], Command::Skill { name, .. } if name == "imagegen"));
  }
  #[tokio::test]
  async fn picking_another_providers_skill_switches_then_runs() {
      let mut app = skills_app();
      let vendor = RecordingVendor::default();
      type_keys(&mut app, &vendor, "/skills").await;
      key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
      key_action(&mut app, &vendor, key(KeyCode::Down)).await;
      let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
      assert!(matches!(&action, Action::Exit(Exit::Model(s)) if s.provider == Engine::CLAUDE));
      assert!(app.pending_skill.is_some());
      // The kept interface connects to Claude; on ready, the skill runs there.
      app.connection(&octet_core::Config::new(Engine::CLAUDE, "claude", "/tmp"), "journal".into());
      let claude = RecordingVendor::default();
      crate::session_event(&mut app, &claude, Event::Ready { session: String::new() }).await;
      assert!(matches!(&claude.sent.borrow()[0], Command::Skill { name, .. } if name == "brainstorming"));
      assert!(app.pending_skill.is_none());
  }
  #[tokio::test]
  async fn a_failed_switch_drops_the_pending_skill() {
      let mut app = skills_app();
      app.pending_skill = Some(SkillRun { engine: Engine::CLAUDE, skill: skill("brainstorming", &[], None), args: String::new() });
      app.connection(&octet_core::Config::new(Engine::CLAUDE, "claude", "/tmp"), "journal".into());
      let claude = RecordingVendor::default();
      crate::session_event(&mut app, &claude, Event::Error("claude: not found".into())).await;
      crate::session_event(&mut app, &claude, Event::Stopped).await;
      assert!(app.pending_skill.is_none());
      assert!(app.entries_text().contains("The skill /brainstorming was not run: the switch to claude failed"));
      // A later ready (after /reconnect) runs nothing.
      app.connection(&octet_core::Config::new(Engine::CLAUDE, "claude", "/tmp"), "journal".into());
      crate::session_event(&mut app, &claude, Event::Ready { session: String::new() }).await;
      assert!(claude.sent.borrow().is_empty());
  }
  ```

  `skill(name, aliases, path)` is a small test helper building a `SkillInfo` (empty
  description and argument hint). `type_keys`, `key_action` and `key` are the existing
  helpers in `tests/keys.rs`, where these three tests go.

- [ ] **Step 2: Run to see the expected failure.**
- [ ] **Step 3: Implement**
  - **`commands::try_command`:** when `Spec::find(name)` is `None` and the draft is not
    path-like:
    - If a `conn.skills` entry has `name == name[1..]` or an alias equal to it, return
      `Some(app.run_skill(vendor, &skill, argument))`.
    - Else, if another provider's cached skills have it, show the hint and return `None`.
    - Else, the existing error with " /skills lists what each provider offers." appended.
  - **`run_skill`:**
    - Builds `Command::Skill { name, path, args, display: format!("/{name}{}", if args.is_empty() { "" } else { " " } + args), context: None }`.
    - Sends it through `begin_turn`, or queues it while a turn runs, as `submit` does.
    - Returns `Action::Continue`.
  - **`/skills`:** a registry entry with usage "/skills [filter] · Up/Down choose, Enter runs"
    and description "Run a skill from any provider".
    - Bare `/skills` + Enter, or typing `/skills `, opens the picker. This mirrors the model
      picker:
      - `composer::skills_query`, the twin of `model_query`, for the word after `/skills `;
      - `skill_items(entries, query)` makes lines `name · provider · description`, filtered by
        name, alias or description;
      - `refresh_skill_picker` in `input.rs`.
    - Enter on a line:
      - If its provider is the current one, run the skill with no args.
      - Else check `can_reconnect`, set `pending_skill`, and return
        `Exit::Model(Selection { provider, model: None })`.
    - Tab inserts `/name ` (`name` alone when the line's provider is current, else the hint
      note).
  - **`lib.rs` `session_event`:**
    - On `Ready` while idle, if `pending_skill` is set and the engine matches, take it and run
      it.
    - On `Stopped` with a `pending_skill`, drop it with the note.
  - **`view.rs`:** the popup title " Skills ", and an empty list says
    " No skills yet · /model refresh fetches them".
- [ ] **Step 4: Run tests, gate, commit**

  Commit message:

  ```text
  TUI: /skills picker and typed skill names; another provider's skill switches first

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

### Task 7: Docs and the live check

**Files:**

- Modify: `docs/tui.md`, `README.md`, `CHANGELOG.md`, `docs/rust/tui-features.md`,
  `docs/rust/parity-matrix.md` (the plugin row).

- [ ] **Step 1: Docs.** In `docs/tui.md`:
  - Replace the "Claude preview launches with vendor setting sources disabled" paragraph with
    the loading table (what loads, the two switches, what stays off, approvals by mode).
  - Add a "Skills" section covering `/skills`, typed names, cross-provider runs, and what is
    not supported yet (Codex `requestUserInput` and elicitation).

  Then README features, CHANGELOG "Added", the features table, and the parity matrix row
  ("Plugins load; skills listed and run; management pending").
- [ ] **Step 2: Live check.** Release build, tmux, a throwaway `--cwd` and `--journal-dir`:
  1. `octet --engine claude`: the handshake lists `superpowers:*` skills, and the journal has
     a `hook` record for `SessionStart:startup`.
  2. `/skills`, type `sup`: superpowers' skills are listed.
  3. Run a tiny probe skill from a `--plugin-dir`-free path. Since only enabled plugins load,
     use a user skill instead: create `~/.claude/skills/octet-live-probe/SKILL.md` ("reply
     exactly LIVE_SKILL_OK"), and remove it after the check.
     - Run `/octet-live-probe`; the reply is `LIVE_SKILL_OK`.
  4. `/model codex gpt-5.5`, then `/skills`: Codex's skills are listed (e.g. `imagegen`). To
     keep tokens tiny, run a Codex user skill rather than a real one: create
     `~/.agents/skills/octet-live-probe/SKILL.md` ("reply exactly LIVE_CODEX_SKILL_OK"), run
     `/octet-live-probe` from the picker, check the reply, and remove the file after.
  5. `octet --no-plugins --engine claude`: no `superpowers:*` skills. `octet --no-plugins
     --engine codex --model gpt-5.5`: no skill has a plugin ID.
  6. `octet --no-plugin-hooks --engine claude`: no `hook` records.

  Record the results in the commit message.
- [ ] **Step 3: Gate, Linux clippy, commit**

  Commit message:

  ```text
  Docs: plugins pass through, and the skills picker

  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  ```

## Finish

- A final whole-branch review by a fresh reviewer on the most capable model, then one fix
  pass.
- Merge only when the user says. After any push, check `gh run list`.
