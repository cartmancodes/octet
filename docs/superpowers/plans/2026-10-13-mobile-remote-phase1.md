# Octet on your phone, Phase 1: bridge and hub — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** every running Octet terminal UI shares its session with a local `octet hub`. The hub
serves an authenticated WebSocket API on `127.0.0.1` (published with `tailscale serve`), so a
paired phone can:

- watch the transcript;
- answer approvals;
- send prompts, cancel and steer;
- change mode and model;
- resume or start sessions in tmux and end them.

**Architecture:**

- **`octet-core::remote`** holds the shared contract:
  - the protocol schema, command parsing and frame builders;
  - the run-directory registration format;
  - a TypeScript generator for the app.
- **`octet-tui`** gets `bridge.rs`. Each terminal UI listens on `<data dir>/run/<pid>.sock` and
  registers `<pid>.json`. It streams terminal UI–only state (`state`, `catalog`) and runs
  inbound commands through the same code paths as keys and slash commands.
- **New crate `octet-hub`:**
  - discovers bridges;
  - **tails each session's JSONL journal** for live events and replay, so `octet-core`'s
    `Session` is unchanged;
  - authenticates devices;
  - serves the WebSocket API;
  - runs `recent`, `start` and `end`.
- **`octet hub` subcommands:** `serve` (the default), `devices`, `revoke` and `watch`.
- **`/remote-control`:** reports the hub's state, and `/remote-control pair` shows a QR code.

**Tech stack:** Rust 1.98.1, edition 2024, tokio, serde_json (`Value`, no serde derive, as the
rest of the workspace). New dependencies:

| Crate | Where | Notes |
|---|---|---|
| `tokio-tungstenite` | hub | `default-features = false`, feature `handshake`; dev-dependency in `octet` with `connect` |
| `getrandom` | hub | |
| `sha2` | hub | |
| `qrcode` | TUI | `default-features = false` |

**Spec:** `docs/superpowers/specs/2026-10-13-mobile-remote-design.md`.

## Global constraints

- **The gate.** Cargo always runs through `scripts/rust-env.sh`. The gate is `make rust-check`
  and must pass after every task. Commit only inside
  `if make rust-check; then git commit …; fi`. The e2e test
  `remote_control_reports_a_ready_host_with_the_phone_command` can flake once under parallel
  load; re-run the gate if it alone fails.
- **TDD.** Each new test is watched failing before its implementation.
- **Lints.** Exceptions use `#[expect(lint, reason = "…")]`, never a bare `#[allow]`. Product
  lines are at most 120 columns. `rustfmt` decides layout. Each new item gets one doc line saying
  what it is for. Workspace lints include `missing_docs`, `unreachable_pub`, pedantic clippy and
  `unwrap_used`.
- **Dependencies.**
  - Only the four new crates above, each added with `scripts/rust-env.sh cargo add …` so
    `Cargo.lock` updates.
  - Every licence must be in `deny.toml`'s allow-list (`cargo-deny` runs in CI;
    `cargo deny check` locally if installed).
  - `cargo-machete` fails on unused dependencies.
- **Unix only.** The bridge and hub are `#[cfg(unix)]`, matching Octet's macOS/Linux support.
- **Secrets.** Device tokens, pairing secrets and push tokens are never logged, journaled or
  printed (except the pairing link shown by `/remote-control pair`). Every file the hub writes
  is created 0600 inside a 0700 directory, via `octet_store::{create_private_dir,
  create_private}`, or by writing a temporary file and renaming it into place.
- **No shell.** The hub launches processes (tmux, tailscale) only with `Command::new(...).args(...)`,
  never through a shell.
- **Commits.** Stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`
  changes the task did not make: an unrelated uncommitted test exists in the working tree. Each
  message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **Rulings recorded up front** (decisions the spec leaves open, or adjustments to it):
  1. The protocol schema and the run-directory format live in **`octet-core::remote`**, not in
     `octet-hub`. The terminal UI and the hub share them without the terminal UI linking the
     WebSocket stack. The TypeScript file is generated from that schema.
  2. The hub gets events by **tailing the journals**, not from the bridge, so `octet-core`'s
     `Session` stays unchanged.
     - A cursor is `{journal: <file name>, seq}`, because each reconnect starts a new journal
       numbered from 0.
     - `<pid>.json` lists every journal of that terminal UI, in order.
  3. The run directory is **`<data dir>/run/`**: the journal directory, so `--journal-dir`
     isolates tests. It is not `~/.local/share/octet/run`.
  4. Remote actions are audited in `<data dir>/hub-audit.jsonl` (time, device, session, command
     type, approval id or mode/model; never prompt text) and noted in the desk transcript.
     Session journals are not written to.
  5. A remote approval answer does not wait for the desk's 400 ms arm (the phone arms its own),
     but it must name an approval that is currently open.
  6. Phone prompts starting with `/` are refused ("Slash commands run on the desk"); mode and
     model have their own commands. `!` is plain text, as typed in the composer.
  7. The owner login comes from `tailscale status --json`: `Self.UserID` → `User[id].LoginName`.
     `octet hub --owner LOGIN` overrides it, for tests and development.
  8. `octet hub watch` and the pairing request use a local control socket,
     `<data dir>/run/hub.sock`, not the WebSocket. Local same-user trust is enough there.
  9. A session id is `<pid>-<start unix seconds>`, which is safe against pid reuse.
  10. A `full-access` change is refused unless `confirm == "full access"`. The hub checks it,
      and the bridge checks it again.

## Review focus

1. **A terminal UI that reconnects (`/model`, `/new`) while a phone is subscribed.** The phone
   must keep the same session and receive the new journal's events without missing or repeating
   records. Test: Task 6 `a_reconnect_continues_into_the_next_journal`.
2. **A malicious or broken client on the WebSocket.** Before `hello`, after a revoke, with a
   wrong login header, oversized frames, or a flood: it must be refused or disconnected, and
   must never reach a bridge. Test: Task 7 `unauthenticated_and_flooding_clients_are_refused`.
3. **A bridge whose terminal UI hangs, or a hub that stops reading.** The terminal UI must never
   block, and the slow peer is dropped. Test: Task 3 `a_hub_that_stops_reading_is_dropped`.
4. **`start` with a hostile workspace or arguments** (a path with spaces, `;`, `$()`, `..`, or
   not on the allow-list). It must be refused or passed as one literal argument. Test: Task 8
   `start_arguments_are_literal_and_workspaces_checked`.
5. **The journal tailer reading a half-written line or a file that shrank.** It must wait for
   the newline, and never emit a partial record or panic. Test: Task 6
   `the_tailer_waits_for_whole_lines`.

---

### Task 1: The shared protocol (`octet-core::remote::protocol`) and the TypeScript schema

**Files:**

- Create: `crates/octet-core/src/remote/mod.rs`, `crates/octet-core/src/remote/protocol.rs`
- Modify: `crates/octet-core/src/lib.rs` (add `pub mod remote;`)
- Create: `apps/mobile/src/protocol.ts` (generated; checked in)
- Test: unit tests in `protocol.rs`

**Interfaces:**

- Produces:

```rust
pub const VERSION: u64 = 1;
/// Which link a message travels on, and in which direction.
pub enum Direction { ToPhone, FromPhone, ToBridge, FromBridge }
/// One field of a message: its name, its type and whether it can be left out.
pub struct Field { pub name: &'static str, pub ty: Ty, pub optional: bool }
pub enum Ty { Str, U64, Bool, Json, StrList, Object(&'static str) }
/// One message type.
pub struct Message { pub kind: &'static str, pub direction: Direction, pub fields: &'static [Field], pub doc: &'static str }
pub static MESSAGES: &[Message];
/// A phone → hub command, checked.
pub enum PhoneCommand {
    Hello { token: String }, Pair { secret: String, name: String, platform: String },
    Subscribe { session: String, after: Option<Cursor> }, Unsubscribe { session: String },
    Prompt { session: String, text: String }, Answer { session: String, approval: u64, allow: bool },
    Cancel { session: String }, Steer { session: String, text: String },
    SetMode { session: String, mode: String, confirm: Option<String> },
    SetModel { session: String, selection: String },
    Recent { workspace: Option<String> },
    Start { engine: String, workspace: String, resume: Option<String>, model: Option<String>,
            mode: Option<String>, confirm: Option<String> },
    End { session: String },
}
pub struct Cursor { pub journal: String, pub seq: u64 }
/// Parses one phone message: `(req, command)`, or the reason it was refused.
pub fn parse_phone(text: &str) -> Result<(u64, PhoneCommand), String>;
/// A hub → bridge command (the subset a terminal UI runs), with the device that sent it.
pub enum BridgeCommand { Prompt{..}, Answer{..}, Cancel, Steer{..}, SetMode{..}, SetModel{..}, End }
pub fn bridge_command_json(req: u64, by: &str, command: &BridgeCommand) -> Value;
pub fn parse_bridge_command(line: &str) -> Result<(u64, String, BridgeCommand), String>;
/// Frame builders, each `{ "v": 1, "type": … }`.
pub fn ack(req: u64, message: &str) -> Value;
pub fn refused(req: u64, reason: &str) -> Value;
/// The checked-in TypeScript for the app, generated from `MESSAGES`.
pub fn typescript() -> String;
pub const MAX_MESSAGE: usize = 96 * 1024; // a 64 KiB prompt JSON-escaped, with room
pub const FULL_ACCESS_CONFIRM: &str = "full access";
```

- [ ] **Step 1: Write the failing tests**

In `protocol.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_phone_command_parses() {
        let cases = [
            r#"{"v":1,"req":1,"type":"hello","token":"t"}"#,
            r#"{"v":1,"req":2,"type":"pair","secret":"s","name":"Gemini 10","platform":"android"}"#,
            r#"{"v":1,"req":3,"type":"subscribe","session":"12-34","after":{"journal":"session-1.jsonl","seq":7}}"#,
            r#"{"v":1,"req":4,"type":"subscribe","session":"12-34"}"#,
            r#"{"v":1,"req":5,"type":"prompt","session":"12-34","text":"hi"}"#,
            r#"{"v":1,"req":6,"type":"answer","session":"12-34","approval":3,"allow":true}"#,
            r#"{"v":1,"req":7,"type":"cancel","session":"12-34"}"#,
            r#"{"v":1,"req":8,"type":"steer","session":"12-34","text":"also"}"#,
            r#"{"v":1,"req":9,"type":"set_mode","session":"12-34","mode":"full-access","confirm":"full access"}"#,
            r#"{"v":1,"req":10,"type":"set_model","session":"12-34","selection":"gpt-5.5"}"#,
            r#"{"v":1,"req":11,"type":"recent"}"#,
            r#"{"v":1,"req":12,"type":"start","engine":"claude","workspace":"/w","resume":"abc"}"#,
            r#"{"v":1,"req":13,"type":"end","session":"12-34"}"#,
            r#"{"v":1,"req":14,"type":"unsubscribe","session":"12-34"}"#,
        ];
        for (n, case) in cases.iter().enumerate() {
            let (req, _) = parse_phone(case).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(req, n as u64 + 1);
        }
        assert!(matches!(
            parse_phone(cases[2]).unwrap().1,
            PhoneCommand::Subscribe { after: Some(Cursor { seq: 7, .. }), .. }
        ));
    }
    #[test]
    fn malformed_messages_say_why() {
        for (text, wanted) in [
            ("not json", "JSON"),
            (r#"{"v":2,"req":1,"type":"hello","token":"t"}"#, "version 1"),
            (r#"{"v":1,"req":1,"type":"launch"}"#, "Unknown message launch"),
            (r#"{"v":1,"type":"hello","token":"t"}"#, "req"),
            (r#"{"v":1,"req":1,"type":"prompt","session":"s"}"#, "text"),
            (r#"{"v":1,"req":1,"type":"answer","session":"s","approval":"x","allow":true}"#, "approval"),
            (r#"{"v":1,"req":1,"type":"hello","token":"t","extra":1}"#, "extra"),
        ] {
            let error = parse_phone(text).unwrap_err();
            assert!(error.contains(wanted), "{text}: {error}");
        }
        assert!(parse_phone(&format!(r#"{{"v":1,"req":1,"type":"prompt","session":"s","text":"{}"}}"#,
            "x".repeat(MAX_MESSAGE))).unwrap_err().contains("too large"));
    }
    #[test]
    fn bridge_commands_round_trip() {
        let command = BridgeCommand::SetMode { mode: "auto".into(), confirm: None };
        let line = bridge_command_json(9, "Gemini 10", &command).to_string();
        let (req, by, parsed) = parse_bridge_command(&line).unwrap();
        assert_eq!((req, by.as_str()), (9, "Gemini 10"));
        assert_eq!(parsed, command);
    }
    #[test]
    fn the_checked_in_typescript_is_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps/mobile/src/protocol.ts");
        let wanted = typescript();
        if std::env::var_os("OCTET_WRITE_PROTOCOL").is_some() {
            std::fs::write(path, &wanted).unwrap();
        }
        let current = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(current, wanted, "run with OCTET_WRITE_PROTOCOL=1 to regenerate");
    }
}
```

(Add `#![expect(clippy::unwrap_used, reason = "…")]` on the test module if the crate denies it in
tests. Follow how existing core tests handle it.)

Run: `scripts/rust-env.sh cargo test -p octet-core remote`
Expected: FAIL to compile (the module does not exist).

- [ ] **Step 2: Implement**

- **`MESSAGES`.** A static table of every message in the spec's §2.
  - **Hub → phone:** `sessions`, `event`, `state`, `catalog`, `recent`, `ack`, `refused`, plus
    `welcome {device}` and `paired {token, device}`.
  - **Phone → hub:** the 14 commands above.
  - **Hub → bridge:** `prompt`, `answer`, `cancel`, `steer`, `set_mode`, `set_model`, `end`,
    each with `req` and `by`.
  - **Bridge → hub:** `state`, `catalog`, `ack`, `refused`.

  Each `Field` lists its name, type and whether it is optional. Every message also carries `v`
  (and `req` for commands); the parser handles those generically.
- **`parse_phone`.**
  - Size check first: over `MAX_MESSAGE` → "Message too large".
  - Parse with `serde_json::from_str::<Value>`.
  - Require `v == VERSION` ("Unsupported protocol version; this hub speaks version 1") and an
    integer `req`.
  - Find the message by `type` among the `FromPhone` messages ("Unknown message {type}").
  - Check every field against its `Field` entry: a missing required field → "Missing {name}";
    a wrong type → "{name} must be a {type}"; an unknown field → "Unexpected field {name}".
  - Build the `PhoneCommand` variant.
  - Keep the per-variant construction in one `match` on `kind`.
- **`bridge_command_json` / `parse_bridge_command`.** Mirror the above for the `ToBridge`
  messages, with `by` (device name) required. `BridgeCommand` derives
  `Debug, Clone, PartialEq, Eq`.
- **`typescript()`.** Emits `export const PROTOCOL_VERSION = 1;`, then for every message an
  `export interface <PascalKind> { v: 1; type: "<kind>"; <fields> }` (with `req: number` for
  commands), then unions `ToPhone`, `FromPhone`, `ToBridge`, `FromBridge`.
  - Type mapping: `Str` → `string`, `U64` → `number`, `Bool` → `boolean`, `Json` → `unknown`,
    `StrList` → `string[]`, `Object(name)` → the named interface. Optional fields use `?`.
  - Define the object shapes the frames reference as interfaces: `Cursor { journal: string; seq:
    number }`, `SessionInfo`, `StateInfo`, `CatalogInfo`, `RecentInfo`.
  - The output starts with `// Generated by octet-core (remote::protocol). Do not edit.` and
    ends with one newline.
- Run `OCTET_WRITE_PROTOCOL=1 scripts/rust-env.sh cargo test -p octet-core the_checked_in` once
  to create `apps/mobile/src/protocol.ts`, then run without the variable.

Run: `scripts/rust-env.sh cargo test -p octet-core remote`
Expected: PASS.

- [ ] **Step 3: Gate and commit**

```bash
if make rust-check; then
  git add crates/octet-core/src/lib.rs crates/octet-core/src/remote apps/mobile/src/protocol.ts
  git commit -m "Remote: one protocol schema for the bridge, the hub and the phone, with its TypeScript

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
fi
```

---

### Task 2: Run-directory registration (`octet-core::remote::run`)

**Files:**

- Create: `crates/octet-core/src/remote/run.rs`
- Modify: `crates/octet-core/src/remote/mod.rs`, `crates/octet-core/src/sessions.rs` (add `known_workspaces`)
- Test: unit tests in `run.rs` and `sessions.rs`

**Interfaces:**

- Produces:

```rust
/// `<data dir>/run`.
pub fn run_dir(directory: &Path) -> PathBuf;
/// What a running terminal UI publishes about itself.
pub struct Registration {
    pub id: String,            // "<pid>-<start unix secs>"
    pub pid: u32, pub started: u64,
    pub socket: PathBuf,       // <run>/<pid>.sock
    pub engine: String, pub workspace: PathBuf, pub model: String, pub mode: String,
    pub vendor_session: String,
    pub journals: Vec<PathBuf>, // every journal of this TUI, oldest first
    pub from_phone: bool,      // started by the hub's `start`
}
impl Registration {
    pub fn to_json(&self) -> Value;
    pub fn from_json(value: &Value) -> Option<Self>;
    /// Writes `<run>/<pid>.json` atomically (temp file 0600, then rename).
    pub async fn write(&self, run: &Path) -> std::io::Result<()>;
    pub async fn remove(&self, run: &Path);
}
/// Every registration in `run`, unparsable files skipped.
pub async fn registrations(run: &Path) -> Vec<Registration>;
/// Workspaces of the newest 400 journals in `directory`, newest first, without repeats.
pub async fn known_workspaces(directory: &Path) -> Vec<PathBuf>; // in sessions.rs, re-exported
```

- [ ] **Step 1: Failing tests**

`registration_round_trips_and_writes_privately`:

- Write a `Registration` to a temp run dir created with `create_private_dir`.
- Read it back with `registrations` and assert it is equal.
- Assert file mode `0o600` (`std::os::unix::fs::PermissionsExt`).
- `remove` it, then `registrations` is empty.
- A garbage `x.json` in the directory is skipped.

`known_workspaces_lists_each_once_newest_first`: write three journals with `Journal::create` +
`append("session", {"cwd": …})` for `/a`, `/b`, `/a`, and assert `["/a", "/b"]` in newest-first
order. Use real `octet_store::Journal`s, like `sessions.rs`'s existing tests.

Run: `scripts/rust-env.sh cargo test -p octet-core run::` → FAIL.

- [ ] **Step 2: Implement.**
  - `write`: serialise with `to_json`, write `<pid>.json.tmp` via `create_private` (removing a
    stale tmp first), then `tokio::fs::rename`.
  - `from_json`: returns `None` on any missing or mistyped field.
  - `known_workspaces`: reuses `recent_sessions`'s scan (`journal_stamp` + `read_summary`, newest
    first, `SCAN_LIMIT`). Factor the shared "newest journals" loop into a private helper rather
    than duplicating it.

Run → PASS.

- [ ] **Step 3: Gate and commit**: `git add crates/octet-core/src`; message
  "Remote: a running interface registers itself in the run directory; known workspaces from the journals".

---

### Task 3: The bridge's socket, registration and state frames (`octet-tui`)

**Files:**

- Create: `crates/octet-tui/src/bridge.rs`
- Modify:
  - `crates/octet-tui/src/lib.rs`: `mod bridge;`, a `run` options parameter, a bridge owned by
    `run`, registration on each connection.
  - `crates/octet-tui/src/event_loop.rs`: `run_session` takes `&mut Bridge`; publish after each
    wake.
  - `crates/octet/src/main.rs` and `crates/octet/src/args.rs`: the `--no-remote` flag, and the
    call `octet_tui::run(config, directory, octet_tui::Options { remote })`.
- Test: unit tests in `bridge.rs`; update `crates/octet/tests/cli.rs` if it checks `--help`
  text.

**Interfaces:**

- Consumes: `octet_core::remote::{run_dir, Registration, protocol::*}` (Tasks 1–2).
- Produces:

```rust
pub struct Options { pub remote: bool }           // octet_tui public API
pub(crate) struct Bridge { .. }                    // None-like when disabled
impl Bridge {
    pub(crate) async fn start(directory: &Path, enabled: bool) -> Bridge; // never fails the TUI: on error, disabled + note
    pub(crate) async fn connected(&mut self, app: &App, config: &Config, journal: &Path); // updates <pid>.json
    pub(crate) fn publish(&mut self, app: &App);  // sends state/catalog when changed
    pub(crate) async fn next_command(&mut self) -> Remote; // pending forever when disabled
    pub(crate) fn reply(&mut self, req: u64, result: Result<String, String>);
    pub(crate) async fn stop(self);               // removes socket + json
}
pub(crate) struct Remote { pub(crate) req: u64, pub(crate) by: String, pub(crate) command: BridgeCommand }
pub(crate) fn state_json(app: &App) -> Value;    // { phase: connecting|idle|running|stopped, queue:[first lines], queued, goal:{objective,status,turns,max}|null, now, mode, approvals:[{id, summary}] }
pub(crate) fn catalog_json(app: &App) -> Value;  // { modes:[labels], current_mode, models:[{engine,selection,name,id}], current:{engine,selection}|null }
```

- [ ] **Step 1: Failing tests** (`bridge.rs`):

```rust
#[tokio::test]
async fn a_bridge_registers_listens_and_cleans_up() {
    let dir = tempdir();                                   // use the crate's existing temp-dir helper or std::env::temp_dir()+unique
    let mut bridge = Bridge::start(&dir, true).await;
    let app = crate::test_support::app_for(octet_core::Engine::DEMO);
    let config = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    bridge.connected(&app, &config, &dir.join("session-1.jsonl")).await;
    let run = octet_core::remote::run_dir(&dir);
    let regs = octet_core::remote::registrations(&run).await;
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].journals, [dir.join("session-1.jsonl")]);
    let mode = std::fs::metadata(&run).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    // A hub connects and is sent state and catalog at once.
    let stream = tokio::net::UnixStream::connect(&regs[0].socket).await.unwrap();
    let mut lines = tokio::io::BufReader::new(stream).lines();
    bridge.publish(&app);
    let first: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
    assert!(matches!(first["type"].as_str(), Some("state" | "catalog")));
    bridge.stop().await;
    assert!(octet_core::remote::registrations(&run).await.is_empty());
    assert!(!regs[0].socket.exists());
}
#[tokio::test]
async fn state_frames_go_only_when_state_changes() { /* publish twice with no change → one state frame; app.composer.queue push → a second */ }
#[tokio::test]
async fn a_hub_that_stops_reading_is_dropped() {
    // Connect, never read; publish 10_000 changed states (vary the queue); the bridge must
    // return promptly (assert under 1 s with tokio::time::timeout) and close that connection
    // (reading later yields EOF after the buffered lines).
}
#[test]
fn state_and_catalog_describe_the_session() {
    // app with a queued prompt, a goal, an approval (test_support::approval) →
    // state_json has queue ["…"], approvals [{id:1, summary:{action:"Run a shell command",…}}], now "…";
    // catalog_json lists Mode::ALL labels and app.model_entries() rows with current marked.
}
```

Run: `scripts/rust-env.sh cargo test -p octet-tui bridge` → FAIL.

- [ ] **Step 2: Implement.**
  - **Starting.** `Bridge::start` creates `run_dir` with `octet_store::create_private_dir` and
    binds `UnixListener` at `<run>/<pid>.sock`, removing a stale file first. Accepted connections
    go into a `Vec<Peer>`, each with an `mpsc::Sender<String>` of capacity 256 drained by a
    writer task into the socket. A `try_send` that fails (`Full`) drops that peer. On a new peer,
    the last state and catalog lines are queued for it at once. A reader task per peer parses
    lines with `parse_bridge_command` into an `mpsc::Sender<Remote>` (capacity 64) owned by the
    bridge; a bad line is answered with `refused`.
  - **`connected`.** Appends the journal to `journals`, updates engine, model, mode and
    workspace from `app.conn` and `config`, and writes the registration.
  - **`publish`.** Builds `state_json(app)` and `catalog_json(app)` and sends each only when it
    differs from the last sent.
  - **Errors.** Any error in `start` gives a disabled bridge and a one-line note
    ("Remote control is off: {error}"), shown by the caller.
  - **`stop`.** Closes the listener and removes the socket and the JSON file.
  - **Wiring in `lib.rs`.**
    - `run(config, directory, options)` starts the bridge before the loop.
    - After `app.connection(…)`, call `bridge.connected(&app, &config, session.journal())`.
    - Pass `&mut bridge` to `run_session`.
    - Call `bridge.stop()` before returning.
  - **Wiring in `event_loop.rs`.** Add `Wake::Remote(bridge::Remote)` and a select arm
    `remote = bridge.next_command() => Wake::Remote(remote)`. For now its handler replies
    `refused(req, "Not yet supported")`; Task 4 fills it in. Call `bridge.publish(app)` at the
    end of `handle` (after `self.dirty = true`) and after `input`.
  - **CLI.** Add `--no-remote` to `args.rs`'s flat match, its usage line in `main.rs`'s `HEAD`,
    and to `Args`; pass it through to `octet_tui::run`.

Run → PASS; then `scripts/rust-env.sh cargo test -p octet` (the CLI tests).

- [ ] **Step 3: Gate and commit**: `git add crates/octet-tui/src crates/octet/src crates/octet/tests`;
  message "TUI: each interface shares its session over a private socket; state and catalog frames".

---

### Task 4: The bridge runs remote commands through the terminal UI's own paths

**Files:**

- Modify:
  - `crates/octet-tui/src/bridge.rs`: `handle_remote`.
  - `crates/octet-tui/src/event_loop.rs`: the `Wake::Remote` arm calls it and applies the
    returned `Action`.
  - `crates/octet-tui/src/input.rs`: extract `answer_approval` from `approval_key`.
  - `crates/octet-tui/src/commands.rs`: make `mode_command` and `model_command` `pub(crate)`.
- Test: `crates/octet-tui/src/tests/remote.rs` (new module, declared in `tests.rs`).

**Interfaces:**

- Consumes: `Remote`, `BridgeCommand` (Tasks 1, 3); `input::{submit, cancel_turn}`,
  `commands::{steer, mode_command, model_command, can_reconnect}`.
- Produces:

```rust
/// Runs one remote command as the matching key or slash command would; the reply's text, or the refusal.
pub(crate) async fn handle_remote(app: &mut App, vendor: &dyn Vendor, remote: Remote) -> (Result<String, String>, Action);
/// Answers approval `id` (any open one, not only the front); Err when none is open with that id.
pub(crate) fn answer_approval(app: &mut App, vendor: &dyn Vendor, id: u64, allow: bool) -> Result<(), String>;
```

- [ ] **Step 1: Failing tests** (`tests/remote.rs`). Use `RecordingVendor`, `app()`,
  `test_support::{idle, approval}` and `skill`. One test per row:

| Test | Setup → expectation |
|---|---|
| `a_remote_prompt_sends_or_queues_like_enter` | Idle → `Command::Prompt`-family sent, `Ok`. Running → queued (`composer.queue.len() == 1`), `Ok("Queued (1 waiting)")`. Queue full → `Err` with the full-queue text. |
| `a_slash_prompt_is_refused` | `"/model x"` → `Err("Slash commands run on the desk")`; nothing sent. |
| `a_remote_answer_needs_an_open_approval` | Two approvals 1 and 2 open; answer 2 → `Command::Answer{id:2,allow:true}` sent, approval 2 removed, 1 stays at the front. Answer 9 → `Err("No approval 9 is waiting")`. The desk arm is not required. |
| `remote_cancel_mirrors_esc` | Running → `cancel_turn` path (cancel flag set, `Cancelling…` status), `Ok`. Idle → `Err("No turn is running")`. |
| `remote_steer_mirrors_the_steer_command` | Codex running → `Command::Steer`. Claude running → queued with the "cannot steer" note. |
| `remote_mode_changes_follow_the_mode_command` | Idle, `auto` → `Command::SetMode(Auto)`, `Ok`. `full-access` without `confirm` → `Err("Full access needs confirm: \"full access\"")`. With the confirm while idle → `Action::Exit(Exit::Mode(FullAccess))`. A turn open → `Err(TURN_OPEN)` for full-access. |
| `remote_model_changes_follow_the_model_command` | A switch to another provider's model returns `Action::Exit(Exit::Model(..))`. During a turn → `Err(TURN_OPEN)`. |
| `remote_end_refuses_during_a_turn` | Idle → `Action::Exit(Exit::Quit)`. Running → `Err(TURN_OPEN)`. |
| `remote_actions_are_noted_on_the_desk` | After an allowed answer by "Gemini 10", `entries_text()` contains "Allowed from Gemini 10". |

Run: `scripts/rust-env.sh cargo test -p octet-tui remote::` → FAIL.

- [ ] **Step 2: Implement.**
  - **`answer_approval`.** Finds the approval by id in `overlay.approvals`, sends
    `Command::Answer`, removes it, and calls `front_changed()` if it was the front.
    `approval_key` keeps its arm check and then calls `answer_approval(app, vendor, id, allow)`,
    so the desk behaves exactly as before.
  - **`handle_remote`.**
    - **Prompt.** Starts with `/` → `Err`. Otherwise `submit(app, vendor, text)`: `true` →
      `Ok(app.status_line.clone())` (the "Queued (n waiting)" hint, or empty), `false` →
      `Err(app.status_line.clone())`.
    - **Mode.** Parse with `Mode::parse` ("Unknown mode …" → `Err`). `FullAccess` without the
      exact confirm → `Err`. Run the same precondition checks as `mode_command`, then call
      `mode_command(app, vendor, label)`. Turn its `Action` and status line into the reply:
      `Exit` → `Ok("Reconnecting in {mode}")`, a pending switch → `Ok("Switching to {mode}")`,
      otherwise `Err(status_line)`.
    - **Model.** The selection must be one token with no whitespace or control characters; then
      `model_command(app, selection)`, interpreted the same way.
    - **The rest.** Each follows its row in the table.
  - **Desk notes.** Every `Ok` leaves a note: "{Action} from {by}", for example "Prompt from
    Gemini 10", "Allowed from iPhone 15", "Mode auto from iPhone 15".
  - **Event loop.** The arm calls `handle_remote`, then `bridge.reply(req, result)`, then
    `self.apply(action, app)`.

Run → PASS; the full `octet-tui` suite passes.

- [ ] **Step 3: Gate and commit**: message "TUI: remote prompts, answers, cancels, steers, mode and model changes run the desk's own paths".

---

### Task 5: The `octet-hub` crate: store, owner, pairing and devices

**Files:**

- Create: `crates/octet-hub/Cargo.toml` (deps: `octet-core`, `octet-store`, `tokio`,
  `serde_json`, `thiserror`, `getrandom`, `sha2`), `crates/octet-hub/src/lib.rs`,
  `crates/octet-hub/src/store.rs`, `crates/octet-hub/src/owner.rs`
- Modify: the root `Cargo.toml` (workspace member and workspace dependency
  `octet-hub = { path = "crates/octet-hub" }`)
- Test: unit tests in each module

**Interfaces:**

- Produces:

```rust
pub struct Store { .. } // <data dir>/devices.json, hub.json, hub-audit.jsonl
impl Store {
    pub async fn open(directory: &Path) -> std::io::Result<Store>;
    pub fn pairing_secret(&mut self, now: SystemTime) -> String;          // 32 random bytes, hex; valid 10 min, single use
    pub async fn pair(&mut self, secret: &str, name: &str, platform: &str, login: &str, now: SystemTime) -> Result<(String, Device), &'static str>; // returns token
    pub async fn authenticate(&mut self, token: &str, login: &str, now: SystemTime) -> Option<Device>; // updates last_seen
    pub async fn revoke(&mut self, name: &str) -> bool;
    pub fn devices(&self) -> &[Device];
    pub async fn reload_if_changed(&mut self) -> bool;                    // devices.json mtime changed (CLI revoke)
    pub fn config(&self) -> &HubConfig;                                   // port, workspaces allow-list
    pub async fn audit(&self, device: &str, session: &str, what: &str);
}
pub struct Device { pub name: String, pub platform: String, pub login: String, pub created: u64, pub last_seen: u64, pub token_sha256: String }
pub struct HubConfig { pub port: u16 /* 47823 */, pub workspaces: Vec<PathBuf> }
pub async fn owner_login(override_login: Option<String>) -> Result<String, String>; // tailscale status --json
pub async fn tailnet_url() -> Option<String>; // https://<Self.DNSName without trailing dot>
```

- [ ] **Step 1: Failing tests.**

| Test | Expectation |
|---|---|
| `a_pairing_secret_works_once_and_expires` | Pairing with a fresh secret succeeds. The same secret again → `Err("Pairing code already used or expired")`. A secret aged 11 minutes (pass `now`) → the same refusal. |
| `a_token_needs_its_pairing_login` | `authenticate` with the token and the right login → `Some`; with another login → `None`; with a wrong token → `None`. |
| `revoking_removes_a_device_and_the_file_says_so` | After `revoke`, a new `Store::open` sees no device. `devices.json` is mode 0600 and contains no raw token (assert the token string is absent from the file). |
| `audit_lines_never_hold_prompt_text` | `audit("Gemini 10", "s", "prompt")` writes `{time, device, session, what}` only. |
| `owner_login_reads_tailscale_status` | Unit-test the JSON parser (`parse_owner(status_json)`) with a sample containing `Self.UserID` and `User["123"].LoginName`, plus a sample missing them. |

Run: `scripts/rust-env.sh cargo test -p octet-hub` → FAIL.

- [ ] **Step 2: Implement.**
  - **Hex.** Tokens and secrets are `getrandom::fill` of 32 bytes, hex-encoded by a small local
    `hex()` helper. Hashes are hex `sha2::Sha256` digests.
  - **Comparison.** Compare hashes in constant time: fold `a ^ b` over the bytes, after
    checking the lengths match.
  - **Files.** Each file is written to a temp file and renamed, 0600.
  - **Owner.** `owner_login` runs `tailscale status --json` with a 2 s timeout, the same way
    `remote.rs` runs its checks, and parses `Self.UserID` → `User[id].LoginName`. On failure it
    returns "Tailscale isn't running; start it or pass --owner".

Run → PASS.

- [ ] **Step 3: Gate and commit**: `git add Cargo.toml Cargo.lock crates/octet-hub`; message
  "Hub: devices, pairing secrets, tailnet owner and an audit log".

---

### Task 6: Hub sessions: discovery, bridge links and the journal tailer

**Files:**

- Create: `crates/octet-hub/src/sessions.rs`, `crates/octet-hub/src/tail.rs`
- Test: unit tests in both

**Interfaces:**

- Produces:

```rust
/// Reads whole new lines from a growing file.
pub(crate) struct Tail { path: PathBuf, offset: u64, partial: Vec<u8> }
impl Tail { pub(crate) fn new(path: PathBuf) -> Self; pub(crate) async fn read(&mut self) -> std::io::Result<Vec<Value>>; }
/// Every journal record of `journals` after `after` (all of the first journal when None, capped to the last 500 records).
pub(crate) async fn replay(journals: &[PathBuf], after: Option<&Cursor>) -> Vec<(String, u64, Value)>;
pub(crate) struct Sessions { .. }  // registry keyed by Registration.id
impl Sessions {
    pub(crate) fn new(run: PathBuf) -> Self;
    /// One pass: new registrations get a bridge link and a tail; vanished or dead ones are dropped (stale files removed).
    pub(crate) async fn refresh(&mut self);
    pub(crate) fn list(&self) -> Vec<Value>;                  // the `sessions` frame payload
    pub(crate) fn state(&self, id: &str) -> Option<&Value>;   // last state / catalog from the bridge
    pub(crate) async fn send(&mut self, id: &str, req: u64, by: &str, command: &BridgeCommand) -> Result<(), String>;
    pub(crate) async fn next(&mut self) -> SessionUpdate;      // Event{id, journal, seq, record} | State{id} | Catalog{id} | Reply{id, req, ok, message} | Changed
}
```

- [ ] **Step 1: Failing tests.**

| Test | Expectation |
|---|---|
| `the_tailer_waits_for_whole_lines` | Write `{"seq":0…}\n{"seq":1` (no newline) → `read` returns one record. Append `…}\n` → the second. Truncate the file → `read` returns nothing and resets its offset without panicking. |
| `replay_starts_after_the_cursor_and_crosses_journals` | Journals `a` (seq 0..3) and `b` (0..2). After `{a,1}` → a2, a3, b0, b1, b2. After `None` → everything (500 cap tested with a long file). After `{b,2}` → nothing. |
| `a_reconnect_continues_into_the_next_journal` | A registration with one journal; tail some records. Rewrite it with two journals; the next records come from `b` with `journal: "b…"`, and nothing from `a` repeats. |
| `a_registration_whose_socket_refuses_is_dropped_and_removed` | A `.json` pointing at a missing socket is removed and `list()` omits it. |
| `bridge_frames_and_replies_reach_the_hub` | A fake bridge (a `UnixListener` the test owns) sends `state` and `ack` → `next()` yields `State` and `Reply`; `send` writes a parsable `bridge_command_json` line to it. |

Run → FAIL.

- [ ] **Step 2: Implement.**
  - **`Tail::read`.**
    - Open, seek to `offset`, read to the end, and append to `partial`.
    - Split on `\n`; parse each complete line, skipping and counting unparsable ones; keep the
      remainder in `partial`.
    - If the file is shorter than `offset`, reset to 0 and clear `partial`.
  - **`Sessions`.**
    - Poll every 250 ms (driven by the server loop): `registrations(&run)`. For each new id,
      `UnixStream::connect` its socket with a 500 ms timeout; on refusal, remove the `.json` and
      the socket.
    - Per session, keep:
      - the link: a writer `mpsc` and a reader task feeding a shared update channel;
      - a `Tail` on the newest journal, plus the list of journals;
      - the last `state` and `catalog`.
    - Poll the tail every 100 ms.
  - **`sessions` payload per session:** `{id, engine, model, workspace, mode, phase, approvals,
    last_activity, goal, from_phone}`. `phase` and `approvals` come from the last state;
    `last_activity` is the newest journal record time.

Run → PASS.

- [ ] **Step 3: Gate and commit**: message "Hub: finds every interface, links to its bridge, tails its journals".

---

### Task 7: The WebSocket server: handshake, auth, commands, subscribe and replay

**Files:**

- Create: `crates/octet-hub/src/server.rs`
- Modify: `crates/octet-hub/Cargo.toml` (add `tokio-tungstenite`, `default-features = false`,
  `features = ["handshake"]`)
- Test: integration tests in `crates/octet-hub/tests/server.rs`. Add `tokio-tungstenite` with
  `connect` as a dev-dependency. The tests use a fake bridge (a Unix socket plus a written
  registration) and real journal files in a temp data dir.

**Interfaces:**

- Produces:

```rust
pub struct Hub { .. }
impl Hub {
    /// Binds 127.0.0.1:<port> and the local control socket; runs until `shutdown` resolves.
    pub async fn serve(directory: PathBuf, port: u16, owner: String, shutdown: impl Future<Output = ()>) -> Result<(), String>;
}
```

- [ ] **Step 1: Failing tests.**

| Test | Expectation |
|---|---|
| `pair_then_hello_lists_sessions` | Get a secret through the control socket (`{"type":"pair_secret"}` → `{"secret","url"}`). Connect with header `Tailscale-User-Login: owner@example`; `pair` → `paired{token}`. Reconnect; `hello` → `welcome`, then `sessions` with the fake bridge's session. |
| `unauthenticated_and_flooding_clients_are_refused` | A command before `hello` → `refused` and the socket closes. A wrong login header → closed during the handshake. A revoked device's next frame → closed. A 200 KB frame → closed. 100 prompts in a second → `refused("Too many commands; wait a minute")` after 60. Nothing reaches the fake bridge in any of these. |
| `subscribe_replays_then_streams` | Journal with seq 0..4; subscribe after `{journal, 2}` → events 3, 4. Append seq 5 → a live event 5. Reconnect with after `{journal, 5}` → no event until 6 is appended. |
| `commands_reach_the_bridge_and_replies_come_back` | `prompt` → the fake bridge receives `{"type":"prompt","by":"Test phone","text":…}` with the phone's `req` mapped to a hub req. The bridge's `ack` → the phone gets `ack{req: phone req}`. |
| `full_access_needs_the_confirm_here_too` | `set_mode full-access` without `confirm` → `refused` by the hub; the bridge receives nothing. |
| `state_and_catalog_follow_subscriptions` | On subscribe, the phone gets the session's last `state` and `catalog`; updates follow. |
| `audit_records_remote_actions` | After `prompt` and `answer`, `hub-audit.jsonl` has two lines with device and session, and no prompt text. |

Run → FAIL.

- [ ] **Step 2: Implement.**
  - **Listening.** `TcpListener::bind(("127.0.0.1", port))`. Each connection goes through
    `tokio_tungstenite::accept_hdr_async(stream, callback)`, configured with
    `max_message_size` and `max_frame_size` set to `protocol::MAX_MESSAGE`. The callback reads
    `Tailscale-User-Login` into a shared slot and rejects with 403 when it is not the owner.
    (Check the exact callback signature for the version `cargo add` picks, on docs.rs.)
  - **Per connection.** A state machine: `Unauthenticated` (accepts only `hello` or `pair`) →
    `Authenticated(Device)`.
  - **Rate limiting.** A token bucket of 60 commands per minute per device.
  - **Routing.** Bridge commands go to `Sessions::send` with a hub-wide req; a
    `HashMap<hub_req, (connection, phone_req)>` routes the reply back. `recent`, `start` and
    `end` are Task 8 (for now, `refused("Not yet supported")`).
  - **Subscribe.** Run `replay` first, then live events from `Sessions::next`, fanned out to
    each subscribed connection through a bounded `mpsc` (512). A full queue closes that
    connection.
  - **Sessions frame.** Sent on authentication, and whenever `Sessions` reports `Changed`.
  - **Revokes.** `Store::reload_if_changed` every 2 s; connections of removed devices are
    closed.
  - **Control socket.** `<run>/hub.sock` accepts `pair_secret` (returns `{secret, url}`, the URL
    from `tailnet_url()`, or `null` when Tailscale is off) and `watch` (Task 9).
  - **Audit.** `Store::audit` is called for every routed bridge command.

Run → PASS.

- [ ] **Step 3: Gate and commit**: `git add Cargo.toml Cargo.lock crates/octet-hub`; message
  "Hub: an authenticated WebSocket API on 127.0.0.1 with replay, routing, limits and audit".

---

### Task 8: `recent`, `start` and `end`

**Files:**

- Create: `crates/octet-hub/src/launch.rs`
- Modify: `crates/octet-hub/src/server.rs` (route the three commands)
- Test: unit tests in `launch.rs`; one integration test in `tests/server.rs`

**Interfaces:**

- Produces:

```rust
/// The tmux arguments for a phone-started interface; Err when the request is not allowed.
pub(crate) fn start_args(octet: &Path, directory: &Path, request: &StartRequest, allowed: &[PathBuf], running: usize, name: &str) -> Result<Vec<OsString>, String>;
pub(crate) struct StartRequest { pub engine: String, pub workspace: PathBuf, pub resume: Option<String>, pub model: Option<String>, pub mode: Option<String>, pub confirm: Option<String> }
pub(crate) const PHONE_SESSIONS: usize = 8;
pub(crate) async fn launch(args: Vec<OsString>) -> Result<(), String>; // tmux, 5 s timeout, stderr captured into the error
```

- [ ] **Step 1: Failing tests.**

`start_arguments_are_literal_and_workspaces_checked`:

- A workspace `/w/a b; rm -rf ~` that is allowed (the test lists it) → the args contain it as
  one literal `-c` value.
- A workspace not in `allowed` → `Err("Workspace not allowed: …")`.
- `..` is resolved by `canonicalize` before the check, and a missing directory →
  `Err("No such workspace")`.
- An unknown engine → `Err`.
- `mode: full-access` without the confirm → `Err`.
- `running == 8` → `Err("8 sessions started from the phone are running; end one first")`.
- `resume` → `--resume <id>`; the id is one token, and control characters → `Err`.
- The args start with `["new-session","-d","-s",name,"-c",workspace, octet, "--journal-dir",
  directory, "--engine", engine]`.

In `tests/server.rs`, `recent_lists_the_journals_sessions`: write two journals with vendor
session ids for `/w`, and `recent{workspace:"/w"}` returns both, newest first.

Run → FAIL.

- [ ] **Step 2: Implement.**
  - **`allowed`.** `HubConfig.workspaces` together with
    `octet_core::remote::known_workspaces(directory)`, all canonicalised.
  - **The octet path.** `std::env::current_exe()`.
  - **Session name.** `octet-<6 hex>` from `getrandom`.
  - **Marking.** The new interface marks itself as phone-started: the hub passes
    `--remote-started`, a new hidden flag in `args.rs` that sets `Registration.from_phone`.
  - **`launch`.** Runs `tmux` with those args; a missing tmux → `Err("tmux isn't installed")`.
  - **`recent`.** Uses `octet_core::recent_sessions(directory, workspace, 20)`, or, with no
    workspace given, every workspace in `allowed` merged and sorted newest first, capped at 40.
    Each entry is `{engine, workspace, session, title (first prompt, 60 chars), model, started}`.
  - **`end`.** Becomes `BridgeCommand::End`.

Run → PASS. If `tmux` is on the PATH, an extra `#[ignore]`d test `starts_a_real_demo_session`
runs `start` with `engine: demo` and waits for the new registration. Run it locally with
`--ignored`; CI skips it.

- [ ] **Step 3: Gate and commit**: message "Hub: recent sessions, and resuming or starting interfaces in tmux from the phone".

---

### Task 9: The `octet hub` command

**Files:**

- Modify:
  - `crates/octet/Cargo.toml` (depend on `octet-hub`).
  - `crates/octet/src/args.rs`: a `hub` subcommand, parsed before the flags. It accepts
    `--journal-dir`, `--port N` and `--owner LOGIN`, and the actions `serve` (the default),
    `devices`, `revoke NAME` and `watch`.
  - `crates/octet/src/main.rs`: dispatch, and the help text.
- Create: `crates/octet-hub/src/watch.rs`, the text client over `hub.sock`.
- Modify: `crates/octet-hub/src/server.rs` (the control socket's `watch` request).
- Test: `crates/octet/tests/cli.rs` (argument parsing and `hub devices` on an empty store);
  unit tests in `watch.rs` (frame → line formatting).

- [ ] **Step 1: Failing tests.**
  - `octet hub --help` lists the actions.
  - `octet hub devices --journal-dir <tmp>` prints "No paired devices".
  - `octet hub revoke nobody` → exit 1, "No device named nobody".
  - `octet hub --port 0` → usage error.
  - In `watch.rs`, `watch_lines_are_short_and_plain`: a `sessions` frame becomes one line per
    session ("claude · gpt-5.5 · /w · Running Bash cargo test · 1 approval"); a `tool` event
    becomes "… Bash cargo test"; an `approval` becomes "⚠ approval 3: Run a shell command:
    cargo test".
- [ ] **Step 2: Implement.**
  - **`serve`.** Resolves the data directory the same way `run()` does, gets the owner with
    `owner_login`, and calls `Hub::serve` with SIGINT/SIGTERM as the shutdown. On start it
    prints:
    - `Octet hub on 127.0.0.1:<port>`;
    - then either `Publish it on your tailnet: tailscale serve --bg <port>`, or, when
      `tailscale serve status --json` already shows the port, `Published at https://<name>`.
  - **`devices` and `revoke`.** Operate on `Store` directly; a running hub picks up the change
    within 2 s.
  - **`watch`.** Connects to `hub.sock`, sends `watch`, and prints formatted lines until Ctrl+C.
    The control socket streams `sessions` and `event` frames for every session.

Run → PASS.

- [ ] **Step 3: Gate and commit**: message "CLI: octet hub serves, lists and revokes devices, and watches every session".

---

### Task 10: `/remote-control` shows the hub and pairs a phone

**Files:**

- Modify:
  - `crates/octet-tui/src/remote.rs`: hub checks, and the `pair` report.
  - `crates/octet-tui/src/commands.rs`: `/remote-control pair`.
  - `crates/octet-tui/src/jobs.rs`: a pairing job.
  - `crates/octet-tui/src/registry.rs`: usage text.
  - `crates/octet-tui/Cargo.toml`: `qrcode`, `default-features = false`.
  - `crates/octet-hub/src/server.rs`: the control socket's `status` request.
- Test: unit tests in `remote.rs`; key and slash tests in `tests/slash.rs`.

- [ ] **Step 1: Failing tests.**
  - `the_report_names_the_hub_state`. Checks with `hub: None` → a line "Hub: not running ·
    start it with `octet hub` (or `octet hub install` later)". Checks with
    `hub: Some(HubStatus{published: false, port, devices: 0})` → "Hub: running on 127.0.0.1:47823
    · not on the tailnet yet: tailscale serve --bg 47823". Published with 2 devices →
    "Hub: https://mac.ts.net · 2 paired devices".
  - `pairing_shows_a_qr_and_the_link`. `pair_report("https://mac.ts.net", "abcd")` contains
    `https://mac.ts.net/#pair=abcd` as text and a block of QR rows made of `█▀▄ ` characters,
    each the same width.
  - `remote_control_pair_without_a_hub_says_how_to_start_one`. In `tests/slash.rs`, with no
    `hub.sock`, `/remote-control pair` notes "Start the hub first: octet hub".
- [ ] **Step 2: Implement.**
  - **Probe.** `remote::probe` also tries `<run>/hub.sock` with `{"type":"status"}`, which the
    hub answers with `{port, published, url, devices}`. Add that request to Task 7's control
    socket.
  - **Pairing.** `/remote-control pair` starts a job that sends `pair_secret` and renders
    `qrcode::QrCode::new(link)` with the `unicode::Dense1x2` renderer. The note holds the QR, the
    link, "Expires in 10 minutes · works once", and the platform hint "On the phone: open the
    Octet app → Pair → scan". The secret is shown only in that note, never on the status line.

Run → PASS.

- [ ] **Step 3: Gate and commit**: `git add Cargo.lock crates/octet-tui`; message "TUI: /remote-control reports the hub and pairs a phone by QR".

---

### Task 11: End to end, docs and checks

**Files:**

- Create: `crates/octet/tests/hub.rs`, using the PTY harness pattern from
  `crates/octet/tests/terminal.rs` and `common/`.
- Modify: `docs/remote-control.md` (a "Phone app (hub)" section: start the hub, `tailscale
  serve`, pair, revoke, security notes), `docs/tui.md` (`/remote-control pair`, `--no-remote`),
  `CHANGELOG.md`.

- [ ] **Step 1: End-to-end test** `a_phone_drives_a_live_interface_through_the_hub`:
  1. Spawn `octet hub --journal-dir D --port <free port> --owner test@example`.
  2. Spawn a TUI in a PTY: `--journal-dir D --engine codex --binary <protocol-child>`, waiting
     for its registration.
  3. Get a pairing secret via `hub.sock`, then connect a WebSocket with
     `Tailscale-User-Login: test@example`, `pair`, then `hello`.
  4. `sessions` lists the session. `subscribe` with `after: null`.
  5. `prompt "approval"` (fake Codex's approval script) → `event approval` arrives. `answer`
     allow → `approval_closed`, then `finished`.
  6. `set_mode accept-edits` → `ack`, then a `mode` event.
  7. Disconnect and reconnect with the last cursor → no duplicate events.
  8. `end` → the session leaves `sessions`, and the TUI process exits.

  Fail with captured PTY output on any timeout. Mark it `#[cfg(unix)]`.
- [ ] **Step 2: Docs.** Update the files above. Add CHANGELOG bullets:
  - the hub and the bridge;
  - `/remote-control pair`;
  - `octet hub devices|revoke|watch`;
  - `--no-remote`.
- [ ] **Step 3: Live check (real CLIs, tiny prompts, a throwaway `--cwd`).**
  1. Run `octet hub` and `octet hub watch`.
  2. Run one Claude TUI and one Codex TUI (`--model gpt-5.5`) in tmux.
  3. `watch` lists both, and shows tool events and an approval.
  4. A WebSocket script (`websocat`, if installed, or a small test client) answers the approval.
  5. `/remote-control pair` shows a QR code.
  6. With `tailscale serve --bg <port>`, the `https://<mac>.ts.net` handshake succeeds from
     another tailnet device. If no second device is reachable, check it from the Mac against
     the `ts.net` name, and say so in the report.
- [ ] **Step 4: Linux.** Run
  `scripts/rust-env.sh cargo clippy --locked --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`.
  Then run Docker tests with `--init`:
  `docker run --rm --init -v "$PWD":/w -w /w -v octet-linux-target:/target -v octet-linux-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target rust:1.98.1 bash -c 'cargo test --locked --workspace'`.
  Quit Docker with `osascript -e 'quit app "Docker Desktop"'`.
- [ ] **Step 5: Gate and commit**: message "Hub: end-to-end test, docs and changelog".
