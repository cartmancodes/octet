# Use Octet from your phone

The Octet app on your phone lists the sessions running on your Mac, shows their
transcripts, sends prompts, answers approvals, changes mode and model, shows and
commits what a session changed, and starts or resumes a session. The phone reaches
your Mac through a relay, and everything between the two is encrypted end to end:
the relay only passes bytes along and cannot read them. There is no VPN to set up and
no account to make.

**The default relay is not deployed yet.** Octet's built-in relay address is a
placeholder (`wss://relay.octet.invalid`) that does not resolve, so no phone can reach a Mac
through it. Until the default relay is running, run a relay of your own
([Running the relay](relay.md)) and name it in `hub.json` (see [Moving to another
relay](#moving-to-another-relay)). The rest of this guide is how it works once a
relay is there.

The phone app is not in any app store: you build it from source and install it
yourself, as [its README](../apps/mobile/README.md#building-the-app-for-a-phone)
describes. That is the largest part of the setup.

## Phone app (hub)

The hub is a small program on the Mac that shares your running Octet sessions with
the phone app. It makes no listening port on your Mac: it dials out to the relay,
and only while a phone is paired or a pairing is open. You can still sit at the Mac
and join any session: a session started from the phone runs in tmux (`tmux attach -t
octet-NAME`), and one started at the desk shows up on the phone. The phone runs a few
safe desk commands (`/new`, `/session`, `/export`, `/help`).

You need Octet on the Mac, the app on the phone, and `tmux` if the phone should start
sessions. Nothing else.

The app is an Expo (React Native) project in [`apps/mobile`](../apps/mobile/README.md).
Its README covers building it for an iPhone or an Android phone, working on it, pairing,
and what to check when the app can't reach the Mac, says it is unpaired, or doesn't show a
session you started.

### Pair a phone in three steps

1. **Install the app** on the phone.
2. **Run `octet pair`** on the Mac, in a terminal.
3. **Scan the QR code** with the app (its **Pair** screen), check the code, and answer
   `y` at the Mac.

```sh
octet pair
```

`octet pair` does everything the Mac needs, in order:

1. If no hub is running, it says `Starting the hub` and installs the hub as a per-user
   service, exactly as `octet hub install` does (see [Keep the hub
   running](#keep-the-hub-running)), then waits up to 15 seconds for it.
2. It opens a pairing, which is what makes a hub with no paired phones dial the relay,
   and waits for the relay to accept the hub.
3. It prints a QR code and the same pairing link, `octet://pair/…`. The link works once
   and expires after 10 minutes. The app opens it from its own scanner, from the phone's
   camera, or pasted (**Enter code instead**).
4. If the terminal is too narrow for the QR code it says `Widen the terminal to show the QR
   code`; the link is still printed.
5. It says `tmux isn't installed: the phone can watch and answer, but can't start
   sessions` if `tmux` is not on your `PATH`.
6. When a phone uses the link, it asks you to check a code (next section).

It exits 0 when a phone was paired, and 1 when not. `--journal-dir PATH` names the hub's
data directory, as for `octet hub`.

Inside Octet, `/remote-control pair` works too, once the hub is running: it shows the
same QR code and asks the same question in the terminal UI, but it never starts or
installs the hub (the terminal UI changes no system settings). With no hub running it
says `The hub isn't running. Run "octet pair" in a terminal first.` Plain
`/remote-control` reports where things stand:

```
Hub: running
Relay: has the hub
Phones paired: 2
```

`Relay:` reads `has the hub`, `connecting`, or `off · nothing is paired and no pairing is
open`. In the terminal UI, `Y` answers yes to the question, and `N`, Enter, Esc or Ctrl+C
answer no; a key typed just as the question appears never answers it.

### The confirmation code

The pairing link alone would let anyone who got hold of it pair from anywhere on the
internet. So the Mac asks you, and the check is a six-digit code both screens show:

```
Phone shows 482 913? Pair "Shubh's iPhone" (ios)? [y/N]
```

- The phone shows the same digits under **Check this code on your Mac**. Compare them,
  and answer `y` only if they match.
- The hub picks the digits at random when it accepts a pairing, and sends them to that
  one phone inside the encrypted channel. The phone never works the code out for itself,
  and a hostile relay cannot choose it. A code on the phone's screen therefore means the
  hub is holding that phone's connection for your answer.
- **If the phone shows no code, answer no.** The prompt at the Mac with no code on your
  phone means someone else used the link first (a photographed or copied QR code): the Mac
  holds their phone, not yours. Answer `n` (or let the two
  minutes pass), then run `octet pair` again for a fresh link.
- The name and platform in the prompt are the phone's own claim, shown for convenience.
  `Replace` stands in for `Pair` when a phone of that name is already paired: see [Pairing
  a phone again](#pairing-a-phone-again).
- The link's secret is spent the moment the hub accepts a phone's `pair`, whatever you
  answer. One pairing waits at a time; a second phone meanwhile is refused and gets no
  code. If the held phone's connection closes, the question ends with it, and you start
  again with a new `octet pair`.
- No answer in two minutes refuses the phone. The app waits 130 seconds for the answer.
  A phone is stored only on `y`.

The command ends with `Paired "NAME"`, `Not paired`, `Pairing timed out`, `The phone
left`, or why the hub could not save it.

### What the relay sees

A relay, honest or not, sees each connection's address, the channel ids hubs and phones
name (a channel id is a hash of the hub's relay key, 64 hex digits), and the timing and
size of every message. It can drop or delay messages or cut a connection. It cannot read
or change a frame (authenticated encryption), pose as your Mac (it lacks the identity
key the phone pinned at pairing), pose as your phone (it lacks the phone's key), keep a
departed phone looking present (the hub pings inside the encrypted channel), or see a
pairing link: that travels by QR code and never touches the relay. The encryption is
`Noise_IK_25519_ChaChaPoly_SHA256`, with fresh keys for every connection. How to run a
relay, and what an operator can watch, is in [Running the relay](relay.md).

### Pairing a phone again

A phone that pairs again makes a new key, so the hub cannot tell it is the same phone;
its name decides. When you confirm a pairing under a name the hub already holds, the new
key replaces the old one in one step: the old key's connections close, and its push and
Live Activity tokens are dropped. The prompt says `Replace` so you can see it. This is how
a phone comes back after a relay move, a lost `paired`, or a wiped app, without leaving its
old key authorised. If you pair two different phones, give them different names.

### Moving to another relay

A phone keeps the relay address it paired through. The hub dials the default relay unless
`hub.json` (in the data directory, beside `devices.json`) names another:

```json
{
  "relay": "wss://relay.example.com"
}
```

It must be `wss://HOST` or `wss://HOST:PORT`, with no path, query or user name; anything
else is ignored with a line saying so, and the default is used. Restart the hub after
changing it. If the hub's relay changes (the default moves, or you add `"relay"`), phones
paired before can no longer reach it and must pair again; `octet pair` says so when it
sees paired devices that went through a different relay (`N phones are paired through
another relay and must pair again`). The default relay's hostname is meant to be
permanent. Debug builds of Octet (not release builds) also read `ws://HOST:PORT` from the
`OCTET_HUB_RELAY_URL` environment variable, for trying a phone against a relay on the
same network before one is hosted.

### Lost or finished with a phone

```sh
octet hub devices
octet hub revoke "My iPhone"
```

`devices` lists each phone as `NAME · PLATFORM · paired DATE · last seen AGO`. `revoke`
deletes the phone's stored key: a running hub drops its connections within two seconds,
and the phone is told it was unpaired the next time it connects, forgets the pairing, and
can only come back by pairing again. Revoking also removes its push and Live Activity
tokens. Run both with the same `--journal-dir` as the hub if you gave it one. Neither
reads `hub.json`, so a mistake there never blocks a revoke. A relative `--journal-dir` is
resolved to an absolute path when Octet starts, so the hub, the sessions at your desk and
the sessions a phone starts use the same directory, whatever directory each runs in.

**After the last phone is revoked**, the hub has nothing paired and no pairing open, so it
stops dialling the relay. That phone then sees only "Can't reach your Mac" and never
learns it was revoked, because there is no hub on the relay to tell it. Remove the Mac in
the app yourself (**Settings › Machines › Unpair**). With other phones still paired, the
hub keeps dialling, and a revoked phone that connects is told it is unpaired.

### Resetting the hub's identity

The hub's identity is two keys in `hub.key` in the data directory (mode 0600, made on
first use): an X25519 key that phones pin at pairing, and an Ed25519 key whose hash is the
channel id. To reset it, delete `hub.key` and restart the hub. The hub makes
new keys, and because every paired phone pinned the old identity and could never connect
again, it empties `devices.json` first, with every push and Live Activity token in it, so
no old phone keeps getting notifications. Pair each phone again afterwards. A `hub.key`
that exists but cannot be read stops the hub with an error naming the file; it is never
replaced silently.

### Set it up in this order

1. [Pair a phone](#pair-a-phone-in-three-steps). That is all a phone needs.
2. Optionally, [keep the hub running](#keep-the-hub-running) at login (`octet pair` does
   this for you), [push notifications](#push-notifications), the [Live
   Activity](#live-activity-dynamic-island) (iPhone, with your own APNs key), and [more
   Macs](#more-than-one-mac).

After those come [what the phone is sent](#what-the-phone-is-sent), [changes and
commits](#changes-and-commits-from-the-phone), [keeping a session
private](#keep-a-session-off-the-phone) and the [hub security
notes](#hub-security-notes).

### Start the hub

`octet pair` starts the hub for you. To run it yourself, in its own terminal or tmux
window:

```sh
octet hub
```

It says `Octet hub running; phones reach it through the relay once one is paired`. Running
Octet sessions carry on if the hub stops, and the phone just can't reach them. A hub with
no paired phone and no open pairing makes no outbound connection at all. If the relay is
down, the hub retries from half a second, doubling to 30 seconds, with jitter, and sessions
at your desk are unaffected. To have it start at login and come back if it stops, see [Keep
the hub running](#keep-the-hub-running).

To see what every session is doing, in another terminal:

```sh
octet hub watch
```

It prints one line per session event (tool calls, approvals, finished turns) until you
press Ctrl+C. It shows the same thing the phone would see.

### Keep the hub running

To have the hub start at login and come back if it stops, install it as a
per-user service:

```sh
octet hub install
```

On macOS that writes a launchd agent, `~/Library/LaunchAgents/dev.octet.hub.plist`.
On Linux it writes a systemd user unit, `~/.config/systemd/user/octet-hub.service`,
and enables it. Either way the service runs the `octet` you ran this from, as
`octet hub --journal-dir DIR`, where DIR is the data directory this command resolved
(from `--journal-dir`, `$XDG_DATA_HOME`, or `~/.local/share`), so the service and its log
agree even though it doesn't see your shell's environment. Its `PATH` starts
with the directories where `tmux`, `claude`, `codex`, `opencode` and `agy` were
found on your shell's `PATH` when you ran the command, then the usual system directories, so
the hub can start sessions (and the agents in them) from a service's bare
environment, even after a login with no tmux running. `octet pair` installs this service
for you when no hub is running.

- **Logs.** On macOS the hub's output goes to `hub.log` in the data directory
  (`~/.local/share/octet/rust-preview/hub.log` unless you chose another); read it with
  `tail -f`. On Linux use `journalctl --user -u octet-hub.service -f`.
- **hub.log grows.** Nothing rotates it. To empty it, run
  `: > ~/.local/share/octet/rust-preview/hub.log` (your data directory, if you
  chose another); the running hub carries on writing to it.
- **See the file first.** `octet hub install --print` writes the file to the
  terminal and changes nothing.
- **Move or upgrade `octet`.** The service holds the program's path, so run
  `octet hub install` again after moving or reinstalling it; on Linux that also
  restarts the running service. A file already at the service file's path that
  isn't Octet's is never replaced.
- **Move or install tmux, Claude, Codex, opencode or Antigravity.** Install records where
  `tmux`, `claude`, `codex`, `opencode` and `agy` were found; re-run
  `octet hub install` after moving or installing them, or the hub won't find
  them.
- **Installed before opencode support.** A hub installed by an `octet` that
  predates opencode support has a `PATH` without opencode's directory
  (`~/.opencode/bin` by default), so its sessions can't start opencode. Re-run
  `octet hub install` from a shell where `opencode` is on the `PATH`.
- **Installed before Antigravity support.** A hub installed by an `octet` that
  predates Antigravity support has a `PATH` without the directory `agy` is in, if
  that is not one of the usual system directories, so its sessions can't start
  Antigravity. Re-run `octet hub install` from a
  shell where `agy` is on the `PATH`.
- **Remove it.** `octet hub uninstall` stops the service and deletes the file.
  It says "Not installed" if there is nothing to remove.
- **Restarts.** launchd restarts the hub whenever it stops; systemd restarts it
  five seconds after it fails, so a failure at boot doesn't use up systemd's
  start limit.
- **Sessions outlive the service.** Stopping, restarting or reinstalling the
  service stops the hub alone. The tmux server a phone's `start` forked keeps
  running, with every session in it, yours too. The systemd unit sets
  `KillMode=process` and the launchd agent sets `AbandonProcessGroup`.
- **Linux without a login session.** A systemd user service stops when you log
  out unless lingering is on: `loginctl enable-linger "$USER"`.

### Push notifications

When an agent needs your approval, hits an error, or finishes a turn, the hub
can tell a phone that isn't looking at the app. The phone turns this on itself
(Settings in the app, or the step after pairing; [the app's
README](../apps/mobile/README.md#notifications) covers the one-time setup with
your own EAS and Apple or Firebase accounts), and nothing is sent until it does.

- **What is sent.** Exactly this and nothing else: the title `Octet`, the body
  `Approval waiting`, `Session error` or `Turn finished`, and `data` holding
  the hub's opaque session id so a tap opens that session, and `mac`: the
  hub's channel id (64 lowercase hex digits, which the phone already holds
  from pairing), which it uses to tell which Mac a tap is for. The title and
  body never name it. Then the fixed delivery fields
  Expo needs (`to`, which is the phone's push token, and `sound`, `priority`
  and `channelId`, which are constants: `approvals`, `errors` or `turns`). No
  prompt, command, file name, workspace, model, error or reply text ever goes in
  a push, so the push services never see what you are working on.
- **Which kinds.** Each phone has three switches, kept by the hub per phone:
  approvals (`Approval waiting`), errors (`Session error`, for an `error` record
  in a session's journal) and finished turns (`Turn finished`). A phone paired
  now starts with approvals and errors on and finished turns off. A phone paired
  before the switches existed keeps getting all three until it changes one. The
  app sends its switches with its push token (`register_push`); an older app
  that sends none keeps what the hub has.
- **Where it goes.** The hub posts each push to Expo's push service, `https://exp.host/--/api/v2/push/send`,
  which hands it to Apple or Google. These requests, and the Live Activity's
  to Apple once you add an APNs key ([below](#live-activity-dynamic-island)),
  are the only things the hub sends outside your Mac, besides the encrypted
  traffic through the relay. A push
  carries the phone's Expo push token, which the hub keeps in `devices.json`
  (readable by you alone) and never writes to logs, journals, the audit file
  or a command line. Expo, Apple and Google see that a push for that token
  happened, and when.
- **Which phones.** A phone gets a push only if it turned notifications on and
  has no open connection to the hub (a short background connection the app
  makes for the Live Activity doesn't count). The app closes its connection when it goes
  to the background, so a phone you are holding with the app open gets
  nothing. An approval that arrived while you were looking and is still
  waiting when the app goes to the background is pushed then, once (within the
  same limits).
- **How often.** One push per approval, however often it is shown again; one per
  finished turn; and errors, however many, once per collecting window. Pushes to
  a phone collect for about two seconds and go out as one, ranked approval, then
  error, then finished turn, and a phone gets at most six in any minute, so a
  session that keeps failing can't flood it. A phone Expo reports as gone has
  its token cleared.
- **curl.** The hub sends each push by running `curl` from its `PATH`, with
  `-q` so it ignores any `~/.curlrc`. Without
  `curl` every push fails (the hub logs `Push failed` and carries on). macOS
  and most Linux systems have it in `/usr/bin`; the service `octet hub install`
  writes carries a `PATH` that includes `/usr/bin`, so it finds it.
- **Turn it off.** Switch each kind off in the app's Settings; with all three
  off, notifications are off and the hub forgets the token at once. Unpairing
  the phone, or `octet hub revoke
  "NAME"` on the Mac, removes its token as well. Stopping the hub stops
  every push.

### Live Activity (Dynamic Island)

On an iPhone with iOS 17.2 or later, a Live Activity in the Dynamic Island and on the Lock Screen
shows whether a session is running, waiting for approval, or done, and for how long. The app runs
on iOS 16.4 and later, but below 17.2 it has no Live Activity.

There is one activity, with two drivers. While the app is open (on any screen), the app keeps it up
to date itself. While it isn't, the hub starts, updates and ends that same activity through Apple's
push service (APNs), and for that it needs your APNs key. When you come back to the app, it takes
the activity over where it is, whoever started it. Without a key the activity shows only while the
app is open, and the hub sends Apple nothing.

**One Mac drives it while you are away.** With several Macs paired, the phone gives its Live
Activity tokens to one of them only: the Mac of the session the activity was leading when the app
was last in front (the oldest waiting approval, else the newest running turn, else the last to
finish), or, before any, the first Mac you paired. Every other Mac is told to forget them
(`clear_activity`), so no two hubs ever start, update or end the same activity. While you are away
the Island follows that one Mac; the other Macs still send their push notifications. The choice moves
only while the app is in front, and the phone remembers it. A Mac that is out of reach when the
choice moves away from it keeps the tokens until the phone can tell it: on its next connection, and
again each time you come back to the app. Until then both Macs can drive the activity.

**Set up APNs** (once, with your own Apple developer account; nothing here makes one):

1. In the Apple developer site, under **Certificates, Identifiers & Profiles › Keys**, make a key
   with **Apple Push Notifications service (APNs)** on, and download its `AuthKey_<KEY ID>.p8`
   (Apple lets you download it only once). Note the **Key ID** and your **Team ID**: 10 upper-case
   letters and digits each.
2. Put the key on the Mac in a file only you can read, owned by the user the hub runs as, and not a
   symlink:

   ```sh
   mkdir -p ~/.config/octet && mv ~/Downloads/AuthKey_KEY1234567.p8 ~/.config/octet/
   chmod 600 ~/.config/octet/AuthKey_KEY1234567.p8
   ```

3. Name it in `hub.json` in the data directory (beside `devices.json`; see the end of [Hub
   security notes](#hub-security-notes)), with the absolute path:

   ```json
   {
     "apns": {
       "team_id": "ABCDE12345",
       "key_id": "KEY1234567",
       "key_path": "/Users/you/.config/octet/AuthKey_KEY1234567.p8",
       "sandbox": false
     }
   }
   ```

   `sandbox` picks Apple's host: `true` sends to `api.sandbox.push.apple.com`, for a build signed for
   development (from Xcode, or an EAS development build); `false` sends to `api.push.apple.com`, for
   an EAS `preview` (ad hoc) build, TestFlight or the App Store. The wrong one gets no activity.
4. Restart the hub. If the key is missing, is a symlink, isn't mode `0600`, belongs to another
   user, or isn't a PKCS#8 P-256 key, or the block is malformed, Live Activity pushes are off and
   the hub logs one line saying why (for example `APNs key must be 0600`); everything else works as
   before.

**What is sent.** Exactly this, and only to a phone that turned the Live Activity on:

```json
{"aps": {"timestamp": 1790000101, "event": "start",
         "content-state": {"kind": "approval", "session": "4821-1790000000",
                           "mac": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                           "since": 1790000059, "approvals": 2},
         "stale-date": 1790001001,
         "attributes-type": "OctetActivity", "attributes": {},
         "alert": {"title": "Octet", "body": "Approval waiting"}}}
```

- `kind` is `running`, `approval` or `done`; `session` is the hub's own opaque id of the session
  that leads (the one with the oldest waiting approval, else the newest running turn, else the last
  to finish), or empty; `mac` is the hub's channel id; `since` is
  when that session's state began; `approvals` counts the approvals waiting.
- `stale-date` is 15 minutes after the send, on every start and update: an activity the hub can't
  keep up to date shows as stale after that, rather than looking current. Its content dims and a
  muted **Stale** shows under it, on the Lock Screen and in the Island.
- `attributes-type`, `attributes` and `alert` go on a start only (its alert body is `Approval
  waiting` or `Session running`); an end adds `dismissal-date` and has no `stale-date`.
- No session name, workspace, command, file name, prompt or reply ever goes in it, so Apple never
  sees what you are working on. The widget reads names and commands from the phone itself (see
  below).
- The request goes to Apple with the headers `apns-topic: dev.octet.mobile.push-type.liveactivity`,
  `apns-push-type: liveactivity`, `apns-priority` (10 for a start and while an approval waits, 5
  otherwise) and a bearer token: an ES256 JWT signed with your key, made again every 50 minutes.
  The hub runs `curl -q --http2 -K -` and gives it the address, the headers and the body on standard
  input, so neither the key, the JWT nor the phone's token is ever on a command line, in a log, a
  journal or the audit file.

**When.** The phone is in front while the app has any connection open to the hub, on any screen; a
connection the app opens only from the background (to hand a token over, or to send a Deny) says so
in its hello (`background: true`) and doesn't count. While the phone is away, the driving Mac's hub:

- **starts** the activity when an approval waits or a turn starts and it holds no token of a live
  activity for the phone;
- **updates** it through its registered token when what it shows changes, at most once every 2
  seconds;
- **ends** it 5 minutes after every session went idle, and then forgets that token for good.

Coming back to the app ends nothing: the app takes the same activity over. A token counts as live
for 12 hours at most from when the phone first registered it (ActivityKit ends an activity within
that); the same token registered again, at each launch, doesn't restart the count. While an activity
the hub started still waits for its token, the hub starts no other for up to 8 hours. That hold
ends early when you open the app and the Mac still holds no live activity token for the phone (the
activity it started never handed over, and you dismissed it or it ended): the next approval while
you are away starts one again. Activity sends share
the pushes' limit of six a minute per phone, at most three of them, so approval pushes always have
room. Apple answering that a token is gone (410) drops that token, and an approval still waiting
then starts a new activity; any other failure is logged, without the token, and not retried in a
loop.

**Two alerts for one approval.** An approval that starts the activity while you are away brings
two alerts: the push notification (`Approval waiting`, with **An action needs approval** on) and the
activity's own start alert, which Apple requires to start one by push. That is expected.

**Tokens.** The app gives the driving Mac its ActivityKit tokens (`register_activity`) over its own
connection, whenever they change: the push-to-start token, and the running activity's own token.
Each is 1 to 1024 lowercase hex digits; Apple doesn't fix the length (iOS 26.5 issues 256), and the
app and the hub refuse anything else.
When a new activity's token comes while the app is in the background (an activity the hub just
started), the app hands it over at once: it opens a short background connection to that Mac, which
registers the token and closes once the Mac answers or after 10 seconds. It does this too when iOS
launches the closed app in the background for the token, once the app has read its settings and
pairings. When a new activity appears, the app ends the ones it saw before it, so they don't stack;
one it already saw, or one already over, ends nothing, so one activity always stays. A registration
or clear the Mac turns away for now (busy, rate-limited, not saved, or timed out) is tried once more
5 seconds later; any other is tried again on the next change, connection or return to the app. The
hand-over needs two things:

- **Time in the background.** iOS must give the app time when the activity's token arrives.
- **An unlocked phone.** Each Mac's private key is kept in the keychain readable only while the
  phone is unlocked (`WHEN_UNLOCKED_THIS_DEVICE_ONLY`), so on a locked phone the app can't
  reach any Mac.

**What a locked phone costs.** Without the hand-over (the phone locked, which is the usual Lock
Screen case, or no background time), the Mac never gets the token of the activity it started, so it
can neither update nor end that activity until you unlock and open the app. The activity keeps
showing what it started with (`Approval waiting`, say, after you answered at the desk), it shows as
stale (dimmed, marked **Stale**) 15 minutes after the start, and the Mac starts no second one for up
to 8 hours while the first still waits for its token. Once you open the app, the app takes the
activity over and brings it up to date. This follows from keeping the Macs' keys unreadable
while the phone is locked.

The hub keeps the tokens per phone in `devices.json`, records `activity registered` (or `activity
cleared`) with the phone's name in `hub-audit.jsonl`, never the token, and drops them on `octet hub
revoke` and when you unpair that Mac in the app.

**Deny and Allow on the Island.** While an approval waits, the expanded activity shows it with
**Deny** and **Allow**.

- **Deny** answers that approval no, to the Mac that owns it, without opening the app and without
  Face ID, since it allows nothing. With the app in front it goes over that Mac's connection. With
  the app in the background, it goes at once over a short background connection to that Mac (closed
  on the answer, or after 10 seconds). The button waits, at most 9 seconds, for the app to say that
  send is done, and the app asks iOS for the background time to finish it (at most 10 seconds); the
  same holds for a token the app hands over in the background. On a locked phone that connection can't be made (the keychain
  again), so the Deny waits until you unlock and open the app. A Deny that couldn't go, or for a Mac
  out of reach, is sent on that Mac's next connection, within 10 minutes; after that it is dropped.
  Each Deny is sent once. A Deny that names no paired Mac sends nothing.
- **Allow** only opens the app (`octet://approve/<channel id>/<session>/<approval>`), at that approval in
  its session, where Allow asks for Face ID and waits out its short arm as always. The link allows
  nothing by itself. It opens the session only while the approval it names is the one at the front
  of that session's waiting approvals (the one its Permission card shows). A link for a Mac that
  isn't paired, a session that ended, an approval that was already answered or that waits behind
  another, or one that is malformed, opens the Inbox and sends nothing. It is judged by the Mac's
  first list after the app comes back (or the link is up again), never the one from before, which
  may hold an approval answered since; the Inbox shows while it waits, up to 8 seconds, then stays.

**On the phone.** The widget shows a session's name (at most 80 characters) and the command it
asks to run (at most 200) from the app's shared storage on the phone (the App Group
`group.dev.octet.mobile`), which the app writes while it runs, for each waiting approval (at most
64), and clears when you unpair that Mac or turn the Live Activity off. These show on the Lock
Screen; both are marked privacy-sensitive, so iOS hides them on a locked phone when you have it set
to. Nothing of them leaves the phone.

**Turn it off.** In the app, **Settings › Live Activity**: the activity ends, every Mac is told to
forget the phone's tokens (`clear_activity`; a Mac out of reach is told on its next connection), and
the widget's names are cleared. Turning Live Activities off for Octet in the phone's own settings
does the same the next time you open the app (and the switch goes from Settings); turning them back
on registers again. To stop every phone's activity pushes, remove the `apns` block from `hub.json`
and restart the hub.

**Not yet tested on a phone.** The Swift code (the app's Live Activity module and the widget) has
been checked here only as source (parsed, and type-checked against the Mac Catalyst SDK's
ActivityKit and WidgetKit), never built for iOS. The first device build (with EAS, on your
account) is its first real check; see the app's [device pass](../apps/mobile/README.md#device-pass).

### More than one Mac

The app pairs with up to 4 Macs, each running its own hub. Pair the first as
above; for each next one, open **Settings › Machines › + Pair another
machine** and scan the code `octet pair` shows on that Mac. A Mac is known by its channel
id, not its address: every Mac behind the same relay shares the relay's hostname. Pairing
a Mac the phone already has (the same channel id) replaces its pairing, once the hub has
answered `paired`, and keeps its name, its sessions and their cursors. A
5th is refused (`Unpair a Mac first (4 at most)`), and the button is off at 4
(`4 Macs is the most`).

- **All of them at once.** The app keeps one connection per Mac while it is
  open, and closes them all in the background. The Inbox and the drawer show
  every Mac's sessions together, each card named by its Mac; a Mac that can't
  be reached shows "asleep" and only its sessions grey out. Every command goes
  to the Mac that owns its session, so two Macs whose sessions have the same id
  never mix.
- **Settings › Machines** lists each Mac with its relay address and its own state
  (`online`, `connecting…`, `asleep`, or `offline` for a pairing whose
  key the phone can't read right now). **Rename** gives it a name of
  1 to 40 characters, for this phone only. **Unpair** asks "Unpair NAME? Its
  sessions leave this phone." and drops only that Mac: its sessions, drafts
  and approvals leave the phone, and a screen showing one of them goes back to
  the Inbox. The other Macs are untouched.
- **Notifications** are one choice for every Mac. Each push names its Mac by
  channel id (`mac`, above), and a tap opens that Mac's session. A tap
  goes to the Inbox when the push names a channel id none of your pairings has,
  when it names none, or when that session has ended, even with one Mac paired.
  A notification or Live Activity left over from before the relay, which names
  a hostname, names no Mac: tapping it opens the app and nothing more.
- **Unpairing a Mac that can't be reached.** Unpairing tells that Mac
  "notifications off" when it can, and to forget the phone's Live Activity
  tokens (`clear_activity`), so it starts and updates no more activities on
  the phone. A Mac that is asleep or away when you
  unpair it doesn't hear that, and since the phone's push token is the same
  for all your Macs, the phone can't make the token stop working without
  stopping every Mac's pushes. That Mac keeps pushing until you run
  `octet hub revoke "NAME"` on it (`octet hub devices` lists the names), and
  keeps its Live Activity tokens until then too. The
  app ignores those pushes' sessions (their channel id matches no pairing, so a tap
  only opens the Inbox), but they still show. Unpairing the last Mac also
  turns the phone's push registration off when that Mac doesn't answer, or
  when the phone had no connection to it at all (its pairing key
  couldn't be read).

### What the phone is sent

A paired phone is sent the list of live sessions, one row each: what it is
doing, its workspace, mode and model, how many approvals wait (with a short
gist of the first), its goal, the NOW line, a usage line, and whether its turn
ended on a question. It is sent the transcript, state and catalog only of the
sessions it opens.

- **When.** The list goes when a row changes, at most four times a second: a
  change after a quiet spell goes at once, and the changes after it within a
  quarter of a second, together. A row can trail its session by up to 250 ms
  (an open session's own state comes at once). A phone gets the whole list as
  soon as it says hello.
- **How much.** At most 200 sessions, the most recently active, in at most
  96 KiB. Past that the rows drop what can wait, least recently active first:
  the approval's gist, then the usage line, then the NOW line, then the goal.
  Then the least recently active rows go. The count of approvals waiting always
  stays, and the phone keeps the gist it already had while that count says the
  approval still waits.
- **One copy.** The hub makes each list once and shares it with every phone. A
  phone that falls behind gets the newest list, not every one it missed.

### Changes and commits from the phone

A session's **Changes** screen shows what its workspace changed since the last
commit. Open it from **view ›** beside a tool entry that edited a file, from
**Changes ›** in Usage, or from Session info. It shows:

- each changed file with its status, `+added −removed` lines and a small bar;
  tap one for its diff;
- **Checks**, the last test command the session ran (`cargo test`, `npm test`,
  `pytest`, `go test`, `jest`, `vitest` and the like) with ✓ or ✗;
- **Branch**, the branch and how far it is ahead of its upstream.

What the phone can do with git, and nothing more:

- **Read** the list of changed files (at most 500, then "N more files on the
  Mac"; a new file's lines are counted from its first 1 MiB, and past 8 MiB
  of new files in all, or 10 seconds of counting, the rest show `+0 −0`) and
  the diff of a listed file (at most 64 KiB or 2,000 lines; the
  rest stays on the Mac). The screen asks for at most two diffs at a time,
  as the hub runs two git calls at once, and asks again, up to three times,
  for one the Mac was too busy for.
- **Commit everything** in the workspace with a message you write (at most
  2,000 characters). The message starts as the session's first prompt, on one
  line of at most 72 characters. **Commit** asks for Face ID every time,
  whatever the **Face ID to allow** setting says. The Mac runs `git add -A`
  and `git commit -F -` with the message on standard input, limited to the
  workspace's own files, untracked ones too: the sheet says how many files
  that is from the last listing ("Commits 3 files, including 1 new", where
  new means untracked or added, or "about" when the Mac had more than it
  listed). A toast names the new
  commit's short hash and the branch.

The phone can't push, open a pull request, amend, choose which files to
commit, or run any other git command. **Open PR on the desk** only shows
`gh pr create` to run at the Mac, and sends nothing.

The hub refuses a commit:

- while a turn runs or an approval waits (`Finish or cancel the current turn
  first`); the app turns **Commit** off while an approval waits or the Mac
  can't be reached;
- during a merge, rebase, cherry-pick or revert (`Finish the merge at the desk
  first`);
- when nothing changed (`Nothing to commit`) or the message is empty
  (`Write a commit message`);
- while the session's terminal UI has no link to the hub, or hasn't yet told
  it what it is doing (`The session is not connected`);
- while two other git calls run (`Git is busy; try again`).

The repository's commit hooks run, as they do at the desk, however much they
print. If a hook fails, the phone shows the end of git's own words, up to
2,000 characters, where a hook puts its summary, and the files stay staged. A
git call, hooks included, is stopped after 10 seconds (`Git took too long; try
again`). If the repository signs commits (`commit.gpgsign`), the signing
program runs on the Mac: a passphrase or Touch ID prompt there must be
answered within those 10 seconds, or the commit fails.

### Keep a session off the phone

Each Octet session shares itself with the hub through a private socket in
`run/` beside its journals, and a file saying which engine, workspace and mode
it uses. To keep one session private, start it with:

```sh
octet --no-remote
```

It then creates no socket and no file, and the hub and the phone never see it.

### Hub security notes

- **What Tailscale did that nothing does now.** Before the relay, every connection to
  the hub, not only pairing, had to come from a device signed in to your tailnet. That
  check is gone. A phone's stored key is now the whole of its authority: someone who
  extracts it can connect from anywhere until you revoke the device, and removing a
  device from a tailnet no longer cuts it off. The key is kept in the phone's keychain
  (this device only, readable while the phone is unlocked). The typed confirmation for
  `full-access` and the biometric check are the app's own; a program written to hold a
  stolen key could skip the second and supply the first. That is what dropping Tailscale
  gives up. The pairing confirmation at the Mac (above) is what restores a second check
  when a phone pairs.
- **The hub listens on no port.** It dials out to the relay, and only while a phone is
  paired or a pairing is open. Nothing on your network, or on the internet, can connect
  to it.
- **Every connection is Noise end to end.** The phone pins the hub's identity key from
  the pairing link, and the hub knows each paired phone by its own key. A connection's
  first message must be exactly 96 bytes (then 48), carries no payload, and uses fresh
  keys; a failed decryption closes the connection, and there is no unencrypted fallback.
  A known phone key sends `hello` and is authenticated as that device. An unknown key that
  sends `hello` is told `unpaired` and closed, so a revoked phone learns it is revoked. An
  unknown key that sends `pair` with a live link secret starts a pairing; anything else is
  closed. Each connection has 10 seconds from the relay's `connect` for all of this.
- **A compromised or malicious relay** sees only what [What the relay
  sees](#what-the-relay-sees) lists. It can cut a connection, or drop or delay messages,
  and that is all. If the relay is down, no phone can connect; the desk is unaffected.
- **Anyone on the internet who learns your channel id** can send bytes to the hub's
  handshake. The hub does not trust the relay's limits: it takes a place in its own room
  before it dials (64 connections in all, 32 of them not yet authenticated), ignores a
  relay `connect` when none is free, takes at most 120 a minute, and ignores a connection
  id that is malformed or already seen. A phone waiting for your answer to a pairing counts
  as one unauthenticated connection. Such a person can also fill a channel's 16 places at
  the relay for 10 seconds at a time, keeping its phones out; the relay's per-address limit
  is the only brake on that.
- **A photographed or copied pairing link** works once, expires in 10 minutes, and still
  needs your yes at the Mac. Whoever uses it first is the one the Mac holds: the Mac shows
  a code, and your own phone shows no code, so you answer no ([The confirmation
  code](#the-confirmation-code)). A guess at the six digits is one in a million, once.
- **Forward secrecy** covers what was sent, not who sent it. Every connection uses fresh
  keys, discarded when it ends, so recording traffic and later stealing a long-term key does
  not decrypt the frames. Stealing the hub's key does reveal which phone key opened each
  recorded connection, because this pattern sends the phone's key encrypted to the hub's.
- **The code is new.** The Noise code in the app and the hub, and the pairing flow, have
  had no review by anyone but their author yet. Treat the hosted relay as for the owner's
  own use until they have.
- Every device command is rate limited to 60 a minute per device, and the hub checks that
  the device is still paired on every frame. The hub also sends an encrypted ping every 20
  seconds and closes a connection that has not answered the last one, so a relay cannot
  keep a departed phone looking present, or hold a command back for longer than that.
- A lost or stolen phone: run `octet hub revoke NAME` on the Mac, as above. That cuts it
  off at once; nothing else needs doing.
- The hub won't start if `run/` in the data directory is a symlink, isn't a
  directory, or belongs to another user. It names the path. If `run/` is
  readable by others, the hub makes it `0700`.
- Every action from a phone (prompt, answer, mode, start and so on) is
  recorded with the phone's name in `hub-audit.jsonl`, without the prompt
  text. The desk also shows a note naming the phone. A commit is recorded as
  `commit` with the phone's name and the session, never the message, and
  `commit failed` means git refused it after `git add` ran; the desk shows no
  note for a commit. Listing changes and reading diffs aren't recorded.
  Pairing (`paired`) and `octet hub revoke` (`revoked`) are recorded too, with
  the phone's name and never a key or pairing code.
- A command that was waiting when a session's link dropped is refused with
  `Lost the link to that session; check it before sending again`. The command
  may have run, so look before you send it again. `The session ended` means
  the session is gone.
- `full-access` from a phone needs the typed confirmation `full access`, which
  the hub checks itself, and the desk shows `⚠ Full access from NAME`. The
  phone app adds a Face ID or fingerprint check of its own.
- Allowing an approval from the phone allows that one request, once. The
  phone can't add permission rules, and a denied or expired approval stays
  denied.
- The phone can start sessions only in workspaces that appear in your recent
  sessions or in the `workspaces` list of `hub.json` in the data directory,
  and at most eight phone-started sessions run at once.
- The phone cannot trust a workspace. Antigravity loads a workspace's
  `.agents` configuration (hooks, MCP servers, plugins) only once you trust it
  with `/trust` in the terminal interface, and a session the phone starts
  passes no `--trust-workspace-config`; in a workspace whose configuration is
  not trusted yet, it cannot start and says why. A desk session started with
  `--trust-workspace-config` trusts only the configuration it found when it
  started. While the configuration differs from that, every prompt is
  refused, whoever sends it, and so is every action that reconnects: the
  phone's model change, `new`, and a mode change to or from `full-access`
  (a switch between `ask`, `accept-edits` and `auto` is live and does not
  reconnect). Trust the workspace at the desk first.
- The phone runs exactly these desk commands and no others: `/model`,
  `/mode`, `/new`, `/session`, `/help` and `/export`. The last four go as one
  `command` request naming one of `new`, `session`, `help` or `export`, with
  no argument, so nothing from the phone reaches a shell or a path. Like every
  phone request it is refused before the phone says hello, counted against the
  same per-phone rate limit, and audited with the phone's name (`command new`
  and so on). The desk runs its own handler and answers with its own words:
  - `/new` is refused while a turn runs, while the desk's external editor is
    open or a `!` command runs there, or while the desk has an unsent draft;
    otherwise it starts a fresh conversation, as at the desk.
  - `/session` and `/help` only read. `/help` lists the phone's commands.
  - `/export` writes the journal to the desk's default path, the workspace and
    the journal's file name, exactly as `/export` with no argument does at the
    desk. It is refused during a turn or while another desk job runs. It runs
    as the desk's own export job: the desk shows `Exporting the journal…` and
    keeps working, and the phone's answer comes when the copy is done.
- Git from the phone runs in the session's own workspace, which the hub takes
  from the session's registration, never from the phone. The hub fixes every
  argument: the phone names only the session, a path from the hub's own last
  listing for that phone (any other is refused with `Ask for the changes
  first`), or a commit message, which goes on standard input. The hub removes
  every `GIT_*` variable from git's environment, turns off fsmonitor, and runs
  diffs with no external diff or textconv program. A clean filter that the
  repository's `.gitattributes` names still runs, as it does at the desk.
- The phone can't run any other slash command, `!` shell commands or `/goal`,
  edit files outside a session, push or open a pull request, or see sessions
  started with `--no-remote`.
- What leaves the Mac, besides the relay's encrypted traffic: pushes to Expo
  (`exp.host`), for phones that turned notifications on, and, with an `apns`
  block in `hub.json`, Live Activity pushes straight to Apple
  (`api.push.apple.com`, or `api.sandbox.push.apple.com` with `sandbox`), for
  phones that turned the Live Activity on. Both carry no prompt, command, file
  name, workspace, model or reply text (see [what is
  sent](#push-notifications) and [the activity's
  payload](#live-activity-dynamic-island)). Only a phone's own
  `register_push`, `register_activity` and `clear_activity` change who
  receives them, besides unpairing and `octet hub revoke`. Each is recorded in
  `hub-audit.jsonl` (`push on`, `push off`, `activity registered`, `activity
  cleared`).
- Pairing codes and secrets, private keys, push tokens, ActivityKit tokens, the APNs key
  and its JWT are never written to logs or journals.

The hub's files (`devices.json`, `hub.json`, `hub.key`, `hub-audit.jsonl`) and the
`run/` directory live beside the journals: `$XDG_DATA_HOME/octet/rust-preview`,
or `~/.local/share/octet/rust-preview`. `--journal-dir PATH` chooses another.

## Use the terminal UI over SSH

The terminal UI itself works over any SSH connection you already have, from any phone
terminal app, as long as Octet runs inside tmux so it keeps going while you are away. That
can be a VPN, or Tailscale, or a plain SSH server you already trust: Octet doesn't set it
up, check it, or name a network for it. Start Octet with `tmux new -A -s octet "octet …"`
and reattach with the same line. Anyone who can log in over SSH can run anything as your
user, including switching Octet to `full-access`, so keep login to your own key, and never
forward port 22 on your router. The notes on tmux, mosh and phone keyboards that this
route needs are in [the evaluation](rust/remote-control-ssh.md).

## Turn it off

- Revoke each phone with `octet hub revoke "NAME"` (`octet hub devices` lists them). With
  none left and no pairing open, the hub stops dialling the relay.
- Stop the hub with Ctrl+C, or `octet hub uninstall` if you installed it as a service.
- Remove the Mac in the app (**Settings › Machines › Unpair**) so the phone forgets it.
- To stop one session reaching the phone, start it with `octet --no-remote`.
