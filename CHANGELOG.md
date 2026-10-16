# Changelog

All notable changes to Octet are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

The first public preview: a native Rust terminal workspace for the official
Claude Code and Codex CLIs.

### Added

- A Live Activity for the phone app, in the Dynamic Island and on the Lock
  Screen (iOS 17.2 or later; the app still runs on iOS 16.4). It shows whether
  a session runs, waits for approval or is done, and for how long, with the
  waiting approval's session name and command. The app keeps those in the
  phone's App Group only (at most 64, privacy-sensitive, cleared on unpair).
  - **One activity, two drivers.** While the app is open (on any screen), the
    app drives the activity itself, at most once a second. While it isn't, one
    Mac's hub starts, updates and ends it through APNs, signed with your own
    `.p8` key named in `hub.json` (`apns: {team_id, key_id, key_path,
    sandbox}`). The key must be `0600` and yours, or APNs stays off with one
    log line. The pushes are content-free: the state, the hub's session id,
    its host, two times and a count. Each is stale 15 minutes on, and a stale
    activity shows dimmed, marked **Stale**.
  - **One Mac drives it.** The phone gives its ActivityKit tokens
    (`register_activity`) to one Mac, the one of the lead session when the app
    was last in front, and tells the others `clear_activity`. While you are
    away the Island follows that Mac, and the others still send
    notifications. A Mac that missed its `clear_activity` is told again on its
    next connection and each time you come back to the app. A registration
    refused for now is tried once more 5 seconds later.
  - **The background.** A new activity's token that comes while the app is in
    the background is handed over on a short connection whose hello says
    `background: true`. When activities appear together, only the ones seen
    earlier are ended, so one always stays.
  - **Deny and Allow.** **Deny** on the Island answers no on the Mac that owns
    the approval, from the background too, over such a connection. **Allow**
    opens `octet://approve/...`, the approval's session, behind Face ID and
    the arm; a forged or stale link opens the Inbox.
  - **Settings › Live Activity** (on by default) turns it all off, and follows
    the phone's own setting.
  - **Tests.** The contract test checks one content-free start, then an update
    to the handed-over token and no end, at a local stand-in for APNs.
  - **Known limits:**
    - An approval that starts the activity while you are away brings two
      alerts, the push's and the activity's.
    - The hand-over and a background Deny need iOS to give the app background
      time, and an unlocked phone, since the Macs' device tokens are readable
      only while unlocked.
    - On a locked phone, the Mac can neither update nor end the activity it
      started until you unlock and open the app. The activity shows as stale
      after 15 minutes, and a Deny waits for the unlock.
    - No second activity starts for up to 8 hours while the first awaits its
      token. Opening the app with nothing to take over lifts that hold.
    - A token counts as live for 12 hours from its first registration.
    - The Swift is parsed and type-checked against the Mac Catalyst SDK only.
      It is unverified until the first device build.
- The phone app pairs with up to 4 Macs. Each pairing keeps its token in its
  own keychain entry, the one pairing of an earlier version moves over on
  first launch, and every Mac stays connected while the app is open. The
  Inbox and the drawer show every Mac's sessions together, each named by its
  Mac ("2 Macs · 1 online"), a Mac that can't be reached greys only its own
  sessions, and every command goes to the Mac that owns its session, even when
  two Macs report the same session id. **Settings › Machines** lists each Mac
  with its host and state, renames it (1 to 40 characters) and unpairs it
  alone ("Unpair <name>? Its sessions leave this phone."; a screen showing
  its session goes back to the Inbox), and **+ Pair another machine** adds
  one, up to 4. A push now names its Mac in `data.mac`, the host of the hub's
  published address (lowercased, and only when it is a plain DNS name; never
  in the title or body), and a tap opens that Mac's session, or the Inbox for
  a host no pairing has, no host with several Macs, or a session that ended.
  A Mac unpaired while it can't be reached keeps pushing until `octet hub
  revoke <device>` on it, as the phone's push token is shared with the other
  Macs. The contract test runs two real hubs at once.
- Changes and commits on the phone. A session's Changes screen lists what its
  workspace changed since the last commit (at most 500 files), opens a listed
  file's diff (at most 64 KiB or 2,000 lines), and shows the last test run and
  the branch. **Commit** sends a message you edit, prefilled from the
  session's first prompt, after Face ID every time: the hub runs `git add -A`
  and `git commit -F -` limited to the workspace, with the repository's hooks,
  and the app toasts the short hash and the branch. The hub refuses a commit
  while a turn runs or an approval waits, during a merge, rebase, cherry-pick
  or revert, and when nothing changed, and audits it as `commit` with the
  phone's name, never the message. Only paths from the hub's own last listing
  can be diffed, and git runs with no `GIT_*` from the hub's environment, no
  fsmonitor, and no external diff or textconv program. The phone can't push,
  open a pull request or commit part of the changes: **Open PR on the desk**
  shows `gh pr create` to run at the Mac. The contract test covers `changes`,
  `diff` and `commit` on a temp git repository.
- Safe desk commands, usage, error pushes and questions on the phone. The
  slash menu runs `/new` (after a confirm), `/session`, `/export` and `/help`
  through one `command` request that names the command and takes no argument:
  the desk runs its own handler and answers with its own words, `/new` is
  refused while a turn runs or the desk has a draft, and `/export` writes only
  to the desk's default path and is refused during a turn. A Usage screen
  shows the session's engine, latest usage line, turns and journal, and every
  session's usage today. The hub pushes `Session error` for an error in a
  session's journal (ranked after an approval and before a finished turn,
  within the same limits), and each phone chooses its kinds: approvals,
  errors and finished turns, with approvals and errors on for a new pairing
  and all three kept for a phone paired before. A session whose last turn
  ended on a question shows in the Inbox's Needs you with Reply, until you
  send it something or start afresh.
- Push for the phone app, and a hub that keeps itself running: the hub tells a
  phone that is not looking at the app when an approval is waiting or a turn
  finished, through Expo's push service. The message is only `Octet` and
  `Approval waiting` or `Turn finished` plus an opaque session id, sent with
  `curl` (the token never in a command line or a log) to phones that turned it
  on and have no open connection, at most six a minute each; a tap opens that
  session. The app pings the hub to notice a dead link and reconnects at once
  on returning to the foreground. `octet hub install` and `octet hub uninstall`
  run the hub as a launchd agent (macOS) or a systemd user unit (Linux). The
  contract test (`ping`, `register_push` and the push itself) now runs in CI,
  and `apps/mobile/README.md` has the push setup and a device-pass checklist.
- A phone hub and a bridge in every terminal interface: `octet hub` serves
  your running sessions to a phone app over your tailnet (loopback only,
  published with `tailscale serve`), with transcripts, prompts, approvals,
  mode and model changes, recent sessions and starting or ending a session.
  Each interface shares its session through a private socket in `run/`; a
  device token and your tailnet login are both required, every remote action
  is audited, and full access needs a typed confirmation.
- A phone app (`apps/mobile`, Expo and React Native): it pairs by QR code or
  link, lists your sessions with what each is doing, streams transcripts,
  sends prompts, answers approvals (Face ID or fingerprint first), changes
  mode and model, resumes recent sessions and starts new ones in a workspace
  you choose. The device token lives in the phone's secure storage, and a
  revoked phone is told to pair again. A contract test (`make mobile-contract`)
  runs the app's hub client against the real `octet hub`. Build and
  troubleshooting notes are in `apps/mobile/README.md`.
- `/remote-control pair` shows a QR code and link that pair a phone with the
  hub (single use, 10 minutes); `/remote-control` also reports the hub.
- `octet hub devices`, `octet hub revoke NAME` and `octet hub watch` list
  paired phones, unpair one, and print what every session is doing.
- `--no-remote` keeps a session off the hub.
- Plugins pass through: Claude sessions load the plugins your Claude
  settings enable (user, project and local `enabledPlugins`), with hook
  activity journaled and the hooks that fail, block or answer an approval
  shown. `--no-plugins` and `--no-plugin-hooks` turn plugins, or only their
  hooks, off on both providers.
- One skills list: `/skills` shows every provider's skills (cached per
  workspace in `skills.json`); Enter puts one in the prompt, and `/NAME args`
  runs the current provider's.
- One model list across providers: `/model` shows every provider's models,
  fetched from each CLI when missing or a day old (no session, no tokens) and
  cached in `models.json`; `/model refresh` fetches them now. `/model NAME`
  switches to the provider that lists NAME, and Tab completes model names.
- A model picker: `/model` (or typing `/model `) lists every provider's models
  above the prompt; Up/Down choose, typing filters, Enter switches.
- Multi-turn sessions with Claude Code or Codex, with streamed replies, tool
  activity, usage and errors in one transcript.
- An offline `demo` engine that needs no vendor CLI or login.
- Model discovery from the installed CLI (`/model`, paged), model changes
  within a provider, and provider switching mid-session (`/model claude`,
  `/model codex`).
- Four permission modes (`ask`, `accept-edits`, `auto`, `full-access`)
  mapped onto each vendor, set with `--mode`, `/mode` or Shift+Tab.
- An approval dialog showing the complete request, failing closed on expiry,
  cancellation, oversize and unknown request types.
- `--approval-timeout SECONDS` (10–3600, default 120) for the approval window.
- A terminal bell and desktop notification (OSC 9) when an approval opens.
- Persistent multi-turn goals (`/goal`), which survive restarts and load
  paused.
- Private, append-only JSONL journals for every session, `/session` to show
  the path and `/export` to copy one without overwriting.
- `/new`, `/reconnect` and `--resume` for vendor sessions.
- A provider switch carries the conversation: the first prompt to the new
  provider includes a transcript of what was shown (at most 64 KiB, newest
  turns and the first prompt kept), while you see only what you typed.
- Phone access over tmux, Tailscale SSH and mosh, with a step-by-step guide
  and `/remote-control`, a read-only check that names each fix and prints the
  phone command.
- A multiline prompt editor with Unicode-aware editing, bracketed paste and
  history; the prompt box grows with the draft.
- A responsive layout down to 38 × 12 cells with a Task sidebar from 112
  columns, `NO_COLOR` support, suspend with Ctrl+Z, and terminal restoration
  on exit, signals and panic.
- Octet, the header mascot, whose pose follows the agent's activity.
- A build-time guard that fails if any crate names a vendor credential store.
- A follow-up queue: Enter during a turn queues the prompt (up to 8), and
  `/queue` lists or clears it.
- `/steer TEXT` adds to the running Codex turn; for Claude it queues the text.
- Reasoning effort with `--effort LEVEL` and `/effort`.
- `/fork` continues a conversation in a new vendor session; `/compact` asks
  the vendor to compact its context.
- `/image PATH` attaches PNG, JPEG, GIF or WebP images (up to 5 MiB, 4 per
  prompt).
- `/sessions` lists this workspace's recent vendor sessions and `/resume N`
  reopens one.
- `--print` (`-p`), `--output json` and `--rpc` run Octet without the
  interface, for scripts and other programs.
- Release archives with SHA-256 files for macOS (Apple silicon) and Linux
  (x86-64, ARM64), built when a version is tagged.
- `@` file mentions, Tab completion for paths and commands, `!`/`!!` shell
  commands with output attached to the next prompt, Ctrl+G to write the
  prompt in an external editor, and `/copy` (Ctrl+X) through OSC 52.
- `!` commands run in their own session, so credential prompts (git, ssh,
  sudo) fail at once instead of drawing over the screen; progress output
  keeps its last state, tabs survive in attached output, and the status line
  shows each command's result.
- `/copy` takes the whole reply as sent, tabs included, and keeps the reply
  on screen copyable while a new turn starts.
- The command palette also offers Ctrl+G, `@` and `!`; Tab fills a common
  prefix and lists the matches, without blocking on slow folders.
- The offline demo shows the full text a model would receive when `!`
  attachments or a goal change it.
- Stopping Octet (SIGTERM, SIGHUP) while an external editor is open asks the
  editor to quit instead of leaving it on the terminal.
- At most eight approvals wait at once; more are denied with a notice. Claude
  is told why each approval was denied (your refusal, the timeout, the cap,
  a request too large to show, a cancelled turn).
- Typed tool and approval events: the journal's `tool` records, and the
  `tool` events of `--output json` and `--rpc`, carry
  `{id, name, target, phase, detail}` (`phase` is `running`, `done` or
  `failed`); `approval` records and events gain `summary`
  `{action, command, files, cwd, scope}`.
- Grouped tool activity: a run of tool calls is one transcript entry
  (`▸ Read 3 files · Ran 1 command`), a failed call opens in amber with the
  last 8 lines of its output, and Ctrl+O shows or hides every call's detail.
- The approval dialog leads with the action, the `$ command`, the files, the
  directory and `Allows: This request only, once`, then the full request.
- One hint row under the prompt for what Enter does now, and a chips row
  (`+ ls (exit 0) · + image x.png · 2 queued`) for what will go with the
  next prompt.
- A Task sidebar (goal, now, queued prompts) from 112 columns; F2 hides it.
- The command palette filters as you type; Backspace edits the filter.
- A compact layout under 30 rows: a one-row header, no vertical margin and a
  standing key hint.
- Dual licensing under MIT or Apache-2.0.
- CI on macOS and Ubuntu (`make rust-check`) plus a `cargo audit` job.

### Changed

- The hub and the phone at the stated scale (4 Macs, 200 sessions each).
  - The sessions list goes to the phones at most four times a second. A
    change after a quiet spell goes at once, and the rest within a quarter of
    a second, together.
  - Each list is made once and shared by every phone. A phone behind gets the
    newest list, not every one it missed.
  - A list holds at most 200 rows, the most recently active, in at most
    96 KiB. Past that, rows drop the approval's gist, then the usage line,
    the NOW line and the goal, least recently active first. The approvals
    count always stays, and the phone keeps the gist it had while that count
    says the approval still waits.
  - The phone cleans only the last list of each Mac's batch. A test streams
    4 × 200 rows with the whole shell up and holds each store commit to the
    50 ms between commits.
  - `devices.json` is written off the hub loop. New sessions' journals are
    read 8 at a time at start-up.
  - A phone's `/export` runs as the desk's own export job, so the desk keeps
    working while it copies.
  - At most 32 connections wait to say hello. A newer one pushes out the
    oldest only once that one has waited 2 seconds; with every waiter younger,
    the hub answers `503 Too many connections` and the app retries with its
    backoff.
- The phone app has a new look, drawn after the terminal: an Inbox of your
  sessions in three tabs with approvals answered on their cards, a side
  drawer, a redrawn New session and Conversation (permission card, slash menu
  and key bar), and Pair and Settings in notched cards. Pair is a camera in
  four corner brackets with an "Enter code instead" sheet; Settings has
  Machines, Notify me when and Face ID to allow. A toast says "Paired with
  <Mac>" after pairing, and "<name> needs approval · View" when an approval
  arrives for a session you are not in. Every approval guard stays (Allow is
  once only; Face ID and the 400 ms arm apply on each Allow).
- Enter in `/skills` (or Tab) puts `/name ` in the prompt instead of running
  the skill; nothing runs until it is sent. Sending another provider's skill
  switches provider first, with its arguments, and an amber hint says so.
- Scrollback stays still while output arrives; when new rows arrive below,
  the conversation's bottom border says "New output below · Ctrl+End
  follows" until you follow.
- Claude's tool results end its tool calls, so failures show. The headless
  denial warning names the action and the command's first line.
- The sidebar shows the task rather than the engine, model and session, which
  are in the header and `/session`.
- An approval takes answer keys 0.4 seconds after it opens, so a letter
  typed as it appears cannot allow it.
- While the interface is briefly behind, Octet stops reading the vendor
  instead of stopping the session; a burst of output no longer ends it (a
  reader stalled past 2 seconds still does, with a journaled reason).
- A background job carries on across a reconnect; quitting waits only for
  an export. `/export PATH` is relative to the workspace.
- Paste keeps tabs. The external editor runs as the shell runs `$EDITOR`
  (a quoted path with spaces works), on a draft in a private folder.
- `--print` takes a prompt that starts with dashes, and `--print=TEXT`.
- The protocol gate launches the vendor with Octet's own arguments and
  limits.
- Edition 2024; clippy pedantic, `#[expect(lint, reason)]` for every
  exception, `Debug` on every public type; a `cargo doc` build and a
  `cargo-deny` licence check in CI.

- `@` suggestions rank about four times faster in large workspaces (under
  25 ms for 50,000 files).
- Help, the command palette, Tab completion, the sidebar and `octet --help`
  are built from one command list; help shows one line per command and sizes
  itself to fit narrow terminals.
- A `!` command stopped by its time limit names that limit.
- Release builds are about 40% smaller (thin LTO, stripped symbols).
- The protocol gate moved to its own `octet-gate` crate.
- `/sessions`, `/export` and `/remote-control` run in the background. One
  runs at a time; a second is refused and keeps its line in the prompt box.
  A session that ends waits up to 5 seconds for a running job, so an export
  is never cut short, and shows its result in the next screen.
- Usage errors exit 2 and other failures 1. Journal directories Octet
  creates are private (0700).
- Vendors come from one provider table, and each vendor's wire protocol is
  its own `Protocol` implementation, so adding a vendor CLI is one file and
  one row ([guide](docs/rust/adding-a-provider.md)). No behaviour change.
- Switching provider (`/model claude`, `/model codex`) or `/new` starts the
  next CLI while the old one exits, instead of after it. Commands that
  resume a session still wait, so two CLIs never share one.
- Claude Code gets 1.5 seconds to exit on its own (it needs about 0.9)
  rather than being killed after 0.4; Codex keeps 150 ms. A turn still
  running is interrupted first. Commands that resume a Claude session
  (`/reconnect`, `/effort`, `/mode`, `/fork`, `/model` within Claude) and
  quitting from Claude take about 0.5 seconds longer for it.
- A streaming reply re-wraps only its last line on each update. A reply
  over 64 KiB drops its start in 16 KiB steps, so it may briefly show up
  to 80 KiB.

### Fixed

- A session's error push, question or usage could wait until another session
  changed, when the hub's tail pass was cut short.
- After a Lock Screen start that never handed its token over, the Island
  stayed off for up to 8 hours, even after opening the app.
- A launch that registered the same Live Activity token again reset its
  12-hour count.
- Two Live Activities appearing together could end each other and leave none.
- A reconnect storm could push out a phone that was slow to say hello.
- A failed disk write could be reported as written (a torn journal record,
  a truncated goal file installed over a good one); every write is now
  flushed and checked, and new files and renames are synced.
- A `!` command's processes outlived a reconnect or quit.
- Claude subagent (Task tool) messages showed as the reply.
- The panic hook restored the terminal for a background job's panic.
- An RPC answer queued behind a pipelined prompt timed out; `--rpc` did
  not exit on `quit` while stdin stayed open.
- The vendor's last error line was occasionally lost; a blank line from
  the vendor ended the session.
- `tput`'s escapes left a stray "B"; an unterminated escape hid the rest
  of a reply; Arabic text wrapped past the screen edge.
- "Cancelling…" stayed after cancelling a connection; a signal during a
  reconnect was lost; a hung `git` stalled the `@` index.

- A reply over 2 MiB is cut with a note instead of stopping the session.
- `NO_COLOR` now removes every colour, not only the mascot's.
- Esc in the instant a queued prompt is sent cancels that prompt's turn.
- `/steer` during a cancel is queued instead of lost; a failed `/image`
  keeps its line; Up recalls a prompt's images even if sending failed.
- `/fork` announces the fork only once the vendor names the new session.
- An effort level Claude does not take is refused instead of breaking every
  reconnect.
- Print mode exits 130 after Ctrl+C even if the vendor then stops, handles
  SIGTERM (143), reports an over-long stdin prompt as one, shuts the session
  down when stdout or stderr is closed, and exits 1 when stdin cannot be
  read.
- A `--cwd` path that is not UTF-8 is refused.
- In the demo, a cancel from before a turn no longer stops the next one.
- The release workflow runs the checks, matches the tag to the version,
  publishes `-` tags as pre-releases and can be re-run.
- PTY acceptance tests stop sending Ctrl+C after terminal restoration and
  capture trailing shutdown output, avoiding false cleanup failures with live
  providers.

[0.1.0]: https://github.com/cartmancodes/octet/commits/master
