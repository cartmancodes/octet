# Octet for your phone

An Expo app (React Native, SDK 57) that lists the Octet sessions running on your Mac, shows each
transcript, sends prompts, answers approvals, changes mode and model, and starts or resumes a
session. It talks to `octet hub` on your Mac over your tailnet; it holds no vendor credentials and
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
modules that need a development build (below). `npm start` still serves the bundle to a
development build.

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
answer, a dropped connection with a cursor (no duplicates), and a revoke. It builds nothing:
run `scripts/rust-env.sh cargo build -p octet` first. Without that binary it skips. It never
touches Tailscale: the hub is given a stand-in `tailscale` on its `PATH`, and the test's socket
adds the `Tailscale-User-Login` header that `tailscale serve` adds in real life. The app never
sends that header.

## Building the app for a phone

Both builds need your own accounts and tooling, and nothing here creates one. The repository
holds no EAS project id: `eas init` adds yours to `app.json` the first time you run it.

### iPhone

A development build needs an Apple developer account (the paid program, for a device build).

```sh
npm install --global eas-cli
cd apps/mobile
eas login
eas init
eas build --profile development --platform ios
```

`eas build` asks to create the `development` profile (`eas.json`) on first use: choose a
development client for devices. Install the build on your phone from the link EAS gives you
(register the phone first when asked), then run `npx expo start` and open it from the app.

### Android

An APK, built by EAS or locally.

```sh
cd apps/mobile
eas build --profile development --platform android      # EAS: gives you an APK link
npx expo run:android                                     # or locally: needs the Android SDK and a device or emulator
```

## Pairing

1. On the Mac, start the hub and publish it (see [the guide](../../docs/remote-control.md#phone-app-hub)).
2. In an Octet session on the Mac, type `/remote-control pair`. It shows a QR code and a link.
3. Open the app. Until it is paired it shows **Pair with your Mac**. Scan the code with the
   camera, or paste the link into **Paste link**. Once the link is valid the app says `Pair with
   <the hub's address>?` and the **Pair** button turns on (it is dimmed until then). Give the phone a
   name under **Device name** (the Mac lists it in `octet hub devices`) and tap **Pair**. The code
   works once and expires after 10 minutes.

The app keeps the device token in the iOS Keychain / Android Keystore (`expo-secure-store`), never
in plain storage. **Settings** shows where you are paired, turns the Face ID check on allowing an
approval on or off (full access always asks), and unpairs.

## Performance notes

- Every list over a few items is a FlashList (v2, which measures rows itself: no
  `estimatedItemSize`). Sessions and Recent use `keyExtractor` and `extraData` for the one flag
  that changes how rows draw. The transcript adds `getItemType` (one type per entry kind, so
  recycled rows are like for like), `maintainVisibleContentPosition={{ startRenderingFromBottom:
  true }}` (it opens at the end and keeps its place when rows change), and `scrollEventThrottle`
  32 for the follow-the-output logic.
- Rows (`TranscriptRow`, `SessionCard`, `RecentRow`) are `React.memo` with stable callbacks and no
  inline object or function props. A reply that is still streaming draws as plain paragraphs, so
  a chunk lays out only the last one; Markdown is parsed once the reply is done.
- The client delivers frames in one batch at most every 50 ms; the store applies a batch in one
  `set`, and every part a batch did not change keeps its identity, so a screen that selects
  narrowly does not draw for another session's output.
- Budgets that tests hold: 20,000 text and 2,000 tool events through the reducer and store in
  under 1.5 s with another session's view untouched (`src/perf.test.ts`), and 100 streamed text
  events drawing a session's transcript at most one time per batch plus two, and never another
  session's transcript or a card (`src/perf.render.test.tsx`).

## Troubleshooting

**The Sessions list shows "Can't reach your Mac".** The phone must be on the tailnet:
open the Tailscale app, check it is connected and signed in to the same tailnet as the Mac, and
that the Mac itself is online there. The hub is reachable only through your tailnet; there is no
other route. Check that `octet hub` is still running on the Mac.

**`/remote-control pair` says "Publish it first".** The hub is running but `tailscale serve` is
not set up for it. Run `tailscale serve --bg 47823` on the Mac yourself (the hub never changes
your Tailscale configuration), as described in
[docs/remote-control.md](../../docs/remote-control.md#put-it-on-your-tailnet). The hub looks again
every 10 seconds, so it can take that long to notice.

**The app is back on "Pair with your Mac", saying "This phone was unpaired on the Mac".** The Mac
removed this phone (`octet hub revoke NAME`), so the hub no longer knows its token. Pair again with
`/remote-control pair`. `octet hub devices` lists the phones the Mac knows.

**Allow says "Face ID isn't set up on this phone. Turn it off in Settings to allow from here."**
Allowing an approval asks the phone's biometrics first. If Face ID or a fingerprint isn't set up on
the phone (or none is enrolled), the app can't ask and sends nothing. Enrol one in the phone's own
settings, or turn off **Face ID to allow** in the app's Settings (the gear on the Sessions screen).
Deny never needs it.

**A session I started doesn't appear.** Phone-started sessions run in tmux on the Mac. Check
`tmux ls` for an `octet-...` session (`tmux attach -t octet-NAME` shows what it says): tmux must
be installed, the workspace must be one the hub allows (a folder from your recent sessions, or in
`hub.json`'s `workspaces`), and at most eight phone-started sessions run at once. Pull down on the
Sessions list to refresh it; after a Start the app says "NAME hasn't appeared yet. Check tmux on the Mac." when the session is late.

**Pairing says the code is invalid or expired.** Codes work once and last 10 minutes. Run
`/remote-control pair` again for a new one.

**The camera is off.** The pairing screen says "Camera access is off — paste the link instead": paste the
link into **Paste link**.
