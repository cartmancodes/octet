# Relay Connectivity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A phone pairs with Octet in three steps (install the app, run `octet pair`, scan) with no
Tailscale anywhere in the phone link.

**Architecture:** The hub stops listening on TCP and dials out to a small relay
(`crates/octet-relay`, new); the phone connects to the same relay; the relay splices the two
sockets. Phone and hub run `Noise_IK_25519_ChaChaPoly_SHA256` end to end, so the relay forwards
bytes it cannot read. The existing JSON frames, cursors, replay and push run unchanged inside
the encrypted channel.

**Tech Stack:** Rust (tokio, tokio-tungstenite, `snow`, `ed25519-dalek`, rustls), TypeScript in
the Expo app (`@noble/curves`, `@noble/ciphers`, `@noble/hashes`, `expo-crypto`).

**Spec:** `docs/superpowers/specs/2026-11-19-relay-connectivity-design.md`. Read it before any
task; where this plan and the spec disagree, the spec wins.

## Global Constraints

- Run cargo through `scripts/rust-env.sh`. Gates: `make rust-check`, `make mobile-check`
  (Node 22), and `make mobile-contract` once Task 10 lands.
- Source lines at most 120 columns; every item has a doc comment; warnings are errors; no bare
  `#[allow]` (use `#[expect(lint, reason = "…")]`); no `unwrap` outside tests.
- Tests use `octet_testkit::TempDir`. Stage files by name. Never commit
  `tests/fixtures/synthetic/*.synth_marker`.
- Linux parity: `scripts/rust-env.sh cargo clippy --locked --workspace --all-targets --target
  x86_64-unknown-linux-gnu -- -D warnings` before the branch is called done.
- Dependencies are pinned to exact versions the way the workspace does (`getrandom = "0.4.3"`).
  `cargo-deny` is not installed locally: read each new crate's licence from
  `cargo metadata --format-version 1`.
- Licences: ISC, BSD-3-Clause and CDLA-Permissive-2.0 may be added to `deny.toml`, each as an
  explicit entry with a comment naming the crate, scoped by `exceptions` where one crate needs
  it. Any other new licence stops the work for the owner.
- No Tailscale in the phone link: no code path may run `tailscale`, read a `Tailscale-*`
  header, or accept only `.ts.net` hosts when the branch is done.
- No listening network port on the Mac. The hub's only sockets are outbound and `hub.sock`.
- iPhone and Android are both targets; "keychain" means `expo-secure-store` on either.
- Exact values from the spec, used verbatim:
  - Noise pattern `Noise_IK_25519_ChaChaPoly_SHA256`; handshake messages exactly 96 and 48
    bytes, empty payloads.
  - Chunk: one flag byte (`0` more, `1` last) then at most 65,000 bytes; a frame over
    `MAX_MESSAGE` (96 KiB) is refused.
  - Relay paths `/v1/hub/<channel>`, `/v1/phone/<channel>`, `/v1/hub/<channel>/<connection>`,
    `/pair`; close code 4404 when no hub is registered.
  - Registration signature is Ed25519 over `"octet-relay-v1"` followed by the 32 challenge bytes.
  - Channel id: lowercase hex SHA-256 of the hub's Ed25519 public key (64 characters).
  - Pairing link `https://<relay host>/pair#<base64url, no padding>` of version byte `1`, the
    32-byte hub identity public key, the 32-byte channel id, the 32-byte secret (97 bytes).
  - Pair code: first four bytes of SHA-256(`"octet-pair-code-v1"` ‖ handshake hash), big-endian,
    modulo 1,000,000, zero-padded to six digits, shown as `482 913`.
  - Pairing secret: single use, 10 minutes. Confirmation: 2 minutes. One pairing waits at a time.
  - Hub limits unchanged: 64 connections, 32 unauthenticated, first frame within 10 seconds,
    60 commands a minute per device. Relay: 16 phones per channel, ping every 20 seconds, two
    missed pongs, 10 seconds for an unspliced phone or a missing hub data connection.
  - Reconnect schedule: 0.5 seconds doubling to 30, up to 50% jitter.

## Review Focus

Each line names the task whose tests pin it.

1. **The relay restarts while phones are connected.** The hub registers again on its backoff and
   each phone reconnects and resumes from its cursor, with no pairing lost. (Task 6)
2. **Two hubs hold the same `hub.key`** (a copied data directory). Each registration replaces the
   other. A replaced hub must back off and log `Another hub took this channel`, never spin.
   (Tasks 4 and 6)
3. **`hub.key` is unreadable or corrupt.** The hub refuses to start and says which file; it never
   makes a new key silently, which would unpair every phone. (Task 5)
4. **The owner leaves the confirmation prompt open.** After 2 minutes the phone is refused, the
   prompt ends with `Pairing timed out`, and a later `octet pair` works. (Tasks 7 and 8)
5. **The relay is unreachable from the phone's network.** The app shows `Can't reach your Mac`
   and stays paired; only the hub's `unpaired` or `revoked` forgets a pairing. (Task 10)

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/octet-relay/` (new crate) | The relay: registration, splice, limits, `/pair` page, health |
| `crates/octet-hub/src/secure.rs` (new) | Noise responder, chunking, pair code, over any stream |
| `crates/octet-hub/src/keys.rs` (new) | `hub.key`: identity and relay keys, channel id, signing |
| `crates/octet-hub/src/relay.rs` (new) | Outbound control link, data connections, reconnect |
| `crates/octet-hub/src/link.rs` (new) | The pairing link's encoding |
| `crates/octet-hub/src/server.rs` | Loses the listener, gate and publication; gains arrivals and pending pairings |
| `crates/octet-hub/src/store.rs` | Devices keyed by phone public key; `relay` in `hub.json` |
| `crates/octet-hub/src/owner.rs` | Deleted |
| `crates/octet-core/src/remote/protocol.rs` | `hello` without token; `paired` and `welcome` carry `name` |
| `crates/octet/src/pair.rs` (new), `args.rs`, `hub.rs`, `main.rs` | `octet pair`; `--port`, `--owner` removed |
| `crates/octet-tui/src/remote.rs` | `/remote-control pair` with the prompt; Tailscale checks removed |
| `apps/mobile/src/client/noise.ts` (new) | Noise initiator, chunking, pair code |
| `apps/mobile/src/client/secureSocket.ts` (new) | The encrypted socket `HubClient` sees as text |
| `apps/mobile/src/client/link.ts`, `HubClient.ts`, `fleet/*`, `macs.ts`, `persist.ts`, `notifications.ts`, `activityDeny.ts` | New link, keys in place of tokens, Macs keyed by channel id |
| `docs/remote-control.md`, `docs/relay.md` (new), `apps/mobile/README.md` | Setup, hosting, security notes |

---

### Task 1: Dependencies and licences

**Files:**
- Modify: `crates/octet-hub/Cargo.toml`, `Cargo.lock`, `deny.toml`
- Create: `docs/rust/relay-dependencies.md`

**Interfaces:**
- Produces: `snow`, `ed25519-dalek` and a rustls-backed `tokio-tungstenite` client with compiled-in
  webpki roots available to `octet-hub`; `connect` available outside dev-dependencies.

- [ ] **Step 1:** Add the crates with `scripts/rust-env.sh cargo add -p octet-hub`, default
  features off, enabling only what Tasks 2, 5 and 6 need (snow's default resolver, Ed25519
  signing and verifying, the tungstenite `connect` and rustls features). Pin exact versions.
- [ ] **Step 2:** List every crate new to `Cargo.lock` with its licence
  (`cargo metadata --format-version 1`) in `docs/rust/relay-dependencies.md`: crate, version,
  licence, why it is needed.
- [ ] **Step 3:** Add each licence not already in `deny.toml` as the Global Constraints allow. If
  any crate carries a licence outside that list, stop and report it; do not look for a
  workaround crate without saying so.
- [ ] **Step 4:** Run `scripts/rust-env.sh cargo build --locked -p octet-hub` and the Linux
  clippy command. Expected: both succeed with no warnings.
- [ ] **Step 5:** Commit: `Hub: crates for the relay link, with their licences reviewed`.

### Task 2: The hub's encrypted channel

**Files:**
- Create: `crates/octet-hub/src/secure.rs`, `crates/octet-hub/tests/data/noise-vectors.json`
- Modify: `crates/octet-hub/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub(crate) struct Peer { pub key: [u8; 32], pub hash: [u8; 32] }` (the phone's static key
    and the handshake hash).
  - `pub(crate) async fn respond<S>(ws: WebSocketStream<S>, identity: &[u8; 32]) -> Option<(Secure<S>, Peer)>`
    for `S: AsyncRead + AsyncWrite + Unpin`, within `HANDSHAKE_WITHIN` (5 seconds).
  - `impl<S> Secure<S>`: `async fn next(&mut self) -> Incoming`, `async fn send(&mut self, frame: &str) -> bool`,
    `async fn ping(&mut self) -> bool`, `async fn close(&mut self)`.
  - `pub(crate) enum Incoming { Text(String), Pong, Skip, Closed }`.
  - `pub(crate) fn pair_code(hash: &[u8; 32]) -> String` (six digits, no space).
  - `noise-vectors.json`: `{ "pattern", "hub_static_private", "phone_static_private",
    "hub_ephemeral_private", "phone_ephemeral_private", "message1", "message2", "hash",
    "transport": [{ "from": "phone"|"hub", "plain", "cipher" }], "pair_code" }`, all hex.

- [ ] **Step 1: Failing tests** in `secure.rs`:
  - `the_vectors_file_matches_snow`: regenerates the vectors with fixed keys and asserts equality
    with the committed file (`UPDATE_VECTORS=1` rewrites it).
  - `a_first_message_of_any_other_size_is_refused` (95 and 97 bytes; a text message; a 96-byte
    message of random bytes).
  - `a_frame_over_65000_bytes_goes_as_two_chunks_and_comes_back_whole` (a 96 KiB frame).
  - `a_frame_past_max_message_is_refused_and_closes`, `a_tampered_message_closes`,
    `chunks_never_end_closes` (flag `0` forever), `random_messages_never_panic` (seeded loop of
    2,000 random binary messages after a good handshake).
  - `pair_code_is_six_digits_from_the_first_four_bytes`: a hash whose SHA-256 input gives a known
    value, asserting the exact string including a leading zero case.
- [ ] **Step 2:** Run `scripts/rust-env.sh cargo test --locked -p octet-hub secure`. Expected: FAIL.
- [ ] **Step 3:** Implement. One WebSocket binary message is one Noise message. Non-binary
  messages other than ping, pong and close end the connection. Decryption failure ends it.
- [ ] **Step 4:** Run the tests. Expected: PASS, and `noise-vectors.json` committed.
- [ ] **Step 5:** Commit: `Hub: Noise IK responder with chunked frames and the pair code`.

### Task 3: Noise in the app, and the speed measurement

**Files:**
- Create: `apps/mobile/src/client/noise.ts`, `apps/mobile/src/client/noise.test.ts`,
  `apps/mobile/src/dev/cryptoBench.ts`, `apps/mobile/src/dev/cryptoBench.test.ts`
- Modify: `apps/mobile/package.json`, `package-lock.json`, the app's developer screen

**Interfaces:**
- Consumes: `crates/octet-hub/tests/data/noise-vectors.json` (read by relative path in the test).
- Produces:
  - `export interface KeyPair { publicKey: Uint8Array; privateKey: Uint8Array }`
  - `export function newKeyPair(random: (n: number) => Uint8Array): KeyPair`
  - `export function initiate(local: KeyPair, hubKey: Uint8Array, random: (n: number) => Uint8Array): { message1: Uint8Array; finish(message2: Uint8Array): Channel | null }`
  - `export interface Channel { hash: Uint8Array; seal(frame: string): Uint8Array[]; open(message: Uint8Array): { frame: string } | "more" | null }`
    (`"more"` means a chunk was kept and the frame is not whole yet; `null` is a failure that
    must close the socket).
  - `export function pairCode(hash: Uint8Array): string` and `export const showCode = (code: string): string` (`"482 913"`).
  - `export function runCryptoBench(): { handshakeMs: number; megabytesPerSecond: number }`.

- [ ] **Step 1: Failing tests** in `noise.test.ts`: `matches_the_shared_vectors` (message1,
  hash, every transport message and the pair code, with the vector's fixed ephemeral key),
  `refuses_a_second_message_of_the_wrong_size`, `a_96_KiB_frame_is_two_messages_and_opens_whole`,
  `a_tampered_message_opens_as_null`, `a_frame_past_MAX_MESSAGE_opens_as_null`.
- [ ] **Step 2:** Run `cd apps/mobile && npm test -- noise`. Expected: FAIL.
- [ ] **Step 3:** Add `@noble/curves`, `@noble/ciphers`, `@noble/hashes` and `expo-crypto` at exact
  versions, and implement the one pattern by hand over them. No other Noise pattern, no options.
- [ ] **Step 4:** Add the benchmark (100 handshakes; sealing and opening 8 MiB in 64 KiB frames)
  and a row on the developer screen that runs it and shows both numbers. Its test checks only
  that it returns positive numbers.
- [ ] **Step 5:** Run `make mobile-check`. Expected: PASS.
- [ ] **Step 6:** Commit: `App: Noise IK over noble primitives, checked against the hub's vectors`.

**Owner checkpoint (not a task step).** The owner runs the benchmark on the iPhone and the
Android phone. Pass: handshake at most 1,000 ms and at least 1 MiB per second, on both. Tasks 4
to 9 do not depend on the result and go ahead. Task 10 starts only once the owner reports a
pass or waives it; on a fail, stop and report, because the fallback (a native crypto module on
both platforms) is a change to this plan.

### Task 4: The relay

**Files:**
- Create: `crates/octet-relay/Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/limits.rs`,
  `src/pair.html`, `tests/relay.rs`; at the repo root `Dockerfile.relay`; `docs/relay.md`
- Modify: `Cargo.toml` (workspace member and dependency)

**Interfaces:**
- Produces:
  - `pub struct Config { pub limits: Limits, pub client_address_header: Option<String> }` with
    `Default`; `pub struct Limits { pub message_bytes: usize, pub phones_per_channel: usize,
    pub connects_per_minute: u32, pub registrations_per_minute: u32, pub bytes_per_second: u64,
    pub burst_bytes: u64, pub unspliced: Duration, pub ping_every: Duration }`, defaults
    `66_560`, `16`, `30`, `10`, `262_144`, `4_194_304`, 10 seconds, 20 seconds.
  - `pub async fn serve(listener: TcpListener, config: Config, shutdown: impl Future<Output = ()>) -> io::Result<()>`.
  - Control messages (JSON text): relay → hub `{"challenge": <64 hex>}`, hub → relay
    `{"key": <64 hex>, "signature": <128 hex>}`, relay → hub `{"registered": true}` then
    `{"connect": <32 hex connection id>}` per phone.
  - Close codes: `4401` bad registration, `4404` no hub, `4408` not spliced in time, `4409`
    replaced by a newer registration, `4429` a limit.
  - `GET /healthz` → `200 ok`; `GET /pair` → the static page; binary `octet-relay` reading
    `PORT` (default 8080) and `OCTET_RELAY_CLIENT_ADDRESS_HEADER`.

- [ ] **Step 1: Failing tests** in `tests/relay.rs` (each starts `serve` on `127.0.0.1:0`):
  `a_hub_with_a_good_signature_registers`, `a_bad_signature_closes_4401`,
  `a_key_that_does_not_hash_to_the_channel_closes_4401`,
  `a_phone_with_no_hub_closes_4404`, `a_phone_and_hub_are_spliced_both_ways` (binary bytes equal),
  `closing_one_side_closes_the_other`, `a_second_registration_replaces_the_first_with_4409`,
  `a_17th_phone_closes_4429`, `a_message_over_the_cap_closes`, `a_text_message_on_data_closes`,
  `an_unspliced_phone_closes_4408` (paused time), `a_silent_connection_is_dropped_after_two_pings`,
  `the_byte_rate_limit_delays_but_delivers_a_burst`, `connects_per_address_are_limited`
  (with the header set and unset), `the_pair_page_and_healthz_answer`,
  `logs_carry_no_channel_id_or_address` (capture the log sink).
- [ ] **Step 2:** Run `scripts/rust-env.sh cargo test --locked -p octet-relay`. Expected: FAIL.
- [ ] **Step 3:** Implement. Channel table in memory; nothing buffered for an absent side;
  paths outside the table answer 404; a WebSocket upgrade is required on the three `/v1` paths.
- [ ] **Step 4:** `Dockerfile.relay` (multi-stage, final image runs the one binary as a non-root
  user) and `docs/relay.md`: what the relay sees, the single-instance rule, the client address
  header, `docker run`, pointing a hub at it (`"relay"` in `hub.json`), and the deploy checklist
  the owner follows. Build the image if Docker is running; say so if it was not built.
- [ ] **Step 5:** Run the tests and `make rust-check`. Expected: PASS.
- [ ] **Step 6:** Commit: `Relay: registration, splice and limits`.

### Task 5: Hub keys, and devices keyed by phone key

**Files:**
- Create: `crates/octet-hub/src/keys.rs`, `crates/octet-hub/src/link.rs`
- Modify: `crates/octet-hub/src/store.rs`, `lib.rs`, and every use of `token_sha256` and `login`
  in `server.rs`, `push.rs`, `activity.rs`, `crates/octet/src/hub.rs`

**Interfaces:**
- Produces:
  - `pub(crate) struct HubKeys` with `pub(crate) async fn load_or_create(directory: &Path) -> io::Result<HubKeys>`,
    `fn identity_private(&self) -> &[u8; 32]`, `fn identity_public(&self) -> [u8; 32]`,
    `fn relay_public(&self) -> [u8; 32]`, `fn channel(&self) -> &str`,
    `fn sign(&self, challenge: &[u8; 32]) -> [u8; 64]`. File `hub.key`, mode 0600, JSON
    `{"v":1,"identity":<hex>,"relay":<hex>}`.
  - `Device.key: String` (64 lowercase hex, the phone's public key) replaces `token_sha256`;
    `Device.relay: String` (the relay address it paired through) is new; `Device.login` is gone.
  - `Store::begin_pair(&mut self, secret: &str, name: &str, platform: &str, key: &str, relay: &str, now: SystemTime) -> Result<Pairing, &'static str>`;
    `Store::recognise(&mut self, key: &str, now: SystemTime) -> Option<(Device, Option<u64>)>`.
    `Pairing` no longer holds a token. `Store::pair` and `Store::authenticate` follow.
  - `HubConfig.relay: String` replaces `port`; `pub const DEFAULT_RELAY: &str = "wss://relay.octet.invalid"`
    (a placeholder until the owner names the host). `"relay"` in `hub.json` must start `wss://`;
    debug builds also honour `OCTET_HUB_RELAY_URL` when it starts `ws://127.0.0.1:`.
  - `pub(crate) fn pair_link(relay: &str, identity: &[u8; 32], channel: &str, secret: &str) -> Option<String>`.

- [ ] **Step 1: Failing tests**:
  - `keys.rs`: `a_new_key_file_is_private_and_stable_across_loads`,
    `a_corrupt_key_file_is_an_error_naming_the_file_and_is_left_alone` (Review Focus 3),
    `the_channel_is_the_hash_of_the_relay_public_key`, `a_signature_verifies`.
  - `store.rs`: `a_device_is_recognised_by_its_key`, `an_unknown_key_is_not`,
    `records_from_before_keys_are_dropped_at_load` (a `devices.json` with `token_sha256`),
    `a_relay_that_is_not_wss_is_ignored_for_the_default`, existing pairing tests moved to keys.
  - `link.rs`: `the_link_decodes_to_its_97_bytes`, `a_bad_relay_address_makes_no_link`.
- [ ] **Step 2:** Run `scripts/rust-env.sh cargo test --locked -p octet-hub`. Expected: FAIL.
- [ ] **Step 3:** Implement. The rename is mechanical: wherever a device was identified by
  `token_sha256` it is identified by `key`. No behaviour change in push or the Live Activity.
- [ ] **Step 4:** Run `make rust-check`. Expected: PASS. The hub still listens on TCP after this
  task, and Task 6 replaces that path. Until then `server.rs` keeps the old `hello` and `pair`
  working by passing the SHA-256 of the device token as `key` and an empty `relay`, so for this
  one commit a device's `key` holds a token hash. Say so in a comment that Task 6 deletes.
- [ ] **Step 5:** Commit: `Hub: its own keys, and devices known by their public key`.

### Task 6: The hub connects through the relay

**Files:**
- Create: `crates/octet-hub/src/relay.rs`
- Modify: `crates/octet-hub/src/server.rs`, `src/server/*`, `lib.rs`, `Cargo.toml`
  (dev-dependency on `octet-relay`), `crates/octet-core/src/remote/protocol.rs`,
  `apps/mobile/src/protocol.ts` (generated), `crates/octet-hub/src/push.rs`, `activity.rs`,
  `crates/octet-hub/tests/*.rs`, `crates/octet/src/{hub,args,main}.rs`
- Delete: `crates/octet-hub/src/owner.rs`

**Interfaces:**
- Consumes: Task 2 `respond`, `Secure`, `Peer`; Task 4 `octet_relay::serve`; Task 5 `HubKeys`,
  `Store::recognise`, `HubConfig.relay`.
- Produces:
  - `pub(crate) enum RelayState { Off, Connecting, Registered }`
  - `pub(crate) type Stream = MaybeTlsStream<TcpStream>` (tungstenite's client stream)
  - `pub(crate) struct Arrival { pub channel: Secure<Stream>, pub peer: Peer, pub seat: Seat }`
  - `pub(crate) async fn run(url: String, keys: Arc<HubKeys>, wanted: watch::Receiver<bool>, room: Arc<Room>, arrivals: mpsc::Sender<Arrival>, state: watch::Sender<RelayState>)`
  - `Hub::serve(directory: PathBuf, shutdown: impl Future<Output = ()>) -> Result<(), String>`
    and `serve_notify(…, ready: oneshot::Sender<()>)`; `port`, `owner` and `Publisher` are gone.
  - Protocol: `hello` carries no `token` (a `token` field is ignored); `welcome(device, name)`
    and `paired(device, name)` where `name` is the Mac's host name, at most 64 characters.
  - Control socket `status` answers `{"relay": "off"|"connecting"|"registered", "devices": <n>}`.
  - Push and Live Activity `mac` is `keys.channel()`, always present.

- [ ] **Step 1: Failing tests** (hub tests now start an in-process relay and connect a test phone
  that runs the initiator side with `snow`):
  `a_known_key_says_hello_and_is_welcomed_with_the_macs_name`,
  `an_unknown_key_that_says_hello_is_refused_unpaired_and_closed`,
  `a_revoked_key_is_dropped_within_two_seconds`, `nothing_is_dialled_with_no_device_and_no_pairing`,
  `the_hub_registers_again_after_the_relay_restarts_and_a_phone_resumes_from_its_cursor`
  (Review Focus 1), `a_replaced_hub_backs_off_and_logs_another_hub_took_this_channel`
  (Review Focus 2; assert at most 3 registrations in a paused 10 seconds),
  `a_data_connection_that_fails_the_handshake_frees_its_seat`,
  `a_push_names_the_channel_as_mac`, `status_reports_the_relay_state`.
  Every existing server, push and activity test is moved onto this harness and kept.
- [ ] **Step 2:** Run `scripts/rust-env.sh cargo test --locked -p octet-hub`. Expected: FAIL.
- [ ] **Step 3:** Implement. `pump` and `write` work on `Secure` (`Incoming::Text` where
  `Message::Text` was). The room's seat is taken when the relay says `connect`, before the data
  connection opens. `wanted` is true while a device is paired or a pairing secret is unexpired.
  Remove the listener, `Gate`, `foreign_origin`, `Publication`, `watch_published`, `--port`,
  `--owner` and `owner.rs`.
- [ ] **Step 4:** Regenerate `apps/mobile/src/protocol.ts` the way `protocol.rs` does, and fix
  the app's compile errors only as far as `npm run typecheck` (behaviour lands in Task 10).
- [ ] **Step 5:** Run `make rust-check` and the Linux clippy command. Expected: PASS.
- [ ] **Step 6:** Commit: `Hub: phones arrive through the relay, end-to-end encrypted`.

### Task 7: Pairing with confirmation, in the hub

**Files:**
- Modify: `crates/octet-hub/src/server.rs`, `store.rs`

**Interfaces:**
- Consumes: Task 2 `pair_code`; Task 5 `pair_link`, `begin_pair`.
- Produces, on `hub.sock` (one request line each):
  - `{"type":"pair_secret"}` → `{"link": <string>}` once the relay has registered, waiting at
    most 10 seconds; else `{"link": null, "reason": "Can't reach the relay at <host>"}`.
  - `{"type":"pair_wait"}` → answers when a phone sends `pair`, or after the secret's 10 minutes:
    `{"pending": <u64>, "code": "482913", "name": <string>, "platform": <string>}` or
    `{"expired": true}`. Not bound by `CONTROL_WITHIN`; ends if the client leaves.
  - `{"type":"pair_answer","pending":<u64>,"accept":<bool>}` → `{"paired": true, "device": <name>}`,
    `{"paired": false}`, `{"error": "Pairing timed out"}`, or `{"error": "The phone left"}` when
    its connection closed while the question was open.
  - To the phone: `paired` on accept; on refusal or timeout `refused` with code `unpaired` and
    reason `Pairing was refused on the Mac` or `Pairing timed out`, then close. A second `pair`
    while one waits: `refused`, code `busy`, reason `Another phone is pairing`.

- [ ] **Step 1: Failing tests**: `an_accepted_pairing_stores_the_key_and_answers_paired`,
  `a_refused_pairing_stores_nothing_and_spends_the_secret`,
  `an_unanswered_pairing_times_out_after_two_minutes` (Review Focus 4, paused time),
  `a_second_pair_while_one_waits_is_refused_busy`,
  `the_code_matches_what_the_phone_computes` (the test phone's own handshake hash),
  `a_different_phone_with_the_same_secret_shows_a_different_code`,
  `a_phone_that_leaves_while_the_question_is_open_makes_the_answer_fail_with_the_phone_left`,
  `pair_secret_with_the_relay_down_gives_the_reason`,
  `an_unknown_key_whose_first_frame_is_not_pair_is_closed`.
- [ ] **Step 2:** Run the hub tests. Expected: FAIL.
- [ ] **Step 3:** Implement. The secret is spent when `pair` arrives, whatever the answer.
- [ ] **Step 4:** Run `make rust-check`. Expected: PASS.
- [ ] **Step 5:** Commit: `Hub: a pairing waits for a yes at the Mac, by a code both sides show`.

### Task 8: `octet pair`

**Files:**
- Create: `crates/octet/src/pair.rs`
- Modify: `crates/octet/src/args.rs`, `main.rs`, `hub.rs`, `crates/octet-tui/src/remote.rs`
  (export the QR rows), `crates/octet/tests/*`

**Interfaces:**
- Consumes: Task 7's three control requests; `hub::install`.
- Produces: `Parsed::Pair`; `pub(crate) async fn pair(directory: PathBuf, input: impl AsyncBufRead + Unpin, output: impl Write) -> Result<i32, CliError>`;
  `octet_tui::qr_rows(link: &str) -> Option<Vec<String>>` made public.
- Copy, exact: `Phone shows 482 913? Pair "<name>" (<platform>)? [y/N]`, `Paired "<name>"`,
  `Not paired`, `Pairing timed out`, `Can't reach the relay at <host>`,
  `tmux isn't installed: the phone can watch and answer, but can't start sessions`,
  `<n> phones are paired through another relay and must pair again`.

- [ ] **Step 1: Failing tests**: `pair_parses`, `port_and_owner_are_no_longer_flags`,
  `pair_starts_the_hub_when_it_is_not_running`, `yes_pairs_and_no_does_not` (`y`, `Y`, `yes` pair;
  empty, `n` and end of input do not), `the_prompt_times_out_with_pairing_timed_out`
  (Review Focus 4), `a_missing_tmux_is_said_once`, `a_narrow_terminal_gets_the_link_and_no_qr`,
  `devices_on_another_relay_are_counted`.
- [ ] **Step 2:** Run `scripts/rust-env.sh cargo test --locked -p octet pair`. Expected: FAIL.
- [ ] **Step 3:** Implement: install and start the service if `hub.sock` does not answer, wait
  up to 15 seconds for it, then `pair_secret`, print the QR and link, `pair_wait`, prompt,
  `pair_answer`. Exit 0 on a pairing, 1 otherwise.
- [ ] **Step 4:** Run `make rust-check`. Expected: PASS.
- [ ] **Step 5:** Commit: `octet pair: one command from nothing to a paired phone`.

### Task 9: `/remote-control` in the terminal UI

**Files:**
- Modify: `crates/octet-tui/src/remote.rs`, its command wiring and tests,
  `crates/octet-tui/src/tests/slash.rs`

**Interfaces:**
- Consumes: Task 7's control requests.
- Produces: `/remote-control pair` shows the QR and link, then the confirmation as a prompt in
  the UI with the same copy as Task 8; plain `/remote-control` prints three lines: hub running or
  not, relay state, phones paired.

- [ ] **Step 1: Failing tests**: `pair_shows_the_qr_then_asks_with_the_code`,
  `answering_yes_pairs_and_no_does_not`, `the_prompt_ends_when_the_pairing_times_out`,
  `plain_remote_control_reports_hub_relay_and_phones`,
  `pair_with_no_hub_starts_it` (as Task 8), `no_tailscale_ssh_or_mosh_command_is_run`
  (the fake command runner records nothing but the hub's).
- [ ] **Step 2:** Run `scripts/rust-env.sh cargo test --locked -p octet-tui remote`. Expected: FAIL.
- [ ] **Step 3:** Implement, and delete the tailnet, SSH and mosh checks (`Tailnet`, `Checks`
  and what only they use).
- [ ] **Step 4:** Run `make rust-check`. Expected: PASS.
- [ ] **Step 5:** Commit: `/remote-control: pairing with a prompt, and no Tailscale checks`.

### Task 10: The app connects through the relay

Starts only after the owner checkpoint under Task 3.

**Files:**
- Create: `apps/mobile/src/client/secureSocket.ts` and its test
- Modify: `apps/mobile/src/client/{link,socket,HubClient}.ts`, `fleet/{fleet,keychain,macLink}.ts`,
  `macs.ts`, `persist.ts`, the pairing screen, `contract/{hub.contract.test,nodeSocket}.ts`, tests

**Interfaces:**
- Consumes: Task 3 `initiate`, `Channel`, `pairCode`, `showCode`, `newKeyPair`.
- Produces:
  - `parsePairLink(text): { relay: string; hubKey: string; channel: string; secret: string } | null`
    (`relay` as `wss://<authority>/`, the rest lowercase hex); `NOT_TS_NET` is deleted.
  - `export interface Pairing { id: string; relay: string; channel: string; hubKey: string; deviceName: string; macName: string }`
    (replaces `hubUrl`).
  - `secureSocketFactory(keys: { hubKey: Uint8Array; device: KeyPair }, raw: RawSocketFactory): SocketFactory`,
    whose socket exposes `hash: Uint8Array | null` once open. Its URL is
    `<relay>v1/phone/<channel>`.
  - `HubClientOptions`: `key: KeyPair | null` replaces `token`; `hubKey` and `channel` are added.
    `pair(secret, name, platform, onCode: (code: string) => void): Promise<{ device: string; name: string }>`.
  - Keychain item per pairing holds the device private key in hex, under the existing item names.

- [ ] **Step 1: Failing tests**: link parsing (a good link, a wrong version byte, 96 and 98
  bytes, `http`, a path other than `/pair`); `the_socket_opens_only_after_the_handshake`;
  `a_bad_second_message_closes`; `frames_cross_whole_in_both_directions`;
  `hello_carries_no_token`; `pair_reports_the_code_before_paired`;
  `a_4404_close_is_offline_and_keeps_the_pairing` and `a_refused_unpaired_forgets_it`
  (Review Focus 5); `pairing_a_channel_already_held_replaces_it`;
  `a_fifth_channel_is_refused`; `pairings_and_tokens_from_before_are_dropped_at_first_launch`;
  `the_private_key_is_this_device_only`. The contract test pairs and subscribes against the real
  hub and an in-process relay binary.
- [ ] **Step 2:** Run `cd apps/mobile && npm test`. Expected: FAIL.
- [ ] **Step 3:** Implement. The pairing screen shows `Check this code on your Mac` with the
  code from `onCode` while it waits.
- [ ] **Step 4:** Run `make mobile-check` and, with `cargo build -p octet -p octet-relay` done,
  `make mobile-contract`. Expected: PASS.
- [ ] **Step 5:** Commit: `App: pairs and connects through the relay`.

### Task 11: Macs by channel id in the app

**Files:**
- Modify: `apps/mobile/src/macs.ts`, `notifications.ts`, `activityDeny.ts`, `activity.ts`,
  `fleet/fleet.ts`, `apps/mobile/targets/octet-live/_shared/*.swift` (comments and any host
  check), their tests

**Interfaces:**
- Consumes: Task 10 `Pairing`.
- Produces: `isChannelId(text): boolean` (64 lowercase hex); `macOfChannel(pairings, channel): string | null`
  replaces `macOfHost`; `keyFor(pairings, channel, raw)`; `Tap` is `{ channel: string | null; session: string | null }`;
  `hostOf`, `hostKey`, `sameHost`, `isHostName` and `macNameOf(hubUrl)` are deleted, and a Mac's
  name is the `name` the hub sent, cleaned by `cleanMacName`.

- [ ] **Step 1: Failing tests**: `a_tap_naming_a_channel_opens_that_macs_session`,
  `a_tap_naming_a_hostname_names_no_mac`, `two_macs_on_one_relay_stay_two`,
  `deny_goes_to_the_mac_whose_channel_the_activity_named`,
  `the_macs_name_is_the_hubs_cleaned`, `a_missing_name_falls_back_to_Mac`.
- [ ] **Step 2:** Run the app tests. Expected: FAIL.
- [ ] **Step 3:** Implement. The Swift side must treat the value as an opaque string; change it
  only where it validates a host.
- [ ] **Step 4:** Run `make mobile-check`. Expected: PASS.
- [ ] **Step 5:** Commit: `App: a Mac is its channel, not its host`.

### Task 12: Documentation and the last sweep

**Files:**
- Modify: `docs/remote-control.md`, `apps/mobile/README.md`, `docs/tui.md`, `README.md` if it
  names Tailscale for the phone, `docs/superpowers/specs/2026-10-13-mobile-remote-design.md`
  (a note at its top naming what the new design replaces)

- [ ] **Step 1:** Rewrite the phone section of `docs/remote-control.md` around the three steps,
  what the relay sees, the confirmation code, moving relays, `octet hub revoke`, and the hub
  security notes. Shrink the SSH route to the short note the spec describes.
- [ ] **Step 2:** Run `grep -rni "tailscale\|ts\.net" crates apps/mobile/src docs/remote-control.md apps/mobile/README.md`.
  Expected: no line in code, and in docs only the short SSH note if it names Tailscale as one
  example of a network the user may already have.
- [ ] **Step 3:** Run `make rust-check`, `make mobile-check`, `make mobile-contract`, the Linux
  clippy command, and the Linux tests in Docker
  (`docker run --rm --init -v "$PWD":/w -w /w -v octet-linux-target:/target -v octet-linux-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target rust:1.98.1 bash -c 'cargo test --locked --workspace'`).
  Expected: all pass. Report each with its output.
- [ ] **Step 4:** Commit: `Docs: pairing a phone in three steps, with no Tailscale`.

## Owed after the plan (the owner's, not a task)

- The crypto benchmark on both phones (checkpoint under Task 3).
- Deploying the default relay and replacing `DEFAULT_RELAY`.
- A real iPhone and a real Android phone, each on cellular, against the hosted relay.
- A dedicated security review before anyone but the owner uses the hosted relay.
- Astra's full review of the design.
