# Relay connectivity: a phone link with no Tailscale

Status: approved by the owner on 2026-11-19, to go ahead to planning. Reviewed by a builder
check and, in part, by Astra (its review was cut short by a usage limit; the three findings it
reported are folded in, and a full run is still owed). Replaces the transport and pairing parts of
[the mobile remote design](2026-10-13-mobile-remote-design.md); everything else in that design
(the bridge, the hub's session model, the wire frames, push) stands.

## Why

Setting up the phone today takes seven manual steps: Tailscale on the Mac, Tailscale on the
phone, tmux, building the app, `octet hub`, `tailscale serve --bg 47823`, and
`/remote-control pair`. Two of those need an account with a third party, and a person without
Tailscale cannot use the phone app at all. The owner wants a user to do the minimum: install
the app, run one command, scan one code. The owner also wants the Tailscale dependency gone
altogether, and has agreed to host a small relay for Octet users.

## Goals

- **Three steps.** Install the app, run `octet pair` on the Mac, scan the QR code.
- **No Tailscale** anywhere in the phone link: not installed, not checked for, not mentioned.
- **Nothing to install besides Octet and the app**, and no account with anyone.
- **The relay is untrusted.** It forwards bytes it cannot read, and cannot impersonate either
  side.
- **Everything the phone does today still works**: watching sessions, approvals, prompts, mode
  and model, starting and resuming sessions, changes and commits, replay after a dropped
  connection, push, the Live Activity, up to 4 Macs per phone, several phones per Mac, revoke.
- **Self-hosting**: a user can point their hub at their own relay with one setting.

## Non-goals

- A session that outlives its terminal UI (still deferred; unchanged by this work).
- A direct phone-to-Mac path (same wifi, or peer-to-peer). Every connection goes through a relay.
- A web or desktop client, or a CLI that attaches to another machine's hub.
- Putting the app in an app store. It is still built from source; that is a separate project and
  the largest remaining setup step for anyone but the owner.
- Keeping the Tailscale route as an option. There is one transport after this work.
- Migrating existing pairings. Phones paired today pair again.

## Anti-goals

- The relay learning anything about a session beyond addresses, timing and sizes.
- The hub accepting a command from a device whose key it has not stored.
- Any listening network port on the Mac.
- A setup that works only if the user edits a config file.

## Constraints

- **Dependency policy.** `deny.toml` allows a fixed licence list and fails on a new one. The new
  crates (a TLS client, Noise, X25519, Ed25519) bring licences not on it today (ISC, and
  BSD-3-Clause for the dalek crates). Each needs a review and an explicit entry.
- **The hub has no TLS stack today.** It shells out to `curl` for Expo push. An outbound `wss://`
  connection needs one.
- **The app is Expo (React Native 0.86, Hermes).** Its socket passes text frames only
  (`apps/mobile/src/client/socket.ts`). Encrypted traffic is binary.
- **iPhone and Android are both targets.** Everything here must work on both, and "keychain"
  below means `expo-secure-store` on either: the iOS keychain or the Android Keystore.
- **Protocol frames are JSON text up to 96 KiB** (`octet-core/src/remote/protocol.rs`). A Noise
  transport message carries at most 65,535 bytes.
- **House rules**: cargo through `scripts/rust-env.sh`, lines at most 120 columns, every item
  documented, warnings are errors, `make rust-check` and `make mobile-check` are the gates, and
  Linux parity is checked locally.
- **Octet never reads vendor credentials**; nothing here changes that.

## Approaches

Three ways to carry traffic between a phone on cellular and a Mac behind a home router were
considered. Each leaves the user with one command and one scan.

1. **A relay Octet hosts (chosen).** The hub dials out to a small relay; the phone meets it
   there; traffic is end-to-end encrypted with keys exchanged through the QR code. The user
   installs nothing extra, and the app keeps using a WebSocket. Costs: the owner runs a service
   that must stay up, and Octet gains encryption code that needs a careful review. This is the
   model Paseo uses.
2. **A peer-to-peer library** (dial by public key, hole punching, the library maintainers' relays
   as fallback). Nothing to host, but the phone needs a native module wrapping the library on
   both iOS and Android, which is the largest build effort, and how well that works inside an
   Expo app was not confirmed.
3. **A third-party tunnel program.** It trades one dependency for another, and a free tunnel's
   address changes on every restart, so the phone would lose the Mac.

The owner picked 1 and agreed to host the default relay, with a setting for self-hosting.

## Design

### 1. Components

```
phone app ──wss──▶ relay ◀──wss── hub ──unix sockets──▶ terminal UIs (unchanged)
          └──────── end-to-end encrypted (Noise) ────────┘
```

**Relay (`crates/octet-relay`, new).** A small WebSocket service that splices one phone socket to
one hub socket and forwards binary messages between them. It keeps no accounts, stores nothing,
and never holds a key that can read traffic. It speaks plain WebSocket and expects the hosting
platform's proxy to terminate TLS.

**Hub (`crates/octet-hub`).** It stops listening on TCP. A new module holds an outbound control
connection to the relay, opens one data connection per arriving phone, runs the encrypted
handshake, and hands the decrypted frame stream to the connection code that exists today
(queues, replay, rate limit, pings). `owner.rs`, the `Tailscale-User-Login` gate, the `Origin`
check and the "published" state are removed.

**Phone app (`apps/mobile`).** Its socket connects to the relay, runs the handshake, and then
exchanges the same frames as today inside the encrypted channel. A pairing stores a private key
where it stores a device token today.

**Terminal UI.** `/remote-control pair` changes; the bridge does not.

### 2. Keys and identities

- **Hub identity key**: one X25519 key pair, made on first use, kept in `<data>/hub.key`
  (mode 0600). Its public key is what a phone pins at pairing. Deleting the file unpairs every
  phone.
- **Hub relay key**: one Ed25519 key pair in the same file. The hub's **channel id** is the
  SHA-256 of this public key. The hub proves it owns the channel by signing the relay's
  challenge, so nobody else can register it.
- **Phone key**: one X25519 key pair per pairing, made on the phone at pairing and kept in the
  keychain under the same rules as the token today (this device only, readable while unlocked).
  The hub stores the phone's public key in `devices.json` in place of `token_sha256`. A device
  is its public key: revoking deletes it.
- Device tokens, `hello`'s `token` field and the `login` field of a device record go away.
- **A Mac's identity on the phone is its channel id.** Today the app tells Macs apart by
  hostname (`macOfHost` in `fleet/fleet.ts`, `hostKey` in `macs.ts`, `data.mac` in
  `notifications.ts`, the Deny path in `activityDeny.ts`). Every Mac now shares the relay's
  hostname, so each of those keys on the channel id instead: 64 lowercase hex characters, with
  its own validator. Pairing again with a channel id already held replaces that pairing; a new
  one counts toward the 4-Mac limit.
- **A Mac's name on the phone** was its hostname. `paired` and `welcome` now carry `name`, the
  Mac's own host name (at most 64 characters, display only), which the app shows and never
  trusts.

### 3. Encryption

**Noise, not hand-rolled.** The channel is `Noise_IK_25519_ChaChaPoly_SHA256`: the phone is the
initiator and already knows the hub's static key from the QR code; the hub learns the phone's
static key, encrypted, in the first message. One round trip, fresh keys for every connection.

- **Hub**: the `snow` crate.
- **Phone**: a small in-repo TypeScript implementation of this one pattern over audited
  primitives (`@noble/curves`, `@noble/ciphers`, `@noble/hashes`), with randomness from
  `expo-crypto`. No native module.
- **Both are checked against the same test vectors**, in both roles, and against each other in an
  end-to-end test.
- **Handshake messages carry no payload** and must be exactly their fixed sizes (96 bytes, then
  48); anything else closes the connection. Nothing that matters travels before the handshake
  completes, because the first message of this pattern can be replayed.
- **After the handshake**, each WebSocket binary message is one Noise transport message. Its
  plaintext is one flag byte (`0` more follows, `1` last) and a chunk of a UTF-8 JSON frame of
  at most 65,000 bytes; the receiver joins chunks until the last and refuses a frame over the
  existing 96 KiB limit.
- A failed decryption closes the connection. There is no fallback to an unencrypted mode.

### 4. Relay protocol

All paths are under `/v1`. Messages on the control connection are JSON text; data connections
carry binary only.

- **Hub control**: `GET /v1/hub/<channel id>`. The relay sends `{"challenge": <32 random bytes,
  hex>}`; the hub answers `{"key": <Ed25519 public key>, "signature": <over "octet-relay-v1" ‖
  challenge>}`. The relay checks the signature and that the key hashes to the channel id, then
  holds the connection as that channel's hub. A second valid registration replaces the first.
- **Phone**: `GET /v1/phone/<channel id>`. With no hub registered the relay closes with code
  4404, which the app shows as "Can't reach your Mac". Otherwise the relay sends the hub
  `{"connect": <connection id>}` on the control connection.
- **Hub data**: `GET /v1/hub/<channel id>/<connection id>`, opened by the hub within 10 seconds.
  The relay splices it to the waiting phone socket and forwards binary messages unchanged in both
  directions until either side closes, then closes the other.
- **Keepalive**: the relay pings every connection every 20 seconds and drops one that misses two.
- **Limits** (the relay enforces all of them, and the numbers are the builder's to tune):
  a message size cap a little over 65,535 bytes, at most 16 phone connections per channel, a
  per-address limit on new connections and on channel registrations, a byte-rate limit per
  connection with a burst allowance large enough for a journal replay, and a 10 second limit on
  an unspliced phone socket.
- **No buffering.** A message for a side that is gone is dropped with the connection. Catching
  up is the job of the cursors the phone already keeps.
- **Logging**: counts and errors only. No channel ids and no addresses in logs.

### 5. Hub behaviour

- **When it dials.** The hub connects to the relay only while it has at least one paired device
  or an open pairing. A hub nobody has paired with makes no outbound connection.
- **Reconnecting.** A lost control connection is retried on the app's schedule: 0.5 seconds
  doubling to 30, with jitter. Sessions at the desk are unaffected by the relay being down.
- **Per phone.** On `connect`, the hub opens the data connection, runs the handshake as
  responder, and then requires the first frame within 10 seconds:
  - a known phone key must send `hello` (no token), and is then authenticated as that device;
  - an unknown key must send `pair` with a valid one-time secret, and is closed otherwise.
- **Existing limits stay**: 64 open connections, 32 unauthenticated, 60 commands a minute per
  device, the revoked check on every frame.
- **TLS**: `rustls` through `tokio-tungstenite`, with the webpki root set compiled in, so a hub
  run as a service needs nothing from the environment.
- **Relay address**: a constant for the default relay, overridden by `"relay"` in `hub.json`.
  Only `wss://` addresses are accepted, except `ws://127.0.0.1` in debug builds for tests.

### 6. Pairing

**One command.** `octet pair` on the Mac:

1. Starts the hub if it is not running, by installing the per-user service exactly as
   `octet hub install` does today.
2. Waits for the relay to accept the hub.
3. Prints a QR code and the same content as a link, valid once and for 10 minutes.
4. When a phone sends `pair`, shows a six-digit code with the phone's name and platform, and
   asks `Phone shows 482 913? Pair "<name>"? [y/N]`. The phone shows the same code while it
   waits. The device is stored only on `y`; anything else, or two minutes without an answer,
   refuses the phone. Either way the one-time secret is spent.
5. Says if `tmux` is missing: the phone can watch and answer, but cannot start sessions.

**The code.** Both sides compute it from the handshake hash of the connection the `pair`
arrived on (the first four bytes of SHA-256 over `"octet-pair-code-v1"` and the hash, read as
a big-endian number, modulo 1,000,000, padded to six digits). It depends on both static keys and both ephemeral keys, so it identifies the
exact phone at the other end: a different phone using a copied QR code produces a different
code, which the owner's phone is not showing. The name and platform are the phone's own claim
and are shown for convenience only. One pairing waits for an answer at a time; a second `pair`
while one waits is refused.

`/remote-control pair` in the terminal UI does the same, with the confirmation as a prompt in
the UI. Plain `/remote-control` reports whether the hub is running, whether the relay has it,
and how many phones are paired.

**The link.** `https://<relay host>/pair#<base64url>`, where the payload is one version byte
(`1`), the hub's 32-byte identity public key, the 32-byte channel id and the 32-byte one-time
secret. The relay serves a static page at `/pair` that says to open the link in the Octet app;
the fragment never reaches the relay. The app accepts any `https` host, since trust rests on the
hub's key and not on the relay.

**Why the confirmation.** Today a pairing needs the code and a device signed in to the owner's
tailnet. Without Tailscale the code alone would be enough, from anywhere on the internet. The
confirmation at the Mac restores a second check for one keypress.

**On the phone.** The app makes the key pair, connects, completes the handshake, sends `pair`
with the secret, its name and platform, shows the six-digit code with "Check this code on your
Mac", waits for `paired`, and then stores the private key, the hub's public key, the channel id
and the relay address under the pairing's id. `paired` no longer carries a token.

**Moving relays.** A phone keeps the relay address it paired through. If the hub's relay
changes (the default moves, or `"relay"` is added to `hub.json`), paired phones can no longer
reach it and must pair again; `octet pair` says so when it sees paired devices and a relay
address that differs from the one they paired through. The default relay's hostname is meant
to be permanent.

### 7. Phone app changes

- **Link parsing** (`client/link.ts`): the new link shape; the `.ts.net` rule and its message are
  removed.
- **Socket** (`client/socket.ts`): binary frames, with the Noise handshake and chunking in a new
  module between the socket and `HubClient`. `HubClient` still sees whole JSON frames.
- **Client states**: unchanged. A relay close of 4404 is `offline`, not `unpaired`; only the
  hub's `unpaired` or `revoked` refusal forgets a pairing, as today.
- **Keychain**: one item per pairing holding the private key, with the same access rule as the
  token. Old token items are deleted on first launch.
- **Push `mac`**: a push names the Mac by its channel id where it carried the published host.
  The hub always has one, so `mac` is always present; `notifications.ts` validates it as a
  channel id.
- **Background connections** (handing over a Live Activity token, the Island's Deny) connect
  through the relay and run the handshake like any other. Deny has about 10 seconds to connect,
  finish the handshake and get its ack, so handshake time on the phone is part of the
  measurement under Risks.

### 8. What is removed

- `crates/octet-hub/src/owner.rs`, the handshake gate and `foreign_origin` in `server.rs`, the
  loopback listener and `--port`, `--owner`, and the port in `hub.json`.
- The Tailscale reads in `crates/octet-tui/src/remote.rs` and `crates/octet/src/{hub,args,main}.rs`,
  and `NOT_TS_NET` in the app.
- **The terminal route's check.** Plain `/remote-control` today checks tmux, Tailscale SSH and
  mosh for reaching the terminal UI over SSH. That check is removed with the rest. The SSH
  route itself is only documentation; its guide shrinks to a short note that the terminal UI
  works over any SSH connection the user already has, inside tmux.

### 9. Security model

- **A compromised or malicious relay** can see addresses, channel ids, timing and sizes, and can
  drop or delay messages. It cannot read or change a frame (authenticated encryption), cannot
  pose as the hub (it lacks the identity key the phone pinned) and cannot pose as a phone (it
  lacks a stored phone key).
- **Anyone on the internet who learns a channel id** can deliver bytes to the hub's handshake.
  The handshake accepts only an exact-size first message, the relay limits attempts, and an
  unknown key that does not present a valid pairing secret is closed. The handshake and frame
  reassembly get fuzz tests.
- **A photographed pairing code** is single-use, expires in 10 minutes, and still needs the
  confirmation at the Mac, where the six-digit code will not match the one on the owner's own
  phone.
- **A lost phone** is revoked with `octet hub revoke`, as today.
- **Forward secrecy**: every connection uses fresh keys, so recording traffic and later stealing
  a long-term key does not decrypt it.
- **Unchanged**: `full-access` from the phone still needs biometrics and a typed confirmation;
  push text stays content-free; the workspace allow-list for phone-started sessions stays.
- **Availability**: if the relay is down, no phone can connect. The desk is unaffected.

### 10. Hosting

- The relay is one small container: a single Rust binary behind the platform's TLS proxy, with
  a health endpoint. It needs no database and no disk.
- **Exactly one instance.** The channel table is in memory, so the relay must run as a single
  instance on a platform that allows long-lived WebSockets and does not autoscale it. Running
  several is out of scope.
- **Client addresses.** Behind a proxy the per-address limits need the real client address. The
  relay reads it from the header named by `OCTET_RELAY_CLIENT_ADDRESS_HEADER`, and from the
  socket's peer address when that is unset. The deploy guide names the header for the platform
  chosen.
- **What this work delivers**: the relay's code, its `Dockerfile`, an image that builds, and a
  deploy guide. Deploying the default relay needs the owner's hosting account and domain, so
  the owner does that step, following the guide. Until then the hub's default relay constant is
  a placeholder and development uses a relay on `127.0.0.1`.
- The owner runs the default relay. Its hostname is a constant in the hub and is the one thing
  only the owner can supply (see Open items).
- Self-hosting is `docker run` of the same image and one line in `hub.json`. The QR code carries
  the relay address, so the app needs no setting.

## Testing

- **Noise**: shared test vectors on both implementations, both roles; a Rust-to-TypeScript
  handshake and transport test in each direction.
- **Relay**: registration with a good and a bad signature, takeover attempts, splice and close in
  both directions, every limit, 4404 with no hub.
- **Hub**: the existing connection and subscription tests run over an in-process relay; pairing
  with confirmation accepted and refused; an unknown key without a secret; a revoked key.
- **App**: link parsing, the encrypted socket against a fake relay, the existing `HubClient`
  suites over it.
- **Fuzzing**: handshake message sizes and contents, chunk reassembly.
- **By hand, owed before release**: an iPhone and an Android phone, each on cellular, against
  the hosted relay.

## Left to the builder

- Module layout inside the hub and the app, and how the existing hub tests reach an in-process
  relay.
- The exact numbers for the relay's limits.
- Whether `snow` uses its default primitives or another resolver, given the licence review.
- The relay's deployment files, beyond a `Dockerfile`.

## Calls made in writing this (the owner can overturn any)

- `octet pair` as the one command, with `/remote-control pair` doing the same in the UI.
- The confirmation at the Mac when a phone pairs, by a six-digit code shown on both.
- A relay change means pairing again; there is no way to move a pairing between relays.
- The owner deploys the default relay; this work delivers the code, the image and a guide.
- **Licences.** The owner was asked and has not answered, so this is assumed: the permissive
  licences the new crates bring (ISC, BSD-3-Clause, and CDLA-Permissive-2.0 if the compiled-in
  root set needs it) may be added to `deny.toml`, each as a reviewed, explicit entry scoped to
  the crate that needs it where `deny.toml` allows. A copyleft or non-permissive licence is not
  covered by this and stops the work for the owner's decision.
- Noise IK over audited JavaScript primitives, with no native module on the phone.
- One data connection per phone at the relay, not one shared multiplexed connection.
- The hub dials the relay only once a phone is paired or pairing.
- No migration: existing phones pair again.
- The SSH terminal route keeps a short note in the docs and loses its `/remote-control` check.

## Risks

- **Encryption speed on the phone.** Pure-JavaScript crypto in Hermes may be slow. Measure it
  first, on the owner's iPhone and Android phone, before building the rest: ChaCha20-Poly1305
  throughput for a long journal replay, and the handshake's X25519 operations against the Deny
  path's 10 second budget. The Android phone is the likely worst case and sets the result. The
  fallback is a native crypto module, which must then exist for both platforms.
- **A service to keep running.** The relay's uptime is now part of the product.
- **Abuse.** The relay forwards for anyone who registers a channel. The limits above bound it;
  they may need tightening once it is public.
- **New security-critical code.** It needs a dedicated review before the hosted relay is used by
  anyone but the owner.

## Open items

- The default relay's hostname, which only the owner can choose.
