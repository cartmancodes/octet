# Adding a provider

Octet drives each vendor CLI through two pieces:

- **A row in the provider table:** `Engine::ALL` in
  `crates/octet-engine/src/live/mod.rs`. The CLI, `/model`, `/mode`, the
  sidebar and the journals all read it.
- **A `Protocol` implementation** in the vendor's own file. The shared driver
  (`live/driver.rs`) owns the loop, the timers, approvals and commands, and
  calls the protocol for everything vendor-specific.

So adding a vendor is one new file and one table row. The driver, the TUI and
`/model` do not change.

## Steps

### 1. Write `crates/octet-engine/src/live/<name>.rs`

Declare it next to the others in `live/mod.rs` (`mod <name>;`). The file
holds three things.

**The row:**

```rust
pub(super) const PROVIDER: Provider = Provider {
    name: "gemini",                 // --engine gemini, journals, /model gemini …
    title: "Gemini",                // "Gemini reports …" in notices
    default_binary: "gemini",       // run when --binary is absent
    offline: false,
    modes: [                        // /mode descriptions, in Mode::ALL order
        "…ask…", "…accept-edits…", "…auto…", "…full-access…",
    ],
    steer: false,                   // true if a running turn takes more input
    mode_applies_next_turn: false,  // true if a live /mode switch reaches only the next turn
    inline_images: false,           // true if images go in the prompt as base64
    effort_live: false,             // true if effort changes in the open session, not at launch
    takes_effort: true,             // false if the vendor has no reasoning effort setting
    ids_carry_name: false,          // true if model IDs can start with the provider's own name (opencode/X)
    efforts: &[],                   // the effort levels it takes; empty passes any word
    // How long the CLI may take to exit once its input closes, before
    // SIGTERM: time how long it really takes and allow some margin.
    shutdown_grace: Duration::from_millis(150),
    lists_models: false,            // true if the CLI can list its models without opening a session
    forks: true,                    // false if the vendor cannot open a session as a copy of another
    carries_others_models: false,   // true if it lists other providers' own models (Antigravity)
    launch_args: <GeminiProtocol as Protocol>::launch_args, // shared with the protocol gate
    launch_env: super::no_env,      // extra env vars for the CLI, on top of the inherited ones
    prepare: super::no_prepare,     // work before the CLI starts, e.g. writing a settings folder
    start,
};

fn start(config: Config, limits: Limits, channels: Channels) -> BoxFuture<Result<(), String>> {
    Box::pin(async move { super::driver::run::<GeminiProtocol>(config, limits, channels).await })
}
```

Write `start` for the concrete type, as shown. That lets the compiler prove
the session can move between threads.

Three fields change how the rest of Octet treats the vendor.
`takes_effort: false` makes `/effort` say the vendor takes no level and change
nothing. A live vendor that sets effort by request (opencode's `effort`
session option) implements `Protocol::set_effort`; without it the driver
records the level for the next turn (Codex). `ids_carry_name: true` is for a vendor whose model
IDs are `provider/model` and can start with the vendor's own name (opencode
has an `opencode` provider): `/model opencode/big-pickle` keeps
`opencode/big-pickle` whole as the model, where other vendors have the leading
`NAME/` stripped as Octet's own prefix. `launch_env` is the environment the CLI
starts with on top of the inherited one (see the table below).

Three more fields cover what a vendor may lack or do beyond the usual.
`forks: false` is for a vendor that cannot open a session as a copy of another:
`/fork` then says "NAME cannot fork a session" and the driver refuses a
launch that asks for a fork.
`carries_others_models: true` is for a vendor whose catalog also lists models
that belong to other providers under the same bare IDs (Antigravity lists some
Claude and GPT models): a bare `/model NAME` that another provider lists too
goes to that provider unless the vendor is the current one, and
`/model PROVIDER NAME` or `/model PROVIDER/NAME` picks the vendor's copy.
`prepare` is work that has to happen before the CLI starts, such as writing a
settings folder the launch arguments name (Antigravity). It returns `Prepared`:
notices to show before the session starts, a `scratch` folder that the driver
removes once the CLI has stopped, and a `workspace` string that the driver
hands to the protocol through `Protocol::prepared` before `initialize`. An
`Err` is the reason the session cannot start. The driver and the protocol gate
both call it, as they call `launch_args` and `launch_env`; `super::no_prepare`
is for a vendor with nothing to do.

**The protocol and its state.** Keep only this vendor's state here; the
shared state is in `Core`.

```rust
#[derive(Default)]
pub(super) struct GeminiProtocol { /* this vendor's state only */ }

impl Protocol for GeminiProtocol {
    fn launch_args(config: &Config) -> Vec<OsString> { … }
    async fn initialize(&mut self, core: &mut Core) -> Result<(), DriverError> { … }
    async fn send_prompt(&mut self, core: &mut Core, text: &str, images: &[ImageAttachment])
        -> Result<(), DriverError> { … }
    async fn compact(&mut self, core: &mut Core) -> Result<(), DriverError> { … }
    async fn interrupt(&mut self, core: &mut Core) -> Result<(), DriverError> { … }
    async fn set_mode(&mut self, core: &mut Core, target: Mode) -> Result<(), DriverError> { … }
    async fn on_frame(&mut self, core: &mut Core, frame: Value) -> Result<(), DriverError> { … }
    fn answer(wire: &Value, allow: bool) -> Value { … }
    // Optional: deny, is_progress, mode_change_pending, mode_unsettled, take_switch_timed_out,
    // holds_turn_for_switch, deadline, on_deadline, turn_started, stopping, set_effort, steer,
    // prepared.
}
```

**The catalog parser.** If the vendor reports models, write a parser for its
format, built on `model_catalog_with`.

**ACP vendors.** opencode and Antigravity both speak the Agent Client
Protocol, and what they share is in `live/acp.rs`: `Requests` (Octet's own
requests awaiting a reply, numbered on the wire), `Switches` (live mode
switches awaiting a reply, and the timed-out ones the vendor may still
confirm), `refuse_request` and the stray replies, `Levels` (a model's thinking
levels), `tool_phase`, `end_prompt`, the permission dialog's contents and the
inline image blocks. A vendor that speaks ACP builds on these rather than on
its own copies.

### 2. Add the row to the table

```rust
pub const ALL: &'static [Engine] = &[Engine::CODEX, Engine::CLAUDE, Engine::OPENCODE,
    Engine::ANTIGRAVITY, Engine::DEMO, Engine(&gemini::PROVIDER)];
```

`ALL` lives in the same module as `Engine`, so it can build the row
directly. Add a named constant (`Engine::GEMINI`) only if code needs to
name the vendor. Code should almost never need to: it asks `engine.offline()`, or it
reads the row.

### 3. Add a fake-vendor script and live tests

Teach `crates/octet-testkit/src/bin/protocol-child.rs` to speak enough of the
protocol to run scripted scenarios. Then add tests to
`crates/octet-engine/tests/live.rs` for:

- connect;
- a turn;
- an approval allowed, denied and timed out;
- a cancel;
- a mode switch;
- a vendor error.

The existing Codex and Claude tests are the pattern.

### 4. Optionally, add gate scenarios

`crates/octet-gate` records contract evidence against the real CLI. Its
scenarios are per vendor.

## What each `Protocol` method must do

| Method | Contract |
| --- | --- |
| `launch_args` | The CLI's arguments for `config`: model, resume ID, the permission mode at launch. |
| `initialize` | Send the handshake. When its reply arrives (in `on_frame`): set `core.phase = Phase::Idle`, emit `Event::Ready` with the session ID, and pass the mode the vendor reports to `core.adopt_mode`, which notes a mismatch and emits `Event::ModeChanged`. A two-step handshake passes through `Phase::Handshaken`. **Only move the phase forward**: ignore a repeated reply, and treat a step that arrives early as a protocol error, as Codex and Claude do. |
| `launch_args` (more) | Also: `config.effort` when the vendor takes effort at launch, and `config.fork` (open `resume` as a new session). |
| `launch_env` (the row's function, not a method) | The environment variables the CLI starts with, on top of the inherited ones. `super::no_env` for a vendor that needs none; opencode passes its agents and permission rules this way (`OPENCODE_CONFIG_CONTENT`). The driver and the protocol gate both call it, so the gate checks what ships. |
| `prepare` (the row's function, not a method) | Work before the CLI starts, once per launch (a catalog probe too, with `config.catalog_only` set). Returns `Prepared { notices, scratch, workspace }`: the notices are shown first, the `scratch` folder is removed when the CLI has stopped (or failed to start), and `workspace` is handed to `Protocol::prepared`. `Err` is why the session cannot start. `super::no_prepare` for a vendor with nothing to do. Antigravity writes its settings folder and checks its workspace configuration here. |
| `prepared` | Receives `Prepared::workspace` once, before `initialize`. The default ignores it. |
| `send_prompt` | Send the user's text, then its images (`ImageAttachment`: path, media type, name; `read_base64` reads the bytes). The driver has already set `Phase::InTurn` and emitted `User` and `Started`. If an image cannot be read, fail the turn with `core.finish_turn(Outcome::Failed, Some(message))`. |
| `compact` | Ask the vendor to compact its context, as a turn: the driver has set `Phase::InTurn` and emitted `User("/compact")` and `Started`. |
| `steer` | Add text to the running turn; `Ok(false)` (the default) means the vendor cannot, and the interface queues the text instead. Set the row's `steer` to match. |
| `interrupt` | Ask the vendor to stop the turn. The driver has set `Phase::Interrupting`, and denies pending approvals after this returns. |
| `set_mode` | Switch a live session to `target`. The driver has refused full access, an unready session, a pending switch and a no-op. Emit `Event::ModeChanged` once the vendor confirms. |
| `on_frame` | Turn vendor output into `Text`, `Tool`, `Usage`, `Approval` (through `core.queue_approval`, which denies a request too large to show or beyond the 8 already waiting) and, at the turn's end, `core.finish_turn(outcome, error)`, which closes the turn's approvals and makes the session idle. Ignore output for other sessions or turns. |
| `answer` | The reply that allows or denies an approval request. The driver uses it for user answers, timeouts and oversized requests. |
| `deny` | The reply denying an approval with a reason (the user's refusal, a timeout, the cap, a cancelled turn). Override it if the vendor passes a message on to the model, as Claude does; the default is `answer(wire, false)`. |
| `mode_change_pending` | True while a mode switch waits for the vendor to confirm it. The driver then refuses another switch. Where `mode_applies_next_turn` is set, it also holds a prompt sent while idle until the switch is confirmed or refused, so the prompt runs under the mode the header shows; a prompt held behind a switch that `take_switch_timed_out` reports unconfirmed fails unsent. |
| `holds_turn_for_switch` | True if the vendor applies a mode switch to the running turn but answers requests in no fixed order, so a prompt sent right after a switch could run before it applies. The driver then holds a prompt sent while a switch is unconfirmed, as it does for a provider with `mode_applies_next_turn`, and a prompt behind a switch that timed out is not sent (see `mode_unsettled` and `take_switch_timed_out`). Antigravity returns true; the default is false. |
| `is_progress` | Whether a frame shows the turn is alive. Return false for another thread's output. |
| `deadline` / `on_deadline` | An optional protocol timer, such as Claude's mode confirmation. |
| `mode_unsettled` | The target of a timed-out switch the vendor has not answered yet, while it is unanswered: the vendor's mode is unknown, so a provider whose mode applies from the next turn has every turn refused unsent. |
| `take_switch_timed_out` | Whether the last switch was reported unconfirmed since it was sent (reading clears it), so the driver does not send a prompt held behind it. |
| `turn_started` | Clear per-turn state. |
| `stopping` | The session is ending, however it ends, before the vendor process is stopped: clean up after it (opencode deletes a probe's session here when the probe ended early). |

Requests Octet will not show the user (a permission outside a turn, a
method it does not know) get a reply from a plain function in the vendor's
file, as Claude's and Codex's `stray` do: deny permissions, and report
anything else as unsupported. Export it as `<vendor>_stray_reply` when the
protocol gate needs it.

## Rules that apply

- **Never answer a vendor approval yourself.** Queue it for the user, or deny
  it when it is too large to show.
- **Keep every buffer bounded,** and fail visibly on overload (`emit` returns
  `ConsumerOverloaded` when the interface falls behind).
- **Never read vendor credentials.** The CLI authenticates itself.
- **Follow `.claude/skills/rust-engineer/SKILL.md`,** and pass `make rust-check`.
