# Octet mobile, rebuilt to the prototype — design

**Status:** approved in conversation, 2026-10-15.

**Builds on:** `docs/superpowers/specs/2026-10-13-mobile-remote-design.md`, called "the remote spec"
below. Its security rules still hold unless this document says otherwise.

**Reference:** the prototype source is in `docs/design/mobile-prototype/`.

## Goal

Rebuild the phone app to look and work like the Claude Design prototype "Octet Mobile Prototype".
It has a terminal look in Octet's colours, an Inbox with inline approvals, a conversation with a key
bar, a drawer, more than one Mac, a Changes screen, a Usage screen, and a Dynamic Island. It ships in
five phases, each working on its own. Phases B, C and E add hub and bridge APIs.

## Decisions taken with the user

| Question | Decision |
|---|---|
| Scope | Everything, in phases A–E. |
| Allow scope | **Once only.** There is no "Session" button and no `a` key: the phone never widens permissions. |
| Slash commands from the phone | **A fixed safe list:** `/model` and `/mode` (as today), plus `/new`, `/session`, `/help`, `/export` through a new typed command. Any other `/` text is refused. |
| Changes screen | **Read-only, plus commit.** Diffs, checks and branch are read. Commit is behind Face ID. No push or PR from the phone. |
| More than one Mac | **All connected at once**, merged in the Inbox and drawer. |
| Dynamic Island | **The last phase, done in full.** A Live Activity, updated by the hub through APNs, content-free. |

## Rules that hold in every phase

- **Guarded actions.** Allow, Allow all, full access and Commit sit behind Face ID. The 400 ms arm
  and the hidden-padding fold apply wherever an approval is shown: Inbox cards, the Permission card,
  Allow all and the Island.
- **New phone commands.** Every one is:
  - a closed set, validated in `octet-core::remote::protocol`;
  - refused before hello;
  - rate-limited;
  - audited with the device name;
  - never run through a shell.
- **Content-free pushes.** Pushes and Island updates carry no workspace names, commands, file names
  or text.
- **Performance budgets** (the perf tests enforce them):
  - one store commit per 50 ms while streaming, across all Macs;
  - memoised rows, and only the visible session re-renders;
  - the current transcript limits (1,000 entries, 4 MiB per session, 4 live views, 400 Markdown blocks);
  - no extra re-renders per streamed event from the key bar or header;
  - Inbox ordering and tab counts cost O(sessions) per batch.
- **Gates for every phase:**
  - `make rust-check`, `make mobile-check`, `npm run export:check` and `make mobile-contract`;
  - Linux clippy, and Linux tests in Docker (`--init`);
  - the live end-to-end and attack harness, re-run at the end.

## Look (all phases)

- **Type.** One monospace face: Menlo on iOS, monospace on Android. 14 px body, 12 px labels, 20 px line height.
  - Transcript text cycles 12 → 13 → 14 px with `Aa`, saved in settings (default 13).
- **Colours.** The existing theme tokens, which equal the TUI's.
  - Muted upper-case section labels.
  - Cards with a 1 px edge border and a 12 px radius.
  - Card titles notched into the top border.
  - Amber only for approvals, warnings and errors.
- **Touch targets** of at least 44 pt.
- **Mascot.** The existing generated poses: idle, thinking, searching, coding, success, approval, error, sleeping.

## Phase A — the new UI on today's hub (app only)

**Navigation**
- A single stack with a side drawer.
- The drawer opens with ☰, or by swiping right from the left edge on Inbox, Conversation and
  Settings. A left swipe or a tap on the scrim closes it.
- Drawer contents:
  - Octet, with a "N need you · M running" subtitle and a + (new session) button;
  - Inbox, with an amber count badge;
  - sessions grouped under the Mac (● online, ○ unreachable), each with its status word (approve,
    a phase, done, question, or time ago);
  - Settings.

**Pair**
- A camera viewfinder with four accent corner brackets, under "Scan the code from **octet pair**".
- A bottom sheet:
  - "PAIRED" with each paired Mac and its status;
  - "Enter code instead", which leads to the existing paste field, name field and confirm flow.

**Inbox** (replaces the Sessions list)
- **Header:** ☰ · Inbox, with "● studio-mac · online" under it · +.
- **Segmented tabs:**
  - "Needs you N": an approval is waiting, or it's a question;
  - "Running N": a turn is running;
  - "Done": idle, success or ended.
- **Cards:** the session name (workspace folder) and time ago, then a status line:
  - an approval in amber, "● Bash · cargo test";
  - running in accent, "● THINKING · claude";
  - otherwise muted, "studio-mac · claude".

  Then a two-line muted excerpt: the approval's reason or summary, the question, the last tool, or
  the last reply.
- **Approval cards:**
  - **Allow once** (accent): Face ID if on, then armed 400 ms after the card shows.
  - **Deny**, or **Review** for a file-edit approval, which opens the conversation.
- **Question cards:** a **Reply** button that opens the conversation with the prompt focused.
- **Bottom button:** "Allow all N once" when two or more approvals wait. It takes one Face ID pass,
  then answers each request once, each still armed. Otherwise it reads "New session".
- **Empty states:** the success mascot with "Nothing needs you.", or the sleeping mascot with
  "Nothing running." or "No finished sessions.".

**New session**
- **Header:** Cancel · New session · Start ↑. Start is accent when there's text, muted when not.
- **Prompt:** a full-screen field, "What should the agent do?".
- **"TAP TO CHANGE" chips**, each cycling its own value:
  - the Mac (online ones only);
  - the folder (the allowed workspaces, as today);
  - engine / model;
  - mode (ask, accept-edits, auto; full access stays in its guarded flow on the mode chip's
    long-press).
- **Start** sends the existing `start`. When the new session appears, the first prompt goes to it
  with `prompt`, and the conversation opens. Empty text shows the toast "Write a prompt first.".

**Conversation**
- **Header:** ☰ · the session name ▾, with "model · mode" under it · Aa · ⋯ (Session info / Usage).
- **The ▾ dropdown:**
  - "MODEL · engine": the catalog's models for the session's engine, the current one marked ✓;
  - "PERMISSION MODE": a segmented control (ask · accept-edits · auto) with one line of help:
    - ask: "Ask before every command and edit.";
    - accept-edits: "Edit files without asking; still ask before commands.";
    - auto: "Run commands and edits without asking.";
  - "Session info ›" with the short session id.

  Full access stays in the existing guarded sheet: biometrics, then typing `full access`.
- **Transcript header:**
  - the mascot pose for the phase;
  - "**Octet** v<app version> · <mac>";
  - "**mode** · engine / model", with the mode in accent unless it's ask;
  - "PHASE · ~/path", with the phase in accent, or amber for approval.
- **Entries:** labelled `YOU` (accent), `OCTET` (fg), `TOOL` (muted), `NOTE` (muted) and `ERROR` (amber).
  - The existing grouping, Markdown and bounds stay.
  - A tool entry for a file edit ends with an accent "  view ›", which opens Changes on that file
    (Phase C; before then it isn't shown).
- **Permission card:** an amber border, "**Permission required** #id" notched into it, then rows:
  - TOOL — the action;
  - CMD or FILE — the command or files, folded and marked as today;
  - REASON — the summary or detail.

  Buttons Allow · Deny. The existing approval guards apply unchanged.
- **Slash menu:** shown while the prompt starts with `/` and has no space. Each row is ▸ /cmd,
  description, current value. Rows: `/model` · `/mode`, plus `/new` · `/session` · `/export` · `/help` from Phase B.
- **Prompt line:** `>` and a single-line field. The placeholder follows the state:
  - "Or say what to do instead" when an approval waits (sending it denies, then sends the text);
  - "Working… esc cancels" while running;
  - "Answer, or describe a change" for a question;
  - "Ask, describe a change, or /help" otherwise.
- **Key bar:**
  - No approval waiting: `esc` (cancel, accent while running) · `⇧tab` (cycle ask → accept-edits →
    auto) · `↑` (last prompt sent from this phone) · `/` (start a command) · `⏎` (send, accent when
    there's text).
  - Approval waiting: `y` (Allow) · `n` (Deny) · `esc` (Deny).

  Steer stays available while a Codex turn runs: the prompt sends `steer`, as today.

**Settings**
- **"Machines":**
  - each Mac with its status and engines;
  - "+ Pair another machine" (Phase D; in Phase A, Unpair and re-pair).
- **"Notify me when":** switches for "An action needs approval", "A turn finishes", and "A session
  hits an error" (Phase B).
- Face ID to allow, and Unpair.
- **Footer:** "Octet v<version> · talks to your Macs directly over your tailnet; nothing passes
  through a relay."

**Toasts** sit at the top for 3.4 s and can carry an action. For example: "my-project needs approval ·
View", "Paired with studio-mac", "A turn is running. esc cancels it.".

**Kept from today:** the hub client, the store and transcript model, the auth and arm guards, the
offline greying, the full-access flow, notifications and the dev-build banner.

## Phase B — safe commands, usage, error push, questions

**Protocol and bridge**
- **`command {session, name}`**, where `name` is one of `new`, `session`, `help` or `export`.
  - The bridge runs the desk's own handler and gets the desk's own refusals. The desk note names the
    device.
  - **new:** refused while a turn runs, or while the desk has an unsent draft. It starts a fresh
    conversation.
  - **session:** read-only. The ack carries the desk's session text.
  - **help:** read-only. The ack lists only the phone's commands: `/model`, `/mode`, `/new`,
    `/session`, `/export`, `/help`.
  - **export:** never takes a path. It writes to the desk's default `<workspace>/<journal name>`.
    The ack names the file.
- **`register_push {token, kinds?}`**, where `kinds` is `{approvals, errors, turns}` booleans,
  stored on the device. A missing `kinds` keeps what is stored. The default for new devices is
  approvals on, errors on, turns off.
- **Error push.** A journal `error` record pushes "Octet · Session error" to devices with errors on.
  The cap, coalescing and connected rules are unchanged; an error ranks after an approval and before
  a finished turn.

**App**
- **Usage screen** (from ⋯ or Session info):
  - **"This session":** ENGINE (engine / model), USAGE (the latest `usage` record's text, verbatim),
    TURNS (count of `finished`), JOURNAL (the newest journal's path).
  - **"Today":** each session's latest usage text, sorted by most recent activity.
  - **Buttons:** "Export journal" (`/export`) and "Fresh context" (`/new`, with a confirm step).
  - No context-% bar, because neither vendor reports context use.
- **Question detection (heuristic, labelled in code):** a session counts as a question when it is
  idle, its last turn finished, and its last `OCTET` text ends with `?`. The question disappears
  once the user sends anything.

## Phase C — Changes

**Protocol and hub.** The hub runs git in the session's workspace, which comes from its
registration and never from the phone.
- Every git call uses literal argv with `GIT_TERMINAL_PROMPT=0`. It never takes options or `-c` from
  the phone.
- Each call has a 10 s timeout, and at most 2 git calls run at once.

The commands:
- **`changes {session}` → `changes {req, branch, upstream, ahead, behind, files, more}`.**
  - Each file is `{path, add, del, binary, status}`.
  - Built from `git status --porcelain=v2 --branch -z` and `git diff --numstat -z HEAD`, with
    untracked files counted by line.
  - At most 500 files, with `more` counting the rest. Not a repository → refused, "This folder isn't
    a git repository".
- **`diff {session, path}` → `diff {req, path, text, clipped}`.**
  - `path` must be one of the paths the most recent `changes` reply listed for that session;
    otherwise it's refused.
  - The text is clipped at 64 KiB / 2,000 lines. A binary file gives "binary file".
- **`commit {session, message}` → ack "Committed <short hash>".**
  - Refused while a turn runs, when nothing has changed, or when the message is empty or over 2,000
    characters.
  - Runs `git add -A`, then `git commit -F -` with the message on stdin. The repository's hooks run.
  - Audited with the device name.

**App**
- **Changes screen:**
  - **Header:** ‹ · Changes, with "session · N files · +a −d" under it.
  - **File rows:** name, directory, +a −d, and a 5-cell `■` bar. A tap expands the diff:
    - `+` lines on the selected background with an accent sign;
    - `−` lines muted and struck through;
    - context lines plain.
  - **"Checks":** the last test-like tool call in the transcript, with ✓, ✗, or "No tests run in
    this session". It matches cargo test, npm/pnpm/yarn test, pytest, go test, jest and vitest. This
    is a heuristic, labelled in code.
  - **"Branch":** name, "N ahead of upstream" or "uncommitted changes".
  - **Commit:** an editable message, prefilled from the session's first prompt as one line of at
    most 72 characters. Face ID, then `commit`. A toast confirms, and the screen refreshes.
  - **"Open PR on the desk":** shows `gh pr create` to run at the Mac, and sends nothing.

## Phase D — more than one Mac (app only)

- **Pairings.**
  - Up to 4 Macs, each `{id, hubUrl, deviceName, macName}`, with its token in its own keychain entry
    (`WHEN_UNLOCKED_THIS_DEVICE_ONLY`).
  - Today's single pairing migrates on first launch.
  - `macName` is the first label of the hub host, and can be renamed in Settings.
- **Connections.**
  - One `HubClient` per Mac, all open in the foreground, each with its own backoff, keepalive and
    cursors.
  - All close in the background.
  - Store keys become `mac/sessionId`.
  - The offline rule applies per Mac: an unreachable Mac shows "asleep" and its sessions grey out.
- **Screens.**
  - Merged Inbox: cards name their Mac when more than one is paired.
  - The drawer groups sessions by Mac.
  - The header shows "2 Macs · 1 online".
  - The New-session chip cycles online Macs.
  - Settings › Machines has Unpair per Mac and "+ Pair another machine".
- **Push.**
  - The data gains `mac`, the hub's own published host, set by the hub. That host is already known
    to the phone.
  - Taps open `mac/session`, falling back to the Inbox.
- **Limits.**
  - At most 4 connections.
  - The 4 live session views are counted across all Macs.

## Phase E — Dynamic Island (iOS)

- **App, native.**
  - A WidgetKit / ActivityKit Live Activity extension (`targets/octet-live/`, Swift), added through
    `@bacons/apple-targets`.
  - A local Expo module `modules/octet-live` (Swift):
    - `start(state)`, `update(state)` and `end()`;
    - push tokens and the push-to-start token as events;
    - `isSupported()`.
- **Shared App Group** `group.dev.octet.mobile`. While the app runs it writes
  `session → {name, command}`. The widget reads names and commands from there only.
- **Content state:** `{kind: "running"|"approval"|"done", session, mac, since, approvals}`. This is
  the same shape whether it comes from the app or from APNs.
- **What the widget shows.**
  - **Compact:** the mascot pose for the phase, and the elapsed time since `since`.
  - **Expanded approval:** "Approval waiting", plus the name and command when the App Group knows
    them, or "N approvals waiting".
- **The Island's buttons.**
  - **Deny** is a `LiveActivityIntent` that runs in the app process and sends `answer allow:false`.
  - **Allow** opens `octet://approve/<mac>/<session>/<approval>`. The app shows that approval with
    Face ID and the arm as usual.
- **Protocol and hub.**
  - **`register_activity {token, start_token}`** is stored per device, validated (hex, at most 200
    characters), redacted and dropped on revoke.
  - **Lifecycle:** the hub starts the activity (push-to-start) when an approval waits or a turn
    starts while the phone is away, updates it on changes, and ends it 5 minutes after the sessions
    are idle.
  - **Payload:** `{"aps": {"timestamp", "event": "start"|"update"|"end", "content-state": {...},
    "attributes-type": "OctetActivity", "attributes": {}}}`. It carries no names or commands.
  - **APNs credentials:** ES256 JWT (team id, key id, `.p8` key), from `hub.json` `apns: {team_id,
    key_id, key_path, sandbox}`. The key file must be 0600.
  - **Sending:** HTTP/2 POST to `api.push.apple.com` (or `api.sandbox.push.apple.com`) through
    `curl --http2 -K -`, with the URL, headers and body on stdin. The JWT is refreshed every 50
    minutes, and nothing secret appears in argv or logs.
  - **Bounds:** the same per-device cap of 6 per minute as other pushes. A 410 response drops the token.
- **Not available here.** The Swift code is built only by EAS on the user's account. The device pass
  gains the Island steps.

## Testing

- **Each phase is test-first.**
  - App: RNTL behaviour tests for each screen and each tap that sends something.
  - Rust: tests for each new command's refusals, argv and bounds.
- **Contract test** against the real hub binary, extended with `command`, `changes`, `diff`,
  `commit` (on a temp git repo), `register_push` kinds, and `register_activity`.
- **Live end to end** (fake vendor, private tmux, temp git workspaces) and the attack suite, re-run
  after the last phase. New attack cases:
  - a diff path outside the change list;
  - a commit while running;
  - a `command` name outside the set;
  - an export path injection.
- **APNs:**
  - an ES256 known-answer test;
  - content-free payloads;
  - a stand-in curl that captures argv and stdin.
