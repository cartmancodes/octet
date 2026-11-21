# Relay connectivity: a phone link with no Tailscale

Status: approved by the owner on 2026-11-19, to go ahead to planning. Reviewed by a builder
check and by Astra. Astra's full review (20 findings, one critical) came after the approval and
is folded in below; what it changed is listed under "Changed after review", for the owner to
check. Replaces the transport and pairing parts of
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
- **Protocol frames are JSON text.** The hub takes at most 96 KiB from a phone (`MAX_MESSAGE`,
  `octet-core/src/remote/protocol.rs`). The phone takes at most 256 KiB from the hub
  (`MAX_FRAME_BYTES`, `apps/mobile/src/client/frames.ts`) and ignores a larger frame; a journal
  event can be that large. A Noise transport message carries at most 65,535 bytes.
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
  (mode 0600). Its public key is what a phone pins at pairing. The file is made with an exclusive
  create, so two hubs starting together cannot each make one. A file that cannot be read stops
  the hub with an error naming it; it is never replaced silently.
- **Resetting the hub's identity** is deleting `hub.key` and restarting the hub. A hub that makes
  a new key while `devices.json` still lists devices empties that list first, with their push and
  Live Activity tokens: those phones pinned the old key and can never connect again, and must not
  keep getting notifications.
- **Hub relay key**: one Ed25519 key pair in the same file. The hub's **channel id** is the
  SHA-256 of this public key. The hub proves it owns the channel by signing the relay's
  challenge, so nobody else can register it.
- **Phone key**: one X25519 key pair per pairing, made on the phone at pairing and kept in the
  keychain under the same rules as the token today (this device only, readable while unlocked).
  The hub stores the phone's public key in `devices.json` in place of `token_sha256`. A device
  is its public key: revoking deletes it.
- Device tokens, `hello`'s `token` field and the `login` field of a device record go away.
- **A Mac's identity on the phone is its channel id.** The app keeps its own generated pairing
  id (`mXXXXXXXX`) as the key for sessions, storage, navigation and Live Activity ownership; that
  does not change. What changes is how a Mac is found from outside the app. Today that is its
  hostname (`macOfHost` in `fleet/fleet.ts`, `hostKey` in `macs.ts`, `data.mac` in
  `notifications.ts`, the Deny path in `activityDeny.ts`, the Allow link in `navigation.ts`, the
  `mac` of a Live Activity's content state). Every Mac now shares the relay's hostname, so each
  of those names the Mac by its channel id instead: 64 lowercase hex characters, with its own
  validator. A pairing gains three stored fields: the channel id, the pinned hub key and the
  relay address. Pairing again with a channel id already held replaces that pairing, once the
  hub has answered `paired`, and keeps its local id and its sessions' cursors; a new channel id
  counts toward the 4-Mac limit. A notification or Live Activity left over from before, naming a
  hostname, names no Mac: tapping it opens the app and nothing more.
- **A phone that pairs again** makes a new key, so the hub cannot tell it is the same phone. The
  phone's name decides: when a pairing is confirmed under a name the hub already holds, the new
  key replaces the old one in one step. The old key's connections close and its push and Live
  Activity tokens are dropped. The prompt at the Mac says `Replace` in place of `Pair`, so the
  owner sees it. A phone pairing again with a channel it holds sends the name it paired under,
  whatever is typed (the app asks for no name then), so the old key never stays. This is how a phone comes back after a relay move, a lost `paired`, or a wiped
  app, without leaving its old key authorised.
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
- **Prologue**: the ASCII bytes `octet-link-v1` on both sides, so a later incompatible version
  fails the handshake instead of misreading frames.
- **Handshake messages carry no payload** and must be exactly their fixed sizes (96 bytes, then
  48); anything else closes the connection. Nothing that matters travels before the handshake
  completes, because the first message of this pattern can be replayed. Each side makes a fresh
  ephemeral key per connection and refuses an all-zero shared secret.
- **After the handshake**, each WebSocket binary message is one Noise transport message, with
  empty associated data and Noise's own counter as the nonce, so a replayed, dropped or
  reordered message fails to decrypt. Its plaintext starts with one type byte:
  - `0` (more follows) or `1` (last): a chunk of a UTF-8 JSON frame, at most 65,000 bytes. The
    receiver joins chunks until the last. A sender finishes one frame's chunks before it starts
    another's.
  - `2` (ping) and `3` (pong): exactly 8 bytes after the type byte. A pong carries its ping's
    bytes back.
  - Any other type byte, a chunk with no bytes, a ping or pong of another length, or a frame that
    is not valid UTF-8 closes the connection.
- **Frame size, by direction.** The hub closes a connection whose frame passes 96 KiB, as it does
  today. The phone drops a frame that passes 256 KiB, up to that frame's last chunk, and carries
  on, as it ignores one today. Closing there would trap a replay: the same event would arrive
  again on every reconnect.
- **Liveness is inside the channel.** The relay answers WebSocket pings for both sides, so those
  prove only that the relay is up. The hub sends an encrypted ping with 8 random bytes every 20
  seconds and closes a connection that has not returned the last one when the next is due. A
  relay therefore cannot keep a phone looking present (which would hold back its notifications)
  or hold a command back for longer than that window.
- A failed decryption closes the connection. There is no fallback to an unencrypted mode.

### 4. Relay protocol

All paths are under `/v1`. Messages on the control connection are JSON text; data connections
carry binary only.

- **Encodings.** Keys, signatures, challenges and connection ids are lowercase hex in JSON. The
  signature is Ed25519 over the 14 ASCII bytes `octet-relay-v1` followed by the 32 raw challenge
  bytes. A control message is at most 1 KiB; an unknown one is ignored.
- **Hub control**: `GET /v1/hub/<channel id>`. The relay sends `{"challenge": <32 random
  bytes>}`; the hub answers within 10 seconds with `{"key": <Ed25519 public key>, "signature":
  <64 bytes>}`. The relay checks the signature and that the key hashes to the channel id, answers
  `{"registered": true}`, and holds the connection as that channel's hub. A second valid
  registration replaces the first, which is closed with code 4409.
- **Phone**: `GET /v1/phone/<channel id>`. With no hub registered the relay closes with code
  4404, which the app shows as "Can't reach your Mac". Otherwise the relay sends the hub
  `{"connect": <connection id>}` on the control connection.
- **Connection ids** are 16 random bytes the relay makes for each phone socket. One is good
  once, for 10 seconds, and only for the registration it was sent to. It is the hub's proof that
  it holds that registration, so nobody who merely knows a channel id can take a waiting phone.
- **Hub data**: `GET /v1/hub/<channel id>/<connection id>`, opened by the hub within those 10
  seconds. The relay splices it to the waiting phone socket and forwards binary messages
  unchanged in both directions until either side closes, then closes the other. An id that is
  unknown, used or expired is refused.
- **Data sockets belong to a registration.** When a hub's control connection ends or is
  replaced, the relay closes that registration's phone and data sockets. Phones reconnect and
  resume from their cursors.
- **Keepalive**: the relay pings every connection every 20 seconds and drops one that misses two.
  This keeps proxies from idling sockets out; it is not the hub's liveness check (section 3).
- **Limits** (the relay enforces all of them, and the numbers are the builder's to tune):
  a message size cap a little over 65,535 bytes; at most 16 phone sockets per channel, counting
  waiting and spliced, foreground and background alike; a ceiling on channels and on sockets
  across the whole relay, and one on connections still sending their HTTP request (counted from
  the moment they are accepted, so a flood of slow requests cannot use up file descriptors); a
  per-address limit on new connections and on channel registrations; a
  byte-rate limit per connection with a burst allowance large enough for a journal replay; and a
  10 second limit on an unspliced phone socket. Over a limit, the relay refuses the newcomer and
  never drops an established splice.
- **Browsers are not clients.** A request whose `Origin` header names another host than its
  `Host` is refused. This is a second line only; the encryption is the first.
- **No buffering.** A message for a side that is gone is dropped with the connection. Catching
  up is the job of the cursors the phone already keeps.
- **Logging**: counts and errors only. No channel ids and no addresses in logs.

### 5. Hub behaviour

- **When it dials.** The hub connects to the relay only while it has at least one paired device
  or an open pairing. A hub nobody has paired with makes no outbound connection.
- **Reconnecting.** A lost control connection is retried on the app's schedule: 0.5 seconds
  doubling to 30, with jitter. Sessions at the desk are unaffected by the relay being down.
- **Per phone.** On `connect`, the hub opens the data connection, runs the handshake as
  responder, and requires the first frame. One deadline of 10 seconds covers all of it, counted
  from the `connect`:
  - a known phone key must send `hello` (no token), and is then authenticated as that device;
  - an unknown key that sends `hello` gets the encrypted refusal `unpaired` and is closed, so a
    revoked phone learns it is revoked and forgets the pairing instead of waiting for ever;
  - an unknown key that sends `pair` with a valid one-time secret starts a pairing (section 6);
  - anything else is closed.
- **The hub does not trust the relay's limits.** A hostile relay can send any number of
  `connect`s. The hub takes a place in its own room before it dials: 64 connections in all, 32
  not yet authenticated, counting those still dialling or in the handshake. With no place free
  it ignores the `connect`. It also takes at most 120 `connect`s a minute and ignores a
  connection id that is malformed or already seen.
- **Existing limits stay**: 60 commands a minute per device, and the revoked check on every
  frame.
- **A phone waiting for the owner's answer** to a pairing is held past the 10 seconds, for the 2
  minutes the confirmation may take, and counts as one unauthenticated connection.
- **TLS**: `rustls` through `tokio-tungstenite`, with the webpki root set compiled in, so a hub
  run as a service needs nothing from the environment.
- **Relay address**: a constant for the default relay, overridden by `"relay"` in `hub.json`,
  which must be `wss://<host>` or `wss://<host>:<port>` with no path, query or user name. Debug
  builds also take a `ws://` address from the environment, for tests and for trying a phone
  against a relay on the same network before one is hosted.

### 6. Pairing

**One command.** `octet pair` on the Mac:

1. Starts the hub if it is not running, by installing the per-user service exactly as
   `octet hub install` does today.
2. Opens a pairing (which is what makes an unpaired hub dial the relay) and waits for the relay
   to accept the hub.
3. Prints a QR code and the same content as a link, valid once and for 10 minutes.
4. When a phone sends `pair`, shows a six-digit code with the phone's name and platform, and
   asks `Phone shows 482 913? Pair "<name>" (<platform>)? [y/N]`. The phone shows the same code
   while it waits. The device is stored only on `y`; anything else, or two minutes without an
   answer, refuses the phone.
5. Says if `tmux` is missing: the phone can watch and answer, but cannot start sessions.

**The code.** The hub picks six random digits when it accepts a `pair`, and sends them to that
phone inside the encrypted channel, in a `pair_code` frame. The phone shows a code only when
that frame arrives; it never works one out for itself. So a code on the phone's screen means
the hub is holding *that phone's* connection for the owner's answer, and the answer applies to
that phone's key and nothing else.

- The one-time secret is spent the moment a `pair` is accepted, whatever happens next.
- One pairing waits at a time. A second `pair` meanwhile is refused and gets no code.
- A pairing ends if its connection closes, and the prompt at the Mac ends with it. Starting
  again needs a new `octet pair`.
- The name and platform are the phone's own claim and are shown for convenience only.

An earlier draft took the code from the handshake's hash. Astra's review showed that fails: the
hash is made of public bytes, so a hostile relay holding a copied link could try handshakes
until its own code matched the one on the owner's phone, at about a thousand tries a side. A
random code that only the held connection receives leaves a copied link one guess in a million.

**The link.** `octet://pair/<base64url, no padding>`, where the payload is one version byte
(`1`), the hub's 32-byte identity public key, the 32-byte channel id, the 32-byte one-time
secret, and then the relay's address as ASCII `host` or `host:port`. The app connects to that
address over `wss`. Version byte `0` means `ws`; only debug builds of the hub make such a link
and only development builds of the app accept one.

The link opens the app directly, from the app's own scanner, from the phone's camera, or
pasted. An earlier draft used an `https` link to a page on the relay. That put the relay's own
page between the scan and the app whenever the link opened in a browser, where a hostile relay
could read the secret from the address and show a convincing code of its own. Nothing the relay
serves now sees the link, and the relay has no pairing page.

`/remote-control pair` in the terminal UI shows the same QR code and asks the same question as
a prompt in the UI. It does not install anything: the terminal UI never changes system
settings, as before. When the hub is not running it says to run `octet pair` in a terminal
first. Plain `/remote-control` reports whether the hub is running, whether the relay has it,
and how many phones are paired.

**Why the confirmation.** Today a pairing needs the code and a device signed in to the owner's
tailnet. Without Tailscale the code alone would be enough, from anywhere on the internet. The
confirmation at the Mac restores a second check for one keypress.

**On the phone.** The app makes the key pair, connects, completes the handshake, sends `pair`
with the secret, its name and platform, waits for `pair_code` and shows it with "Check this
code on your Mac", waits for `paired`, and then stores the private key, the hub's public key,
the channel id and the relay address under the pairing's id. `paired` no longer carries a
token. The app waits 130 seconds for the answer (the hub's two minutes and a margin), not the
45 seconds of an ordinary request.

**Between the command and the hub.** `octet pair` and the terminal UI talk to the hub over its
control socket, which today answers one line and closes. Pairing needs three requests: open a
pairing and get the link; wait for a phone (answered when one is held, or when the secret
expires); answer yes or no for that one held phone. One pairing is open at a time: opening
another replaces a secret no phone has used yet, and is refused while a phone waits for an
answer. Each opening has an id, its attempt, which the link comes back with and the wait
must name. An opening replaced while it waited for the relay gets no link, and is told it
expired; a wait that names any attempt but the newest is told the same, so a phone that came
through the newer link is asked about only in the terminal that opened it. The held phone has
an id too, which the answer names. Both ids are 32 random hex digits, never a count, so a
terminal still showing a question from before the hub restarted cannot answer for a phone held
since. If the device cannot be saved, the answer says so and the phone is refused.

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
  hub's encrypted `unpaired` or `revoked` refusal forgets a pairing, as today. Nothing the relay
  says can make the app forget a Mac. One case stays `offline` for good: a hub whose last phone
  was revoked stops dialling, so that phone only ever sees 4404. The user removes the Mac in the
  app, as they can today.
- **Keychain**: one item per pairing holding the private key, with the same access rule as the
  token. Old token items are deleted on first launch.
- **Push `mac`**: a push names the Mac by its channel id where it carried the published host.
  The hub always has one, so `mac` is always present; `notifications.ts` validates it as a
  channel id.
- **The Live Activity** names the Mac the same way everywhere it names one: the `mac` of the
  content state the hub sends through APNs, the state the app sets in the foreground, the
  widget's stored entries, the Deny event, and the Island's Allow link, which becomes
  `octet://approve/<channel id>/<session>/<approval>`. That link is checked as a DNS name today,
  with labels of at most 63 characters, and would refuse a 64-character channel id.
- **Background connections** (handing over a Live Activity token, the Island's Deny) connect
  through the relay and run the handshake like any other. Deny has about 10 seconds for the
  whole trip: name lookup, TLS, the WebSocket, the relay's splice, the handshake, `hello` and
  the ack. That whole trip is what gets measured under Risks, not the handshake alone. When it
  does not finish, the app opens for the answer, as it does today.

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

- **What Tailscale did that nothing does now.** Today every connection, not only pairing, must
  come from a device signed in to the owner's tailnet. That check goes. After this work a
  phone's stored key is the whole of its authority: someone who extracts it can connect from
  anywhere until the device is revoked, and removing a device from a tailnet no longer cuts it
  off. The key is kept in the keychain, for this device only. The typed confirmation for
  `full-access` and the biometric check are the app's; a program written to hold a stolen key
  could skip the second and supply the first. The owner asked for Tailscale to go, and this is
  what goes with it.
- **A compromised or malicious relay** can see addresses, channel ids, timing and sizes, and can
  drop or delay messages, or cut a connection. It cannot read or change a frame (authenticated
  encryption), cannot pose as the hub (it lacks the identity key the phone pinned), cannot pose
  as a phone (it lacks a stored phone key), cannot keep a departed phone looking present past
  the liveness window, and never sees a pairing link.
- **Anyone on the internet who learns a channel id** can deliver bytes to the hub's handshake.
  The handshake accepts only an exact-size first message, the hub bounds how many it will
  entertain, and an unknown key that does not present a valid pairing secret is refused. The
  handshake and frame reassembly get fuzz tests. Such a person can also fill a channel's 16
  places at the relay for 10 seconds at a time, keeping its phones out; the relay's per-address
  limit is the only brake on that.
- **A photographed pairing code** is single-use, expires in 10 minutes, and still needs the
  confirmation at the Mac. Whoever uses it first is the one held: the Mac shows a code, and the
  owner's own phone shows a refusal and no code, so the owner answers no. A guess at the code is
  one in a million, once.
- **A lost phone** is revoked with `octet hub revoke`, as today.
- **Forward secrecy** covers what was sent, not who sent it: every connection uses fresh keys,
  discarded when it ends, so recording traffic and later stealing a long-term key does not
  decrypt the frames. Stealing the hub's key does reveal which phone key opened each recorded
  connection, because this pattern sends the phone's key encrypted to the hub's.
- **Unchanged**: `full-access` from the phone still needs biometrics and a typed confirmation;
  push text stays content-free; the workspace allow-list for phone-started sessions stays.
- **Availability**: if the relay is down, no phone can connect. The desk is unaffected.

### 10. Hosting

- The relay is one small container: a single Rust binary behind the platform's TLS proxy, with
  a health endpoint. It needs no database and no disk.
- **Exactly one instance.** The channel table is in memory, so the relay must run as a single
  instance on a platform that allows long-lived WebSockets and does not autoscale it. A deploy
  stops the old instance before it starts the new one; two running at once would split hubs
  from their phones. Every connection drops at a deploy and comes back by itself within the
  reconnect schedule.
- **Client addresses.** Behind a proxy the per-address limits need the real client address. The
  relay reads it from the header named by `OCTET_RELAY_CLIENT_ADDRESS_HEADER`, taking the last
  address when the header holds several, and from the socket's peer address when that is unset.
  With the header set, the relay's port must be reachable only through the proxy, or anyone
  could write the header. The deploy guide names the header for the platform chosen.
- **The proxy's logs** record request paths, which contain channel ids. The deploy guide says to
  turn request logging off or strip the path.
- **Checking it works.** `/healthz` only shows the process is up. The relay binary has a probe
  mode that registers a throwaway channel on a running relay and passes a message each way
  through it; the deploy guide uses it after every deploy and as the periodic outside check.
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
  both directions, every limit, 4404 with no hub, a connection id used twice or by a stranger,
  a registration's sockets closing with it.
- **Hub**: the existing connection and subscription tests run over an in-process relay; pairing
  with confirmation accepted and refused; an unknown key without a secret; a revoked key; a
  flood of `connect`s; a phone that stops answering encrypted pings.
- **Pairing against a hostile relay**: a first message replayed many times gets no code and
  pairs nothing; a second phone with the same secret gets no code; the code reaches only the
  held connection; a held phone that drops ends the prompt and the secret stays spent.
- **App**: link parsing, the encrypted socket against a fake relay, the existing `HubClient`
  suites over it.
- **Fuzzing**: handshake message sizes and contents, chunk reassembly.
- **By hand, owed before release**: an iPhone and an Android phone, each on cellular, against
  the hosted relay.

## Left to the builder

- Module layout inside the hub and the app, and how the existing hub tests reach an in-process
  relay.
- The exact numbers for the relay's limits, and the wording of the deploy guide.
- Whether `snow` uses its default primitives or another resolver, given the licence review.
- The relay's deployment files, beyond a `Dockerfile`.

## Calls made in writing this (the owner can overturn any)

- `octet pair` as the one command. `/remote-control pair` shows the code and asks the question
  in the UI, and leaves starting the hub to `octet pair`.
- The confirmation at the Mac when a phone pairs, by a six-digit code shown on both.
- Noise IK with phone keys, kept over the alternative Astra raised (a pattern that proves only
  the hub, with today's device tokens sent inside the channel). A token is a secret that can be
  copied and replayed; a key proves itself afresh on every connection and lets the hub know the
  phone before it reads a frame.
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

## Changed after review

Astra's full review arrived after the owner approved this design. These are the places it
changed what was approved; the rest of its findings tightened wording or filled gaps.

- **The pairing code is random and comes from the hub** (section 6). The approved version
  derived it from the handshake, which a hostile relay with a copied link could defeat.
- **The pairing link is `octet://pair/…`, and the relay serves no page** (section 6). The
  approved version was an `https` link to a page on the relay.
- **A phone that pairs under a name already held replaces that device** (section 2). The
  approved version left the old key authorised.
- **`/remote-control pair` no longer installs the hub service** (section 6); only `octet pair`
  does.
- **Liveness pings are encrypted, and the phone's frame limit is 256 KiB** (section 3).
- **Deleting `hub.key` also clears the paired devices** (section 2).
- **What removing Tailscale gives up** is written down (section 9), not changed.

## Risks

- **Encryption speed on the phone.** Pure-JavaScript crypto in Hermes may be slow. Measure it
  first, on the owner's iPhone and Android phone, before building the rest: ChaCha20-Poly1305
  throughput for a long journal replay, and the handshake's X25519 operations. The Android phone
  is the likely worst case and sets the result. The fallback is a native crypto module, which
  must then exist for both platforms.
- **The Deny path's 10 seconds.** A benchmark of the crypto does not show whether Deny from the
  Island finishes in time on cellular, from a cold start, with the phone locked. That is
  measured on real phones once the app connects through a relay.
- **A service to keep running.** The relay's uptime is now part of the product.
- **Abuse.** The relay forwards for anyone who registers a channel. The limits above bound it;
  they may need tightening once it is public.
- **New security-critical code.** The Noise code in the app is written here, over audited
  primitives; correct primitives do not make a correct protocol. It and the pairing flow need a
  dedicated review by someone other than their author before the hosted relay is used by anyone
  but the owner.

## Open items

- The default relay's hostname, which only the owner can choose.
