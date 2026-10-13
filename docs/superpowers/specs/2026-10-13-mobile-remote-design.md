# Octet on your phone: a hub and a mobile app for live sessions — design

**Date:** 2026-10-13. **Branch:** `feat/mobile-remote`.
**Builds on:** `/remote-control` and [Use Octet from your phone](../../remote-control.md) (tmux,
Tailscale SSH, mosh); the earlier evaluation in
[remote-control-ssh.md](../../rust/remote-control-ssh.md); the shelved web-client plan in
[remote-control-plan.md](../../rust/remote-control-plan.md).

## The problem

- Reaching Octet from a phone today means the whole terminal UI over SSH (Termius over Tailscale
  SSH and mosh into tmux). It works, but at phone size it is clunky. The only approval alert is a
  terminal bell, and an approval that waits unseen is denied after the timeout.
- `/remote-control` only checks that SSH access is set up. Nothing listens on the network, so no
  app can connect to a session.
- `octet-core::Session` gives out a single event receiver, which the terminal UI consumes. The
  queue, goals, pending skills and handoffs live in the terminal UI's loop, so a second client
  needs that loop to share the session, not a second consumer.

## Goal

Two phones (an iPhone 15 and an Android "Gemini 10") running one Expo app, over the user's
tailnet only:

- **Watch** every running Octet session: the transcript, tool activity, goal and queue.
- **Answer approvals** from a notification. The push carries no code content.
- **Send prompts**, cancel turns, and steer (Codex).
- **Change mode and model.** `full-access` needs Face ID / fingerprint and a typed confirmation.
- **Resume a past session, or start a new one.** It runs in a new terminal UI in tmux, which the
  desk can join.

Two things stay unchanged:

- The Mac is the only place vendors run, workspaces are read, and logins live.
- The terminal UI at the desk keeps working on the same session, with shared control.

## Non-goals

- Access from outside the tailnet (public relay or tunnel).
- `!shell` commands and slash commands from the phone, other than the ones named below.
- A session that outlives its tmux session or its terminal UI process.
- Storing transcripts on the phone. The Mac's journal is the record.

## Approach

**A: a bridge in each terminal UI, and one hub.** Each terminal UI shares its own session over a
private local socket. A separate `octet hub` process aggregates them, serves the phone over the
tailnet, and sends push. Sessions are still owned by terminal UIs, so `octet-core` and the engines
are unchanged.

Rejected alternatives:

- **B: the hub owns every session and the terminal UI becomes a client.** This is the largest
  refactor (~30–42 days): goals, the queue and handoffs move out of the terminal UI loop. The
  phone ↔ hub protocol below stays compatible with B, so B can come later.
- **C: the hub runs headless `--rpc` sessions for the phone.** The phone could not answer or
  prompt desk sessions, which fails the main use.

## Design

### 1. Components

**Bridge (`octet-tui`, new `bridge.rs`).**

- **Socket.** At start, each terminal UI listens on `<data dir>/run/<pid>.sock` in a
  directory created 0700. Beside it is `<pid>.json` (0600) holding the engine, workspace, model,
  vendor session id, journal path and start time. `--no-remote` turns the bridge off.
- **Outbound.** Every event the terminal UI applies is forwarded as an `event` frame carrying its
  journal `seq`. A `state` frame goes out when terminal UI–only state changes (queue, goal, NOW
  line, mode, pending approvals). A `catalog` frame carries modes and models. Each subscriber
  has a bounded queue, and a subscriber that falls behind is disconnected. The terminal UI is
  never blocked.
- **Inbound.** Commands arrive as an internal `Remote` event in the terminal UI's event loop.
  Each runs through the same function as the matching key or slash command:

  | Command | Runs through |
  |---|---|
  | `prompt` | `send_or_queue` |
  | `answer` | the approval answer, with the arm delay |
  | `cancel` | `cancel_turn` |
  | `steer` | `steer` |
  | `set_mode` | `/mode` |
  | `set_model` | `/model NAME` |
  | `end` | quit (refused while a turn runs) |

  Every remote action is noted in the desk transcript and journaled with the device name, e.g.
  "Allowed from Gemini 10".
- **Cleanup.** The socket and its `.json` are removed on exit. The hub cleans up sockets left by
  processes that no longer exist.

**Hub (new crate `octet-hub`, run as `octet hub`).**

- **Discovery.** Watches the run directory and connects to each bridge.
- **History.** Keeps a short live buffer per session and serves older history from that
  session's JSONL journal.
- **Serving.** One HTTP + WebSocket API on `127.0.0.1:<port>` (default 47823, configurable),
  published only with `tailscale serve`.
- **Push.** Sends content-free notifications (§3).
- **Starting sessions.** Starts terminal UIs in tmux for `start` (§2).
- **Running as a service.** Runs in the foreground. `octet hub install` writes a per-user launchd
  agent (macOS) or systemd user unit (Linux); `octet hub uninstall` removes it.
  `octet hub devices` lists paired devices and `octet hub revoke <name>` removes one.
  `octet hub watch` is a small text client for testing.
- **Config.** `hub.json` (0600) beside `models.json` holds the port and the workspace allow-list.

**`/remote-control` (extended; still never changes system settings).**

- Also reports whether the hub is running, whether `tailscale serve` publishes it, and which
  devices are paired. When something is missing, it prints the exact `tailscale serve` command.
- `/remote-control pair` asks the hub for a pairing secret and shows a QR code (and the link as
  text).

**Mobile app (Expo, TypeScript, in `apps/mobile/`).** See §4.

**Unchanged:** `octet-core`, `octet-engine`, vendor login, and workspace access (only the Mac
reads files).

### 2. Protocol

Both links use JSON messages `{ "v": 1, "type": …, … }`: bridge ↔ hub over the Unix socket,
newline-delimited, and hub ↔ phone over a WebSocket, one message per frame. A message with an
unknown `v` is refused with a `refused` frame naming the supported version.

*Amended 2026-10-13, as built:* a cursor is `{journal, seq}`, because a session can span several
journals (a reconnect or provider switch starts a new one) and `seq` is only unique within one.
`subscribe` takes `after: Cursor`, and `event` frames carry the cursor; `answer` names the
approval as `approval`; and the run directory is `<data dir>/run/`.

**Hub → phone frames:**

| Frame | Contents |
|---|---|
| `sessions` | Live sessions: id, engine, model, workspace, phase, pending-approval count, last activity, goal one-liner, started-from-phone flag. Sent on connect and on any change. |
| `event` | `{session, journal, seq, event: {type, data}}`. `event` is exactly the journal record's type and data: text, `tool {id,name,target,phase,detail}`, `approval {id,detail,summary}`, `approval_closed`, `finished`, notices, errors, usage, mode, model. **The journal format is the wire format.** |
| `state` | Terminal UI–only state: queue (first lines, count), goal (objective, status, turns/max), NOW line, mode, pending approvals with summaries. |
| `catalog` | Modes, and every provider's models (the terminal UI's `model_entries()`), current one marked. |
| `recent` | Recent sessions for `recent {workspace?}`: vendor, workspace, id, title, last activity (the listing `/sessions` uses). |
| `ack` / `refused` | The outcome of each command by its `req` id. `refused` carries the terminal UI's real reason ("A turn is running", "The queue is full (8)", "The approval just opened"). |

**Phone → hub commands.** Each carries a `req` id.

| Command | Effect |
|---|---|
| `subscribe {session, after: Cursor{journal, seq}}` | Replays from the journals after `after` (cursors cross journals; `after` omitted or null replays from the start), then streams live. No gaps, no duplicates. |
| `unsubscribe {session}` | Stops the stream. |
| `prompt {session, text}` | Sends, or queues while a turn runs. |
| `answer {session, approval, allow}` | Allows once, or denies. |
| `cancel {session}` | Cancels the running turn. |
| `steer {session, text}` | Codex only; refused on Claude with the terminal UI's message. |
| `set_mode {session, mode}` | Runs `/mode`. `full-access` needs `confirm: "full access"`, plus a biometric check on the phone. |
| `set_model {session, selection}` | Runs `/model NAME`; a provider switch reconnects and carries the conversation. |
| `recent {workspace?}` | Lists recent sessions. |
| `start {engine, workspace, resume?, model?, mode?}` | Starts a terminal UI in tmux (below). |
| `end {session}` | Quits that terminal UI cleanly; refused while a turn runs. |

**Starting a session.**

- **Command.** The hub runs `tmux new -d -s octet-<short id> -c <workspace> <octet path> --engine
  <engine> [--resume <id>] [--model <m>] [--mode <mode>]`, passing each as a separate argument
  with no shell. The new terminal UI's bridge registers, and the session appears in `sessions`.
  The desk joins with `tmux attach -t octet-<short id>`.
- **Allowed workspaces.** Only those in the recent-sessions list or `hub.json`'s allow-list.
- **Refusals.** Refused when tmux is missing. At most 8 phone-started sessions run at once. A
  `full-access` start needs the same confirmation as `set_mode`.

**Ordering, loss and limits.**

- A cursor, `{journal, seq}`, is the journal file's name and the record's sequence number, shared by live and replayed frames.
- The hub drops a phone that falls behind, and a bridge drops a hub that falls behind. Neither
  blocks the terminal UI. The phone reconnects with its last cursor.
- An approval answered anywhere (desk, the other phone) arrives as `approval_closed`, and every
  screen dismisses it.
- Frames are bounded as the engine bounds events (32 KiB tool detail, 64 KiB prompts). The hub
  caps commands per device per minute. Malformed or unknown commands are refused, never acted on.

**Schema.** Message types are defined once in `octet-hub` (Rust). A test regenerates
`apps/mobile/src/protocol.ts` and fails when the checked-in copy differs.

### 3. Security, pairing and push

**Who can reach the hub.**

- **Network.** The hub binds `127.0.0.1` only. From outside, the only way in is `tailscale
  serve`: HTTPS on the Mac's `*.ts.net` name, reachable only from the tailnet.
- **Tailnet identity.** The hub requires the `Tailscale-User-Login` header (set by `tailscale
  serve`) to be the owner's login.
- **Device token.** Every connection also needs a device token, so other tailnet devices and
  other local processes cannot connect without pairing.
- **Bridges.** Sockets sit in a 0700 directory, so other users on the Mac cannot reach them.

**Pairing (once per phone).**

1. `/remote-control pair` asks the hub for a pairing secret: 32 random bytes, valid for
   **10 minutes, single use**. It is shown as a QR code of `https://<mac>.ts.net/#pair=<secret>`,
   with the same link as text.
2. The app sends the secret with a device name and platform.
3. The hub returns a random 256-bit device token. The phone keeps it in the Keychain / Keystore
   (`expo-secure-store`). The hub stores only a hash, in `devices.json` (0600), with the device
   name, platform, the tailnet login seen at pairing, and created and last-seen times. A later
   connection must present the token **and** come from that tailnet login.
4. `octet hub revoke <name>` deletes the device, closes its connections and drops its push token.

**Phone-side guards.**

- Allow asks for Face ID / fingerprint (`expo-local-authentication`). This is on by default; a
  per-device setting can turn it off. Deny never asks.
- `full-access`, whether by `set_mode` or `start`, always asks for biometrics plus typing
  `full access`. The desk shows an amber note naming the device.
- The answer buttons arm 400 ms after an approval is shown, as on the desk.
- Allow is always "this request only, once". The phone cannot add permission rules.

**Push.**

- The phone registers an Expo push token at pairing and whenever it rotates.
- The hub sends through Expo's push service, which forwards to APNs and FCM. Notifications carry
  only "Octet · approval waiting" or "Octet · turn finished" (the latter only while the app is
  not in the foreground) and an opaque session id. The app then fetches everything from the hub
  over the tailnet.
- Pushes are capped per device per minute.

**Never:** tokens, secrets or push tokens in logs or journals; the hub listening on anything but
`127.0.0.1`; arguments passed through a shell.

**Risks.**

| Risk | Mitigation |
|---|---|
| Stolen phone | Biometrics guard Allow and full access; the device can be revoked. |
| Compromised tailnet device | Still needs a device token. |
| Leaked QR code | Expires after 10 minutes and works once. |
| Hub crash | Terminal UIs carry on; only remote access stops. |
| Wider power from `start` and `set_*` | Workspace allow-list, session cap, full-access confirmation, journaling every remote action with its device. |

### 4. The mobile app

**Stack.**

- Expo (current SDK), TypeScript and Expo Router, with `expo-notifications`,
  `expo-secure-store`, `expo-local-authentication` and `expo-camera` (QR).
- Development builds, not Expo Go: an APK on the Gemini 10, and an Apple developer account
  build on the iPhone. EAS Build is optional.

**Screens.**

1. **Pair.** Scan the QR code or paste the link, name the device, grant notifications.
2. **Sessions.** One card per live session: engine and model, workspace folder name, phase
   ("Running Bash cargo test", "Waiting for approval"), goal line, and an amber approval badge.
   Also has a **Recent and new** entry.
3. **Recent and new.** Recent sessions per workspace, each with **Resume**; a **New session**
   form (engine, workspace from the allowed list, optional model and mode). Both send `start`.
4. **Session.**
   - **Transcript.** Grouped tools as in the terminal UI: "Read 3 files · Ran 1 command" opens on
     tap, failures are open and amber, and replies render as Markdown.
   - **Approval sheet.** Action, `$ command`, files, folder, grant, and "Show full request".
     Allow (biometric) and Deny.
   - **Composer.** The hint says Send when idle or Queue while a turn runs, with the queue count.
   - **Controls.** Cancel; Steer (Codex); a **Mode and model** sheet built from `catalog`, where
     full access needs biometrics and the typed confirmation; End (for sessions with no turn
     running).
   - **Task strip.** Goal, NOW and queue.
5. **Settings.** Hub URL, device name, Face ID to allow, notifications, Unpair.

**Behaviour.**

- **Reconnect.** Subscribes with the last `seq` per session, so returning from the background
  replays exactly what was missed.
- **Offline.** "Can't reach your Mac": the last state is shown greyed out, with no actions until
  reconnected.
- **Refusals.** Shown as the terminal UI words them.
- **Look.** Octet's crimson-on-charcoal palette and mascot, light and dark following the system.

### 5. Testing

- **Bridge (unit).**
  - Each event is forwarded once with its `seq`.
  - A slow subscriber is dropped without stalling the terminal UI.
  - Each remote command takes the same path and gives the same refusals as its key or slash
    command (`TURN_OPEN`, queue full, arm delay, `can_reconnect`).
  - `full-access` is refused without `confirm`.
  - Socket and directory permissions are as specified; stale sockets are removed.
- **Hub (unit and integration, against a fake bridge and a fake push endpoint).**
  - Pairing secrets expire and are single use.
  - A token is accepted only from its pairing's tailnet login; revoking closes its connections.
  - Replay after a cursor from a real journal has no gaps or duplicates.
  - Rate limits hold; malformed commands are refused.
  - Push payloads carry no content.
  - `start` builds the tmux arguments without a shell, refuses workspaces that are not allowed,
    enforces the cap, and passes `--resume`.
- **End to end (fake vendors, macOS and Linux CI).** Terminal UIs on `protocol-child` plus
  `octet hub` on a temporary port, driven by a WebSocket client through: pair, sessions,
  subscribe, prompt, approve, cancel, `set_mode`, `set_model`, `recent`, `start` (new and
  resume), `end`, reconnect with replay, and an approval closed from the desk.
- **Schema.** The generated TypeScript matches the checked-in copy.
- **App (Jest, recorded frames).** Transcript grouping, the approval sheet, the mode and model
  sheet with the full-access guard, and reconnect.
- **Real devices (manual).**
  - Pair both phones.
  - Push on a locked phone; Allow behind biometrics.
  - Switch from Wi-Fi to cellular mid-turn and resume.
  - Answer on one phone and see it close on the other and on the desk.
  - Change model and mode; full access with confirmation.
  - Resume and start sessions, then join from the desk with `tmux attach`.
  - Real Claude and Codex with tiny prompts in a throwaway workspace.
- **Linux.** Clippy for Linux, and Docker tests (with `--init`) for the bridge, the hub and the
  systemd unit.

## Phases

The spec is one contract; it ships as three implementation plans, each with its own review and
merge.

| Phase | Scope | Estimate |
|---|---|---|
| 1. Bridge and hub (Rust) | Bridge; hub; protocol and schema; pairing and devices; `/remote-control pair`; `set_mode`, `set_model`, `recent`, `start`, `end`; `octet hub watch` | 12–17 days |
| 2. Mobile app (Expo) | Pair, Sessions, Recent and new, Session (approvals, composer, mode and model), Settings, on both phones | 8–12 days |
| 3. Push and polish | Content-free push via Expo; background reconnect; `octet hub install`; docs; real-device pass | 3–4 days |

**Total:** about 24–33 days. These are planning estimates; re-estimate after Phase 1.

**The user provides:** an Apple developer account (push and installs on the iPhone), a free Expo
account (push tokens), and optionally a tailnet ACL narrowing access to the two phones.
