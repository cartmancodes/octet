# Octet for your phone

An Expo app (React Native, SDK 57) that lists the Octet sessions running on your Mac, shows each
transcript, sends prompts, answers approvals, changes mode and model, shows and commits what a
session changed, and starts or resumes a session. It talks to `octet hub` on your Mac over your tailnet; it holds no vendor credentials and
no SSH keys. Why the hub is built the way it is, and how to set up the Mac side, is in
[docs/remote-control.md](../../docs/remote-control.md#phone-app-hub).

## What you need

- Octet on the Mac with the hub running (`octet hub`) and published on your tailnet (`tailscale
  serve --bg 47823`; you run that yourself).
- The Tailscale app on the phone, signed in to the same tailnet.
- Node 22 and npm, to work on the app.

## Local development

```sh
cd apps/mobile
npm ci
npx expo start
```

Expo Go cannot run this app: it uses the camera, secure storage and biometrics through native
modules that need a build of its own (below). `npm start` serves the bundle to a development
build. Never pair a development build with the Mac you use every day: see
[Which build](#which-build).

| Script | What it does |
| --- | --- |
| `npm run typecheck` | `tsc --noEmit`, strict |
| `npm run lint` | `expo lint` |
| `npm test` | Jest; hermetic (fake sockets, no network, no Mac) |
| `npm run export:check` | Bundles for iOS and Android, which proves the app compiles with no missing native module |
| `npm run contract` | Runs the real `HubClient` against the real `octet hub` (see below) |

`make mobile-check` from the repository root runs typecheck, lint and tests, as CI does.

### The contract test

`npm run contract` (or `make mobile-contract`) starts `target/debug/octet hub` on a free port with
a fake session (a Unix socket, a registration and a journal in a temp directory), pairs, and drives
`HubClient` through hello, sessions, subscribe, replay, a live event, a prompt, an approval
answer, a dropped connection with a cursor (no duplicates), a ping, a desk command, the
workspace's changes, a diff of a listed file (and the refusal of one not listed), a commit and
then "Nothing to commit", notifications on (a push to a local fake, never to Expo, with the exact
content-free body), notifications off, and a revoke. The fake session's workspace is a git
repository the test makes in its temp directory, with its own `user.name`, `user.email` and
`commit.gpgsign=false` (the hub takes every `GIT_*` variable out of git's environment), so the
test needs `git` on the `PATH`. It builds nothing:
run `scripts/rust-env.sh cargo build -p octet` first. Without that binary it skips. It never
touches Tailscale: the hub is given a stand-in `tailscale` on its `PATH`, and the test's socket
adds the `Tailscale-User-Login` header that `tailscale serve` adds in real life. The app never
sends that header. The hub posts push with `curl`, so `curl` must be on the `PATH`. Its pushes name
the stand-in's host as `mac`. A second part runs two hubs at once, as two Macs (each with its own
temp directory, port, stand-in `tailscale` and fake session, the same session id on both), feeds
one store from both clients and checks that both are listed apart, that an answer reaches only its
own Mac's session, and that the other Mac keeps streaming after one is unpaired. CI runs this
test in its own `contract` job.

## Building the app for a phone

The builds need your own accounts and tooling, and nothing here creates one. The repository
holds no EAS project id: `eas init` adds yours to `app.json` the first time you run it.
`eas.json` names three profiles: `preview`, `development` and `production`.

### Which build

**For daily use, on both phones, install the `preview` build.** It is a release build: it runs
only the JavaScript inside it, and it has no development launcher.

**Use `development` only while you work on the app, and never pair it with the Mac you use every
day.** If you did pair one, unpair it and revoke it on the Mac (`octet hub revoke NAME`).

A development build runs whatever JavaScript it is pointed at, and that code holds the device
token:

- With `expo-dev-client`, a tapped `exp+octet://` link can load a bundle from anywhere.
- A Debug build fetches its bundle from Metro over plain HTTP, so anyone on the same Wi-Fi can
  replace it.

Either way, that code can do everything the token allows, without Face ID: start a session in
full access, or allow an approval. Every check of the phone's own is in JavaScript, so none of them
stops it.

A development build says so: it shows an amber "Development build — don't pair with your everyday
Mac" on Pair, Sessions and Settings. It also keeps its token under its own keychain item
(`octet.token.dev`), so it never picks up the pairing of a release build on the same phone.

### iPhone

You need an Apple developer account: the paid program, which a device build requires.

```sh
npm install --global eas-cli
cd apps/mobile
eas login
eas init
eas device:create                                  # register the iPhone for internal builds
eas build --profile preview --platform ios
```

Install the build on your phone from the link EAS gives you.

To develop on the iPhone instead, run `eas build --profile development --platform ios`, then
`npx expo start`. The `development` profile needs `expo-dev-client`, which the project does not
include. `eas build` asks to install it, and whether to is up to you. It adds the dev menu and
launcher to that build.

### Android

An APK, built by EAS or locally.

```sh
cd apps/mobile
eas build --profile preview --platform android     # EAS: a release APK, and a link to it
npx expo run:android --variant release             # or locally: needs the Android SDK and a device
```

To develop instead, build with `eas build --profile development --platform android` or `npx expo
run:android`. The same rule applies: never pair that build with your everyday Mac.

## Pairing

1. On the Mac, start the hub and publish it (see [the guide](../../docs/remote-control.md#phone-app-hub)).
2. In an Octet session on the Mac, type `/remote-control pair`. It shows a QR code and a link.
3. Open the app. Until it is paired it shows **Scan the code from /remote-control pair**: point the camera at
   the code, inside the four corner brackets. Or tap **Enter code instead** and paste the link into
   **Paste link** (with the camera off, those fields show at once). Once the link is valid the app
   says `Pair with <the hub's address>?` and the **Pair** button turns on (it is dimmed until then).
   Give the phone a name under **Device name** (the Mac lists it in `octet hub devices`) and tap
   **Pair**. A toast says `Paired with <the Mac>`. The code works once and expires after 10 minutes.

The app keeps the device token in the iOS Keychain / Android Keystore (`expo-secure-store`), never
in plain storage.

### More than one Mac

The app pairs with up to 4 Macs. To add one, open **Settings › Machines › + Pair another machine**
(off at 4: `4 Macs is the most`) and scan that Mac's code. Every Mac stays connected while the app is
open; the Inbox and the drawer show all their sessions, each named by its Mac, and a Mac that can't
be reached shows "asleep" and greys only its own sessions. In **Machines**, **Rename** gives a Mac a
name of 1 to 40 characters on this phone, and **Unpair** asks `Unpair <name>? Its sessions leave
this phone.` and drops only that Mac (a screen showing one of its sessions goes back to the Inbox).

A push names the Mac it came from (`data.mac`, the hub's published host), and a tap opens that
Mac's session. A host none of your pairings has, no host with several Macs paired, or a session that
has ended opens the Inbox. A Mac you unpair while it can't be reached doesn't hear "notifications
off": it keeps pushing until you run `octet hub revoke <device>` on it, because the phone's push
token is shared with your other Macs (a tap on one finds no pairing for its host and opens the
Inbox). A pairing whose token the
phone can't read shows `offline` in Machines, and on Pair (with Unpair) when it is the only kind
left, so it can always be cleared.

## Screens

Every screen is drawn in the terminal's look: one monospace face, the TUI's colours, 1 px bordered
cards with their titles notched into the border, and touch targets of at least 44 pt. ☰ opens the
side drawer (or swipe in from the left edge) on the Inbox, a Conversation and Settings.

- **Pair.** A panel with the camera in four accent corner brackets and a bottom sheet: `PAIRED`
  (the Macs this phone is paired with, each with **Unpair** while none can be reached) and
  **Enter code instead**, which shows the paste and name fields. Opened from **+ Pair another
  machine**, it reads **Pair another machine**, with Cancel. A scanned code only fills the field; you confirm the Mac it
  names and tap **Pair**. After pairing, one offer to turn on notifications.
- **Inbox.** The live sessions in three tabs (Needs you, Running, Done). An approval is answered
  right on its card; **Allow all N once** answers the ones still open and fully shown.
- **New session / Resume.** Tap the chips for the Mac, folder, engine and model, and mode, write
  the prompt and start; or resume a recent session.
- **Conversation.** The transcript, the model and mode menu, the permission card, the prompt line
  with its slash menu, and a key bar for the terminal's keys.
- **Changes.** What the session's workspace changed since the last commit, opened from **view ›**
  beside a tool entry that edited a file, or **Changes ›** in Usage or Session info. Each file shows
  its status, `+added −removed` and a bar, and a tap opens its diff; **Checks** shows the last test
  command with ✓ or ✗, and **Branch** the branch and how far ahead it is. Pull down to ask again.
  - **Commit** opens a sheet with the message, prefilled with the session's first prompt as one
    line of at most 72 characters; edit it (up to 2,000 characters) and tap **Commit**. Face ID is
    asked every time, whatever **Face ID to allow** says, and a failed or cancelled check sends
    nothing. The Mac commits everything in the workspace (`git add -A`, then `git commit`), and
    its hooks run. A toast says `Committed <hash> on <branch>`, and the list refreshes. A refusal
    (a turn running, a merge under way, nothing to commit, a hook that failed) shows the Mac's
    words in the sheet and keeps the message. If the answer is lost, the sheet says so and the
    list is asked for again, which shows whether it committed. **Commit** is off while the Mac
    can't be reached or an approval waits.
  - **Open PR on the desk** shows `gh pr create` to run at the Mac, and sends nothing. The phone
    can't push, open a pull request or commit only some files.
- **Settings.** Three cards. **Machines**: each Mac with ● or ○, its own state (`online`,
  `connecting…`, `asleep` or `offline`), its host, **Rename** and **Unpair**; this phone's name; and
  **+ Pair another machine**. **Notify me when**: three switches, **An action needs approval**, **A turn
  finishes** and **A session hits an error**, one per kind of push. A switch moves only when the
  Mac acknowledges (a refusal or an unreachable Mac is said below them). Turning one on while
  notifications are off asks the phone's permission and turns on approvals and errors with it;
  turning the last one off turns notifications off. A phone paired now starts with approvals and
  errors on and finished turns off; an app updated from before the switches, with notifications
  on, shows all three on, which is what the Mac was sending it.
  **Face ID to allow**: turns the Face ID check on allowing an approval on or off (turning it off
  asks for Face ID; full access always asks). The footer names the app version.
- **Toasts.** One line at the bottom of a screen. When an approval newly arrives for a session
  other than the one you are in (and you are not on the Inbox), `<name> needs approval` appears
  with **View**, which opens it.

## Notifications

The app can notify you when an agent needs approval, hits an error, or finishes a turn, while the
app is closed.
The Mac's hub sends the pushes through Expo's push service, only to a phone that has no open
connection to it, and shows nothing while the app is open. Push tokens belong to an EAS project, so
notifications need the app's **EAS project id**: it is not in this repository (it is yours). `eas
init` (see [Building the app for a phone](#building-the-app-for-a-phone)) writes it to `app.json`
as `extra.eas.projectId`; build again after that. Without it the **Notify me when** switches in
Settings are off and say so, and pairing skips the notifications step. On iOS, EAS also sets up the
Apple push credentials for the build; on Android, push needs a Firebase (FCM) credential added to
the EAS project. Neither is in this repository.

Set it up with your own accounts (nothing here creates one):

1. `eas init` in `apps/mobile`, if you have not (it adds the project id to `app.json`).
2. iPhone: register the iPhone with EAS before the first build (`eas device:create`), then run
   `eas build --profile preview --platform ios`. When the build asks "Setup Push Notifications
   for your project" / "Generate a new Apple Push Notifications service key", answer yes. The manual
   route is `eas credentials`. This needs the paid Apple developer account that the device build
   already needs.
3. Android, in three parts:
   - In the Firebase console, create a project and add the Android app (the package name in
     `app.json`), then download `google-services.json`. Put it at the root of `apps/mobile` and add
     `"googleServicesFile": "./google-services.json"` under `expo.android` in `app.json` yourself
     (this repository does not add it: a missing file would break prebuild and export). It holds
     only public identifiers, so it may be committed.
   - In the Firebase console go to Project settings, Service accounts, "Generate new private key".
     This FCM V1 service-account key is a secret: **never commit it** (add it to `.gitignore`).
   - Upload it: `eas credentials`, then Android, the build profile (`preview`),
     Google Service Account, "Manage your Google Service Account Key for Push Notifications (FCM
     V1)", "Set up a Google Service Account Key for Push Notifications (FCM V1)", "Upload a new
     service account key". The same page is on expo.dev under Credentials, Service credentials.
4. Build again (`eas build --profile preview --platform ios` or `android`) and install it.

Turn it on in the step after pairing or in **Settings**: the app asks the phone for permission,
then registers its token with the Mac. Turning it off, or unpairing, tells the Mac to stop; an
unpair the Mac doesn't hear (it is out of reach) drops the phone's push registration instead, so
the Mac's next push bounces and it forgets the token. Taking the permission away in the phone's
settings also tells the Mac to stop, on the next connection; give it back and the app registers
again. Tapping a notification opens that session, and opening the app clears its notifications
from the tray. A push says only `Octet` and one of `Approval waiting`, `Session error` or `Turn
finished`. On Android each goes to its own channel (**Approvals**, **Session errors**, **Finished
turns**), which the phone's settings can silence one by one; approvals and errors sound, finished
turns don't. What the hub sends, where, and when is in [the guide](../../docs/remote-control.md#push-notifications).

## Device pass

The tests cannot cover a real phone. Run this on the iPhone 15 and on the Gemini 10 with a `preview`
build from your own EAS project, against real Claude and Codex with tiny prompts in a throwaway
workspace. Tick each one on each phone.

- [ ] Pair the phone (QR code, and the pasted link).
- [ ] Notifications on, then a push on a locked phone: start a turn that needs approval, lock the
      phone, and the push arrives reading `Octet` / `Approval waiting`.
- [ ] Tap that push: the app opens on that session (from locked, from the background, and from a
      closed app).
- [ ] Allow behind biometrics: the approval needs Face ID or the fingerprint, and the turn goes on.
- [ ] With **A turn finishes** on, a finished turn on a locked phone pushes `Turn finished`; with
      the app open, nothing does. With it off, nothing is pushed for it.
- [ ] A session error on a locked phone (an `error` record in its journal: a turn that fails) pushes
      `Session error`, on the iPhone and on Android (where it arrives on the **Session errors**
      channel, with sound). With **A session hits an error** off, nothing is pushed for it.
- [ ] Each switch in **Notify me when** moves only after the Mac acknowledges, and survives a
      restart of the app.
- [ ] An approval that arrives with the app open, still waiting when you leave the app: one push.
- [ ] With `octet hub install`, after a fresh login and no tmux running, start a Claude session
      from the phone.
- [ ] Switch from Wi-Fi to cellular mid-turn: the transcript resumes with nothing missing or doubled.
- [ ] Answer an approval on one phone: it closes on the other phone and on the desk.
- [ ] Change the model and the mode; full access asks for the typed confirmation.
- [ ] Resume a recent session and start a new one, then join it from the desk with `tmux attach`.
- [ ] Return to the app after a long background: it reconnects at once, without a pull to refresh.
- [ ] All three switches off in Settings: no more pushes. Revoke the phone on the Mac
      (`octet hub revoke`): the app says it is unpaired.
- [ ] Pair a second Mac from **+ Pair another machine**: both Macs' sessions show in the Inbox,
      each named. A push from each Mac, tapped, opens that Mac's session.
- [ ] Rename a Mac; unpair it while its conversation is open: the app goes back to the Inbox and
      the other Mac keeps working.

## Performance notes

- Every list over a few items is a FlashList (v2, which measures rows itself: no
  `estimatedItemSize`). Sessions and Recent use `keyExtractor` and `extraData` for the one flag
  that changes how rows draw. The transcript adds `getItemType` (one type per entry kind, so
  recycled rows are like for like), `maintainVisibleContentPosition={{ startRenderingFromBottom:
  true }}` (it opens at the end and keeps its place when rows change), and `scrollEventThrottle`
  32 for the follow-the-output logic.
- Rows (`TranscriptRow`, `SessionCard`, `RecentRow`) are `React.memo` with stable callbacks and no
  inline object or function props.
- A reply is Markdown, whether it is streaming or done. A streaming reply keeps the blocks before
  its last whole one and parses only what came since, so a chunk costs the same however long the
  reply is. A finished reply is parsed once per text, in a cache of at most 2 MB that unpairing
  clears.
- A reply draws its newest 400 blocks, under "N earlier blocks of this reply are on the Mac".
- A tool call costs a fixed 256 bytes toward the memory limits, even when its strings are empty.
  A tools entry holds 500 calls, then "N more calls on the Mac". An end finds its call by id.
- The client delivers frames in one batch at most every 50 ms; the store applies a batch in one
  `set`, and every part a batch did not change keeps its identity, so a screen that selects
  narrowly does not draw for another session's output.
- The phone keeps the transcripts of the 4 sessions you looked at last. Opening another one
  replays it from the Mac.
- Budgets that tests hold, with room for a busy machine:
  - `src/perf.test.ts`:
    - 20,000 text and 2,000 tool events through the reducer and store in under 1 s, with
      another session's view untouched;
    - a hostile turn of 20,000 empty tool calls in under 600 ms, in bounded memory;
    - a turn of 5,000 Read calls in under 100 ms.
  - `src/perf.render.test.tsx`:
    - 100 streamed text events draw a session's transcript at most once per batch plus two, and
      never another session's transcript or a card;
    - 10 chunks into a streaming reply of 16,000 paragraphs in under 500 ms;
    - a finished reply of 16,000 code fences with fewer than 1,000 text nodes.

## Troubleshooting

**The Inbox shows "Can't reach your Mac".** The phone must be on the tailnet:
open the Tailscale app, check it is connected and signed in to the same tailnet as the Mac, and
that the Mac itself is online there. The hub is reachable only through your tailnet; there is no
other route. Check that `octet hub` is still running on the Mac.

**`/remote-control pair` says "Publish it first".** The hub is running but `tailscale serve` is
not set up for it. Run `tailscale serve --bg 47823` on the Mac yourself (the hub never changes
your Tailscale configuration), as described in
[docs/remote-control.md](../../docs/remote-control.md#put-it-on-your-tailnet). The hub looks again
every 10 seconds, so it can take that long to notice.

**The app is back on the pairing screen ("Scan the code from /remote-control pair"), saying "This phone was unpaired on the Mac".** The Mac
removed this phone (`octet hub revoke NAME`), so the hub no longer knows its token. Pair again with
`/remote-control pair`. `octet hub devices` lists the phones the Mac knows.

**Allow says "Face ID isn't set up on this phone. Turn it off in Settings to allow from here."**
Allowing an approval asks the phone's biometrics first. If Face ID or a fingerprint isn't set up on
the phone (or none is enrolled), the app can't ask and sends nothing. Enrol one in the phone's own
settings, or turn off **Face ID to allow** in the app's Settings (☰, then Settings).
Deny never needs it.

**A session I started doesn't appear.** Phone-started sessions run in tmux on the Mac. Check
`tmux ls` for an `octet-...` session (`tmux attach -t octet-NAME` shows what it says): tmux must
be installed, the workspace must be one the hub allows (a folder from your recent sessions, or in
`hub.json`'s `workspaces`), and at most eight phone-started sessions run at once. Pull down on the
Inbox to refresh it; after a Start the app says "NAME hasn't appeared yet. Check tmux on the Mac." when the session is late.

**Pairing says the code is invalid or expired.** Codes work once and last 10 minutes. Run
`/remote-control pair` again for a new one.

**The camera is off.** The pairing screen says "Camera access is off — paste the link instead": paste the
link into **Paste link** (the fields show at once).
