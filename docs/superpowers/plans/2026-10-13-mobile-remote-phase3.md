# Octet on your phone, Phase 3: push and polish — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** finish the phone remote:

- content-free push ("approval waiting", "turn finished") from `octet hub` through Expo;
- a phone-side keepalive and an immediate reconnect on returning to the foreground;
- `octet hub install` / `uninstall` as a per-user launchd agent or systemd user unit;
- the contract test in CI;
- docs and a device-pass checklist the user runs on the iPhone 15 and the Gemini 10.

**Architecture:**

- **Push in the hub.** A new `crates/octet-hub/src/push.rs` watches the session list the hub
  already keeps: an approval id it hasn't seen before, or a turn ending.
  - It sends to devices that have a push token and **no open, authenticated connection**. The app
    closes its socket in the background, so a live connection means the user is looking at it.
  - Sending runs `curl` with literal argv, and the JSON body goes on stdin, so the token never
    appears in `ps` or in logs.
- **The app** registers its Expo push token with a new `register_push` command after each
  `ready`, and whenever the token rotates. A tap on a notification opens that session.
- **Liveness** is a new `ping` command that the hub acks.
- **The service.** The service files are written by `crates/octet/src/service.rs`. A hidden
  environment seam replaces `launchctl`/`systemctl` in tests, so tests never touch the real user
  session.

**Tech stack:**

- **Rust:** the existing workspace, tokio, and `serde_json::Value` by hand.
- **App:** Expo SDK 57, adding `expo-notifications` and `expo-splash-screen`, both through
  `npx expo install`.

**Spec:** `docs/superpowers/specs/2026-10-13-mobile-remote-design.md` (§1 running as a service,
§3 push, §4 notifications, §5 testing, Phases row 3).

## Global constraints

- **Gates.** Everything from Phase 1 and Phase 2 still applies:
  - `make rust-check`, with cargo via `scripts/rust-env.sh`. Two known flaky tests may need one
    re-run: `remote_control_reports_a_ready_host_with_the_phone_command` and
    `a_silent_cli_is_gone_after_the_probe_gives_up`.
  - `make mobile-check`, `npm run export:check`, and `make mobile-contract` (after
    `cargo build -p octet`).
  - Commit only inside `if <gate>; then git commit …; fi`. A task touching both sides runs both
    gates.
- **Protocol.**
  - Message types are defined once in `crates/octet-core/src/remote/protocol.rs`.
  - Regenerate with `OCTET_WRITE_PROTOCOL=1 scripts/rust-env.sh cargo test -p octet-core
    the_checked_in`. Never hand-edit `apps/mobile/src/protocol.ts`.
- **Push content.** The title is exactly `Octet`, and the body is exactly `Approval waiting` or
  `Turn finished`. `data` is `{session}` with the hub's opaque session id. Nothing else: no
  command, file, workspace, model or text.
- **Never:**
  - push tokens, device tokens or secrets in logs, journals, audit lines, `Debug` output or argv;
  - a shell (every process gets literal argv);
  - the hub listening on anything but `127.0.0.1`.
- **Push caps.** At most 6 pushes per device per rolling minute. Push only once per approval id per
  session, and only once per finished turn.
- **No outward changes on this Mac:**
  - Don't run `octet hub install` for real, and don't run `launchctl` or `systemctl` against the
    user's session. Tests use the seam.
  - Don't send to the real Expo endpoint. Tests use a local fake.
  - Don't run `tailscale serve` or change Tailscale, Claude or Codex configuration.
  - Don't create accounts or run EAS.
  - Don't quit Docker Desktop. Docker runs use `--init`.
- **Commits.** Stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`.
  Messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **App rules.** Everything in `.superpowers/sdd/2026-10-13-mobile-remote-phase2/app-context.md`
  applies, carried into this plan's workspace as `app-context.md`: TDD with a captured RED run, no
  snapshots, memoised rows, theme tokens, accessibility, and hub strings rendered as Text only.

## Review focus

1. **A push storm.**
   - **Input:** many approvals opening and closing fast, a reconnecting session replaying old
     approvals, or ten sessions finishing at once.
   - **Expected:** no more than 6 per device per minute; no push for an approval already
     answered; no repeat push for a replayed approval.
   - **Test:** Task 2 `push_is_capped_and_never_repeats`.
2. **A phone in the background with a half-open socket.**
   - **Expected:** the hub still counts it as connected until pings fail. The app's foreground
     return reconnects at once, and a missed `ping` ack reconnects within 35 s.
   - **Test:** Task 4 `a_missed_ping_reconnects` and `foreground_reconnects_now`.
3. **Push token hygiene.**
   - Covers revoking a device, a phone turning notifications off, and Expo answering
     `DeviceNotRegistered`.
   - **Expected:** the token is dropped and never used again, and it never appears in
     logs, audit lines or `Debug` output.
   - **Test:** Task 1 `push_tokens_are_kept_private` and Task 2 `a_dead_token_is_dropped`.
4. **The service on a Mac with Homebrew tmux.**
   - **Input:** launchd starts with a bare PATH, where tmux is missing.
   - **Expected:** the agent's PATH includes the directory where `tmux` was found at install
     time, plus `/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin`.
   - **Test:** Task 3 `the_launchd_agent_finds_tmux`.
5. **A notification tapped on a phone that has since been unpaired, or that names an unknown
   session.**
   - **Expected:** Pair or the Sessions screen opens; never a crash or a stuck screen.
   - **Test:** Task 5 `a_tap_opens_the_session_or_falls_back`.

---

### Task 1: Protocol: `register_push` and `ping`; push tokens on devices (Rust)

**Files:**

- Modify: `crates/octet-core/src/remote/protocol.rs` (`PhoneCommand::RegisterPush {token:
  Option<String>}` and `PhoneCommand::Ping`, the schema table and the parser), the regenerated
  `apps/mobile/src/protocol.ts`, `crates/octet-hub/src/store.rs` (`Device.push_token`), and
  `crates/octet-hub/src/server.rs` (handling both commands).

- [ ] **Step 1: Failing tests:**
  - **The parser:**
    - `register_push {req, token}` accepts a token matching
      `^Expo(nent)?PushToken\[[A-Za-z0-9_-]{1,200}\]$`, or `null` (turn off).
    - Anything else is refused with `invalid`.
    - `ping {req}` parses.
  - **`push_tokens_are_kept_private`:**
    - `Device`'s `Debug` and `RegisterPush`'s `Debug` redact the token.
    - `devices.json` stores it (0600) as `push_token`, and reading it back round-trips.
    - `revoke` drops it.
    - An audit line for `register_push` says only "push on" or "push off".
  - **The hub:**
    - `register_push` from an authenticated phone sets or clears that device's token and acks
      with "Notifications on" or "Notifications off".
    - Before `hello` it is refused with `unpaired`.
    - `ping` acks with `"pong"` and counts against the rate limit like any other command.
- [ ] **Step 2.** Implement, regenerate `protocol.ts`, and make sure the app still typechecks
  (`npm run typecheck`).
- [ ] **Step 3.** Run both gates and commit: "Hub: register a phone's push token, and answer pings".

### Task 2: Push from the hub (Rust)

**Files:**

- Create: `crates/octet-hub/src/push.rs`.
- Modify: `crates/octet-hub/src/server.rs`, wiring session-list changes and connection presence
  into the pusher; `crates/octet-hub/src/lib.rs`.
- Test: unit tests in `push.rs`; `crates/octet-hub/tests/server.rs` with a fake push endpoint.

**Interfaces:**

- `Pusher::observe(&mut self, rows: &[SessionRow-like])` → the pushes due. It is pure, so it is
  easy to test.
- `Pusher::due_for(device, connected: bool, now)` applies the cap.
- `send(endpoint, batch) -> Vec<Outcome>` runs curl.

**Rulings:**

- **Triggers:**
  - an approval id first seen in a session's state (`approvals[].id`, the source the `sessions`
    rows already use) → `Approval waiting`;
  - a phase change from `running` to anything that isn't running (idle/stopped) → `Turn finished`.
  - The first observation of a session after hub start or a session's appearance seeds what it
    has seen and pushes nothing. That prevents a storm on restart.
- **Recipients:** devices with `push_token` and no authenticated connection open right now.
- **Coalescing:** within one 2 s window, pushes to one device collapse to one per kind. An
  approval beats a finished turn, and the data carries the most recent session.
- **Sending:**
  - `curl -sS --max-time 10 -H 'Content-Type: application/json' -H 'Accept: application/json'
    --data-binary @- <endpoint>`, with the JSON array of messages on stdin.
  - The endpoint is `https://exp.host/--/api/v2/push/send`. A hidden test seam,
    `OCTET_HUB_PUSH_URL`, is honoured only when it starts with `http://127.0.0.1:`.
  - A missing `curl`, or a failure, is logged as "Push failed" with no token, and dropped (no
    retry queue).
- **Message:**

  ```json
  {"to": "<token>", "title": "Octet", "body": "Approval waiting" | "Turn finished",
   "data": {"session": "<id>"}, "sound": "default" (approval only),
   "priority": "high" | "normal", "channelId": "approvals" | "turns"}
  ```

- **Receipts:**
  - An `{"status":"error","details":{"error":"DeviceNotRegistered"}}` ticket clears that device's
    token through the locked read-modify-write the store already uses.
  - Other errors are logged as "Push refused: <error code>".

- [ ] **Step 1: Failing tests:**
  - **The pure pusher:**
    - seeding sends nothing;
    - a new approval id pushes once;
    - the same id seen again, or after close and replay, does not push;
    - a reused id after the session's journal list changes pushes again (a new vendor process);
    - running → idle pushes `Turn finished` once.
  - **`push_is_capped_and_never_repeats`:** 20 approvals across 10 sessions in one minute → at
    most 6 pushes per device, with coalescing honoured.
  - **Connected devices:** a connected device gets nothing, and the same device disconnected
    gets the push.
  - **Integration (`tests/server.rs`):**
    - a fake bridge opens an approval;
    - the hub posts to a local HTTP fake;
    - the body equals exactly the message above, and contains no workspace, command, file or text
      from the approval;
    - `a_dead_token_is_dropped`: the fake answers `DeviceNotRegistered`, then `devices.json` has
      no token and the next approval sends nothing.
  - **No secrets in argv:** the curl argv contains no token (asserted via a stand-in `curl`
    first on PATH that logs its argv and stdin separately).
- [ ] **Step 2.** Implement. Keep `server.rs` changes to the wiring; the logic lives in `push.rs`.
- [ ] **Step 3.** Run `make rust-check`, and a Linux Docker run of `octet-hub` tests (`--init`,
  rust:1.98.1). Commit: "Hub: content-free push through Expo".

### Task 3: `octet hub install` and `uninstall` (Rust)

**Files:**

- Create: `crates/octet/src/service.rs`.
- Modify: `crates/octet/src/args.rs` (`HubAction::Install{print: bool}`, `Uninstall`),
  `crates/octet/src/hub.rs`, and `docs/remote-control.md`.

**Rulings:**

- **macOS:** `~/Library/LaunchAgents/dev.octet.hub.plist`.
  - The program is the canonical `current_exe()`, with `hub` plus the user's `--journal-dir` if
    one was given.
  - `RunAtLoad` and `KeepAlive` are true.
  - `StandardOutPath` and `StandardErrorPath` point to `<data dir>/hub.log`.
  - `EnvironmentVariables.PATH` is the directory of `which tmux` at install time, if found, then
    `/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin` with duplicates removed.
  - Loading: `launchctl bootout gui/<uid> <plist>` (errors ignored), then
    `launchctl bootstrap gui/<uid> <plist>`.
- **Linux:** `~/.config/systemd/user/octet-hub.service`.
  - `ExecStart` is the same argv, systemd-quoted.
  - `Restart=on-failure`, `Environment=PATH=…` as above, and `WantedBy=default.target`.
  - Loading: `systemctl --user daemon-reload`, then `systemctl --user enable --now
    octet-hub.service`.
- **Uninstall:** stop or boot out, remove the file, and on Linux run `daemon-reload` again. It is
  idempotent: "Not installed" when there is nothing to remove.
- **`--print`:** writes the unit to stdout and changes nothing.
- **Test seam:** the hidden `OCTET_SERVICE_CTL=<path>` replaces `launchctl`/`systemctl`, receiving
  the same argv. `HOME` points at a temp dir. Tests never call the real tools.
- **Files:** 0644. The directory is created if missing. An existing file is replaced only if it
  is ours (it contains `dev.octet.hub` / `octet-hub`); otherwise refuse.

- [ ] **Step 1: Failing tests** (args, unit text, and the seam's recorded argv):
  - `the_launchd_agent_finds_tmux`: a stand-in tmux in a temp dir is first on PATH, and its
    directory leads the agent's PATH.
  - the systemd unit text, with argv quoting for spaces;
  - install then uninstall leaves no file;
  - a second uninstall says "Not installed";
  - `--print` writes nothing;
  - a foreign file at the path is refused.
- [ ] **Step 2.** Implement, and document it in `docs/remote-control.md` ("Keep the hub running"):
  logs and how to remove it.
- [ ] **Step 3.** Run `make rust-check` and the Linux Docker run. In a throwaway container, run
  `systemd-analyze verify` on the generated unit if `systemd` can be apt-installed there; record
  the output. Commit: "octet hub install: keep the hub running as a user service".

### Task 4: App: keepalive, foreground reconnect, and Phase 2's parked fixes

**Files:**

- Modify: `apps/mobile/src/client/HubClient.ts`, `src/hub.ts` (AppState), `src/sessions.ts`
  (pose), `src/model/session.ts`, and `src/components/ModeModelSheet.test.tsx`.

**Rulings:**

- **Keepalive:**
  - while `ready` and in the foreground, send `ping` every 25 s;
  - no ack within 10 s → treat the link as dead (the same path as a request timeout:
    tear down and reconnect with backoff);
  - pings stop with `stop()` and in the background.
- **Foreground:** AppState `active` → `client.retry()` if a reconnect timer is pending, so the
  backoff wait is skipped; otherwise `start()` as now.
- **Parked fixes:**
  - **Mascot:** the mascot keeps the error pose when a session stops after an error. In
    `poseOf`, `turn === "failed"` wins over `phase === "stopped"`.
  - **Test flush:** `ModeModelSheet.test.tsx`'s `flush` drains a macrotask inside `act`, so
    no FlashList act warning appears in 20 consecutive runs.
  - **Reused approval ids:** the store forgets its `closed` ids when a session's journal list
    changes (a new vendor process reuses ids), so an id reused in a state frame shows.

- [ ] **Step 1: Failing tests:**
  - `a_missed_ping_reconnects` (fake timers and a fake socket);
  - `foreground_reconnects_now`;
  - no ping in the background;
  - the error-then-stopped pose;
  - a state-only reused id after a new journal shows;
  - the flush fix, checked by running the file 20 times with `--silent=false` and zero act
    warnings.
- [ ] **Step 2.** Implement. Run `make mobile-check` and commit: "App: keepalive pings and an immediate reconnect on return".

### Task 5: App: notifications and the splash

**Files:**

- Create: `apps/mobile/src/notifications.ts`.
- Modify: `app/_layout.tsx` (the tap handler and the notification handler), `app/pair.tsx`
  (a "Turn on notifications" step after pairing), `app/settings.tsx` (the Notifications switch),
  `src/persist.ts` (`notifications: boolean`, default false), `src/hub.ts` (register after
  ready), `app.json`, `package.json`, and `jest.setup.ts` (mocks).

**Rulings:**

- **Installation:**
  - Install `expo-notifications` and `expo-splash-screen` with `npx expo install`.
  - `app.json` plugins:
    - `expo-notifications`, with no custom icon for now;
    - `expo-splash-screen`, with `backgroundColor` `#faf6f5` and `dark.backgroundColor`
      `#161113`, and the existing icon or no image.
- **The project id:**
  - read from `Constants.expoConfig?.extra?.eas?.projectId ?? Constants.easConfig?.projectId`;
  - if it is missing, the Settings switch is disabled with "Notifications need this app's EAS
    project id. See the README.", and the pairing step is skipped;
  - never invent an id.
- **Turning on (pairing step or switch):**
  1. request permission;
  2. denied → "Notifications are off for Octet in the phone's settings.", and the switch stays
     off;
  3. granted → `getExpoPushTokenAsync({projectId})`, save `notifications: true`, and send
     `register_push {token}`.
- **Turning off:** send `register_push {token: null}` and save false. Never ask for biometrics.
- **Registration:**
  - after every `ready` with notifications on, register the current token once;
  - `addPushTokenListener` re-registers on rotation;
  - a refusal of `register_push` shows on Settings only, never as a modal.
- **Android channels:** `approvals` (HIGH importance, sound) and `turns` (DEFAULT), created at
  start-up on Android.
- **Foreground handler:** show nothing in the foreground; the screen is already live.
- **Tap, `a_tap_opens_the_session_or_falls_back`:** `data.session` is a string of at most 128
  characters matching the hub's id charset.
  - If paired: `router.push("/session/[id]")`. The Session screen already handles a gone session.
  - If unpaired: go to Pair.
  - Bad data → Sessions.
  - Handle both a cold start (`getLastNotificationResponseAsync`) and a warm one (the listener).
- **Unpair:** also send `register_push {token: null}` best-effort before releasing. Revoking on
  the Mac drops it there anyway.

- [ ] **Step 1: Failing tests:**
  - the switch flows: granted, denied, no project id;
  - register after ready, and rotation;
  - off sends null;
  - the tap routing (paired, unpaired, bad data, cold start);
  - no banner in the foreground;
  - the Android channels;
  - unpair unregisters.
- [ ] **Step 2.** Implement. Run `make mobile-check` and `npm run export:check`. Commit: "App: notifications that open the session, and a themed splash".

### Task 6: CI, the contract test for push and ping, docs, and the device pass

**Files:**

- Modify:
  - `.github/workflows/check.yml`: a `contract` job on `ubuntu-latest` that builds `octet` debug
    with the pinned toolchain the `check` job uses, runs `npm ci`, then `npm run contract`. Pin
    actions by SHA as the other jobs do.
  - `apps/mobile/contract/hub.contract.test.ts`: add `ping`, `register_push` (on, then off), and
    a push to the local fake when an approval opens while the phone is disconnected. Check the
    body is exactly the content-free message.
  - `docs/remote-control.md`: Push, Keep the hub running, and the outbound request to exp.host.
    Explain that the hub sends only the title and body plus an opaque id.
  - `apps/mobile/README.md`:
    - setting up push: `eas init` adds the project id; APNs key and FCM credentials through
      `eas credentials`; the user's own accounts;
    - a **Device pass** checklist copied from spec §5's real-device list, plus a locked-phone
      push and a tap that opens the session;
  - `CHANGELOG.md`.
- [ ] **Step 1.** Extend the contract test first, and see it fail on a missing step.
- [ ] **Step 2.** Add the CI job and the docs.
- [ ] **Step 3.** Run every gate: `make rust-check`, `make mobile-check`, `npm run export:check`
  and `make mobile-contract`. Commit: "CI runs the phone contract test; push and service docs; the device pass".
