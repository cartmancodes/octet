# Octet mobile prototype, Phase E: Dynamic Island — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** on iOS, a Live Activity shows in the Dynamic Island and on the Lock Screen whether sessions are running, waiting for approval or done.
- **While the app runs**, the app updates the activity itself.
- **While it doesn't**, the hub starts, updates and ends it through APNs.
- **The Island's buttons:**
  - **Deny** answers in place.
  - **Allow** opens the app at that approval, behind Face ID and the arm.

**Architecture:**
- **Hub**
  - A new `register_activity` phone command stores each device's ActivityKit tokens.
  - A new `apns.rs` signs an ES256 JWT with the user's `.p8` key and posts to APNs through `curl --http2`, with every secret on stdin.
  - A new activity driver beside the pusher decides when to send start, update or end.
- **App**
  - A local Expo module, `modules/octet-live` (Swift), wraps ActivityKit.
  - A widget extension, `targets/octet-live` (Swift), is added through `@bacons/apple-targets`. It reads display names from a shared App Group.
  - A JS controller drives the activity and handles the Deny intent and the `octet://approve/…` link.

**Tech Stack:** Rust (octet-core protocol, octet-hub; the `p256` crate for ES256), the Expo app, and Swift (ActivityKit and WidgetKit). Swift isn't compiled here: this machine has no iOS SDK.

**Spec:** `docs/superpowers/specs/2026-10-15-mobile-prototype-design.md`: the "Phase E" section, plus "Rules that hold in every phase".

## Global Constraints

- **Gates**
  - Rust tasks: `make rust-check`, Linux clippy, and Linux Docker for octet-hub (`--init`).
  - App tasks: `make mobile-check` and `npm run export:check`.
  - The last task: `cargo build -p octet`, then `make mobile-contract`.
  - Regenerate `protocol.ts` with the octet-core test.
  - Commit only on exit 0, with the gates run unpiped. Never use bare `git stash`.
- **New dependencies**
  - Rust: `p256`, exact version with features `ecdsa`, `pkcs8`, `pem`. Its licences must pass `deny.toml` (MIT or Apache-2.0).
  - Base64url is written in-crate, about 30 lines, tested against RFC 4648 vectors. Don't add a `base64` dependency.
  - npm: `@bacons/apple-targets`, an exact version at least two weeks old, as a dev dependency. No other new packages.
- **`register_activity {token, start_token?}`**
  - Each token is lowercase hex, 1–200 characters. Anything else is refused as `invalid`.
  - It's refused before hello, rate-limited, and audited as `activity registered` with the device name.
  - Tokens are stored per device, redacted in `Debug` and in logs, and dropped on revoke and on an APNs 410.
- **APNs configuration** (in `hub.json`)
  - `apns: {team_id, key_id, key_path, sandbox}`.
  - `team_id` and `key_id` are 10 upper-case alphanumeric characters each.
  - `key_path` is absolute. The file must be mode 0600 and owned by the hub's user, or APNs is off and the hub logs one line: "APNs key must be 0600".
  - With no `apns` block, everything Live Activity is off and nothing is sent.
- **JWT**
  - ES256. Header `{"alg":"ES256","kid":<key_id>}`, claims `{"iss":<team_id>,"iat":<secs>}`.
  - Refreshed when older than 50 minutes.
  - The key and the JWT never appear in argv, logs or `Debug`.
- **Sending**
  - Run `curl -q --http2 -K -`, with the URL, the headers and the body given as a curl config on stdin. argv is literal and fixed.
  - Host: `api.push.apple.com`, or `api.sandbox.push.apple.com` when `sandbox`.
  - Headers:
    - `apns-topic: dev.octet.mobile.push-type.liveactivity`
    - `apns-push-type: liveactivity`
    - `apns-priority`: 10 for start and for approval updates, 5 otherwise.
    - `authorization: bearer <jwt>`
  - A 10 s timeout.
- **The payload, exactly:**

  ```
  {"aps":{"timestamp":<secs>,"event":"start"|"update"|"end",
          "content-state":{"kind":"running"|"approval"|"done","session":<raw hub id or "">,
                           "mac":<host or "">,"since":<secs>,"approvals":<n>},
          "attributes-type":"OctetActivity","attributes":{}}}
  ```

  `start` adds `"alert":{"title":"Octet","body":"Session running"}`, as ActivityKit requires an alert for push-to-start; `end` adds `"dismissal-date"`.
- **Content-free**
  - No names, commands, workspaces or file names in the payload.
  - `session` is the hub's own raw id of the session that leads, and `mac` is the hub's published host, as in Phase D's push data. The hub never knows the phone's pairing ids, so it never sends a key.
  - **The phone owns every key.** It resolves `{mac: host, session: raw}` through `fleet.keyFor(host, raw)`, the one resolver push taps already use (Phase D's final fix wave).
- **Lifecycle (hub)**
  - **Start** with `start_token` when an approval waits or a turn starts and the device has no live connection.
  - **Update** when the kind or the approval count changes, at most once per 2 s per device.
  - **End** 5 minutes after every session is idle, or when the phone connects. From then on the app drives the activity itself.
- **Bounds:** these sends count against the same per-device cap of 6 per minute as the other pushes. A 410 drops the token, and any other failure is logged without the token.
- **App**
  - **The App Group** is `group.dev.octet.mobile`. The app writes `{"<host>/<raw>": {"name", "command"}}`, keyed the way the widget sees a session (host and raw id from the content state) (each value cleaned, at most 64 entries, names ≤ 80 characters and commands ≤ 200) whenever the Inbox's approvals change, and clears it on unpair.
  - **Deny** is a `LiveActivityIntent` that posts an event to the JS module, which resolves the content state's host and raw id with `fleet.keyFor` and sends `answer allow:false` to that Mac. An unresolvable one sends nothing. Deny needs no Face ID, since it widens nothing.
  - **Allow** opens `octet://approve/<host>/<raw session>/<approval>`, with each part validated by Phase D's host and session-id rules and resolved with `fleet.keyFor`. The app shows that approval's sheet with Face ID and the 400 ms arm, as in the Inbox. It never allows from the link alone.
  - **Off by default.** On Android and in jest the module is a no-op with `isSupported() === false`, and every call is safe.
- **Commits:** stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`. Use the trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A `.p8` key** that's world-readable, a symlink, missing, not PKCS#8, or not P-256. APNs stays off, with one log line, and the hub stays healthy. Test: Task 2 `bad_keys_turn_apns_off`.
2. **The JWT and the device tokens never appear** in argv, `Debug`, logs or the audit. Test: Task 2 `secrets_only_on_stdin`, with a stand-in curl that records argv and stdin.
3. **A storm of state changes,** such as 20 approvals in 10 s. At most one update per 2 s per device, and still within 6 per minute. An approval still beats a turn. Test: Task 3 `activity_updates_are_bounded`.
4. **An `octet://approve/…` link** that's forged, stale or names an unknown Mac. It opens the Inbox; nothing is allowed and nothing is sent. Test: Task 5 `approve_link_never_allows_alone`.
5. **APNs answers 410, 400 or 403,** or the connection drops. A 410 drops that token. The others are logged without secrets and keep the token. Nothing retries in a tight loop. Test: Task 2 `apns_status_handling`.

---

### Task 1: `register_activity` (protocol and hub store)

**Files:**
- Modify `crates/octet-core/src/remote/protocol.rs`: add `PhoneCommand::RegisterActivity {token, start_token: Option}` and its schema entry. Regenerate `apps/mobile/src/protocol.ts`.
- Modify `crates/octet-hub/src/store.rs`:
  - `Device.activity: Option<ActivityTokens {token, start_token}>`, with serde defaults so old `devices.json` files load;
  - a redacted `Debug`;
  - dropped on revoke.
- Modify `crates/octet-hub/src/server.rs`: routing, rate limit, the audit line `activity registered`, and the ack.

- [ ] **Step 1 (failing tests):**
  - The parser accepts hex tokens, and refuses upper case, non-hex, 201 characters, extra fields and a missing token.
  - The hub refuses it before hello, persists it, carries no token in `Debug`, drops it on revoke, and audits it with the device name.
  - An old `devices.json` loads.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run the Rust gates and `make mobile-check`. Commit: "Hub: phones register their Live Activity tokens".

### Task 2: The APNs client (`apns.rs`)

**Files:**
- Create `crates/octet-hub/src/apns.rs`:
  - `ApnsConfig` (parsed from `hub.json` in `store.rs` `HubConfig`);
  - `Signer` (loads the key with the 0600 and owner check, and makes and caches the JWT);
  - `send(program, config, token, payload) -> Outcome {Sent, Gone, Failed(status)}`;
  - in-crate `base64url`.
- Modify `crates/octet-hub/Cargo.toml` (`p256`) and `Cargo.lock`.

**Rulings:**
- **The curl config on stdin** holds `url = "https://<host>/3/device/<token>"`, each header as a `header = "…"` line, and `data = "<json>"`, every value escaped for curl's config quoting (`\\` and `\"`). The token has already been checked as hex.
- **The status code** is read with `-w "%{http_code}"` and `-o /dev/null`. Use `-s -S` with `--max-time 10`.
- **Known-answer test.** Use RFC 6979 deterministic signing, which `p256` does, over a fixed test key generated for the test and checked in under `crates/octet-hub/tests/data/apns-test.p8`. Assert:
  - the JWT's header and claims segments byte for byte;
  - that the signature verifies with the matching public key;
  - that the signature equals the checked-in expected value.

- [ ] **Step 1 (failing tests):**
  - base64url vectors;
  - JWT known-answer;
  - the JWT is reused under 50 minutes and remade after;
  - `bad_keys_turn_apns_off`: mode 0644, a symlink, missing, a P-384 key, and garbage;
  - `secrets_only_on_stdin`: the stand-in curl script records argv and stdin, the argv is fixed, and stdin has the URL, the headers and the body;
  - `apns_status_handling`: 200 gives Sent, 410 gives Gone, 400/403/500 give Failed, and a curl failure gives Failed;
  - the sandbox host.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run the Rust gates. Commit: "Hub: an APNs client for Live Activities".

### Task 3: The activity driver (hub)

**Files:**
- Create `crates/octet-hub/src/activity.rs`. `Driver` gets the same session snapshots as the pusher, plus "device connected / disconnected". Per device it decides start, update or end, and builds the payload.
- Modify:
  - `crates/octet-hub/src/server.rs`: wire the snapshots, connection events and the shared per-device cap;
  - `crates/octet-hub/src/push.rs`: expose the cap as a shared limiter if it isn't already.

**Rulings:**
- **The lead session:** the one with the oldest waiting approval, else the most recently started running one, else the last that finished.
- **`kind`:** approval if any approval waits, else running if any session runs, else done.
- **`since`:** when the lead session's current kind began.
- **No live activity known** (none started since the phone last connected): a change needs a start, and a start needs `start_token`. With no start token, nothing is sent.
- **On connect,** the hub sends `end` to any activity it started. From then on the phone drives it.
- **Payload keys** are exactly as in Global Constraints. The test compares JSON.

- [ ] **Step 1 (failing tests):**
  - starts on an approval or a turn while away, and nothing while connected;
  - updates on a kind or count change;
  - `activity_updates_are_bounded`;
  - ends 5 minutes after idle, and on connect;
  - no start token means no start;
  - the payload is content-free (no session name, workspace or command);
  - a 410 drops the token.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run the Rust gates and Linux Docker. Commit: "Hub: drives the Live Activity while the phone is away".

### Task 4: The native module and widget (Swift, app config)

**Files:**
- Create `apps/mobile/modules/octet-live/`:
  - `expo-module.config.json`;
  - `index.ts` (typed API: `isSupported()`, `start(state)`, `update(state)`, `end()`, events `pushToken`, `pushToStartToken` and `deny {key, approval}`);
  - `src/OctetLiveModule.ts`, which uses `requireOptionalNativeModule` and falls back to a no-op;
  - `ios/OctetLiveModule.swift`;
  - `ios/OctetActivityAttributes.swift`, shared with the widget.
- Create `apps/mobile/targets/octet-live/`:
  - `expo-target.config.js` (type `widget`, App Group entitlement);
  - `OctetLiveWidget.swift`: compact, minimal and expanded views; the mascot pose per kind as asset images copied from `src/mascot`; the elapsed time from `since`;
  - `DenyIntent.swift`: a `LiveActivityIntent` that writes the deny request and pings the app through the module's event.
- Modify:
  - `app.json`: the `@bacons/apple-targets` plugin, `ios.entitlements` `com.apple.security.application-groups: ["group.dev.octet.mobile"]` and `NSSupportsLiveActivities: true`;
  - `package.json`: `@bacons/apple-targets` at an exact version.

**Rulings:**
- **Swift can't be compiled here.**
  - Keep the Swift small and plain, using only ActivityKit/WidgetKit APIs from iOS 17.2 and later (`pushToStartTokenUpdates`).
  - Set the deployment target to iOS 17.2 for the target and the module.
  - Name every API used in the report so the device pass can check it.
- **The widget's text** comes from the content state and the App Group JSON only. It shows "Approval waiting", plus the name and the command when known, or "N approvals waiting".
- **The JS wrapper must be safe** in jest, on Android and in Expo Go. Its tests mock nothing native: they exercise the fallback.
- **`npm run export:check`** must still pass with the plugin.

- [ ] **Step 1 (failing tests):**
  - the wrapper's fallback, where every call is a no-op and `isSupported()` is false;
  - the types of the content state match the hub's payload: a test compares the field names with a fixture taken from Task 3's payload test;
  - `app.json` has the plugin, the entitlement and the Info.plist key.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run `make mobile-check` and `npm run export:check`. Commit: "App: the Live Activity module and widget".

### Task 5: The app's activity controller, links, contract and docs

**Files:**
- Create `apps/mobile/src/activity.ts`:
  - It derives `{kind, session, mac, since, approvals}` from the store, the same way the hub's driver does: lead session, kind and since. Here too `session` is the raw hub id and `mac` the Mac's host (from its pairing), so the widget sees one shape whoever sent it.
  - It starts, updates and ends the activity while the app is in the foreground, at most once per second.
  - It registers the tokens with every Mac through `register_activity`.
  - It writes the App Group JSON.
  - It handles `deny` events by resolving `{mac: host, session: raw}` with `fleet.keyFor` and sending `answer allow:false` to that Mac; an unresolvable event sends nothing.
- Modify:
  - `app/_layout.tsx` and `src/navigation.ts`: handle `octet://approve/<host>/<raw session>/<approval>`, resolved with `fleet.keyFor`. A known Mac, a live session and a waiting approval open that approval's sheet, through `useApprovalAnswer`, with Face ID and the arm. Anything else opens the Inbox.
  - `app/settings.tsx`: a "Live Activity" switch, on by default where supported and hidden elsewhere. Turning it off ends the activity and registers no tokens.
- Extend `apps/mobile/contract/hub.contract.test.ts` with `register_activity`. Then:
  - the hub stores the token;
  - with a stand-in curl and a test APNs key in `hub.json`, an approval while the phone is away gives one start, captured with a content-free body.
- Docs: `docs/remote-control.md` (APNs setup: the key, `hub.json`, sandbox, what's sent), `apps/mobile/README.md` and `CHANGELOG.md`.

- [ ] **Step 1 (failing tests):**
  - the derivation matches the hub's rules for the same fixtures;
  - updates are throttled;
  - registration goes to every Mac;
  - the App Group JSON is cleaned and bounded, and cleared on unpair;
  - a deny event sends `allow:false` only to the Mac its host resolves to, and nothing for an unknown host;
  - `approve_link_never_allows_alone`: the link opens the sheet, and nothing is sent until Face ID and the arm;
  - a forged or stale link opens the Inbox;
  - the setting switches it off;
  - the contract steps.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run all the gates, then `cargo build -p octet` and `make mobile-contract`. Commit: "App: the Dynamic Island, driven by the phone and the Mac".
