# Octet mobile prototype, Phase B: safe commands, usage, error push, questions — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the phone can run a fixed safe list of desk commands (`/new`, `/session`, `/help`,
`/export`), see usage, get an error push, and see sessions that ended on a question. This adds the
hub and bridge APIs those features need.

**Architecture:**
- **Bridge:** a typed `command {session, name}` runs the desk's own handlers through the existing
  desk-guarded path.
- **Hub:** while tailing journals it keeps, per session, the latest `usage` text, an error count,
  and whether the last finished turn ended on a question. These go out on the `sessions` row (for
  the Inbox and Today) and in the pusher's snapshot (for the error push).
- **Push kinds:** `register_push` gains per-device `kinds`.
- **App:** the slash menu, the Usage screen, the Settings switches and question cards.

**Tech Stack:** Rust (octet-core protocol, octet-tui bridge, octet-hub), Expo app (as Phase A).

**Spec:** `docs/superpowers/specs/2026-10-15-mobile-prototype-design.md` (Phase B, plus "Rules that hold
in every phase").

## Global Constraints

- **Gates.**
  - Rust tasks: `make rust-check`, Linux clippy, and Linux Docker for the touched crates (`--init`).
  - App tasks: `make mobile-check` and `npm run export:check`.
  - The last task also runs `make mobile-contract` after `cargo build -p octet`.
  - Commit only after the gates exit 0, run unpiped.
  - Regenerate `protocol.ts` with
    `OCTET_WRITE_PROTOCOL=1 scripts/rust-env.sh cargo test -p octet-core the_checked_in`.
- **Every new phone command:**
  - is a closed set, refused before hello, rate-limited and audited with the device name;
  - never takes free text that reaches a shell or a path.
- **`export` never takes a path.** It writes the desk's default `<workspace>/<journal file name>`.
- **Pushes are content-free.** The error push body is exactly "Session error". The title is
  "Octet", and the data is `{session}` (plus `mac` from Phase D).
- **Question detection is a labelled heuristic.** The last finished turn's final reply text,
  trimmed, ends with `?`. It clears on the next `user` record.
- **Phase A's rules still hold:** Allow once only, the arm, Face ID, hub text rendered as Text only,
  and the performance budgets.
- **Commits:** stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`. End
  each commit message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **`/new` while the desk is editing a prompt, running `!`, has a draft, or a turn is open.**
   It is refused with the desk's words, and nothing exits. Test: Task 1 `new_is_refused_like_the_desk`.
2. **`/export` on a workspace that isn't writable, or that already holds a file of that name.** The
   desk's own export job answers, with the same overwrite and error behaviour as the desk, and the
   phone shows its words. Test: Task 1 `export_answers_with_the_desk_words`.
3. **A storm of error records**, for example a vendor crash loop. Pushes stay within the cap of 6
   per device per minute, one per coalescing window, and an approval still beats an error. Test:
   Task 2 `errors_push_once_per_window_and_rank_after_approvals`.
4. **A huge final reply or a reply streamed in 10,000 chunks.** Question detection costs O(1) per
   chunk and keeps at most 256 characters per session. Test: Task 2 `question_tail_is_bounded`.
5. **`register_push` from an older app with no `kinds`.** It keeps the stored kinds, and new devices
   default to approvals and errors on, turns off. Test: Task 2 `kinds_default_and_persist`.

---

### Task 1: The `command` phone command (Rust: protocol, hub, bridge)

**Files:**
- `crates/octet-core/src/remote/protocol.rs`: `PhoneCommand::Command {session, name: CommandName}`.
  `CommandName` is the closed enum `New | Session | Help | Export`, parsed from
  `"new" | "session" | "help" | "export"`. It maps to `BridgeCommand::Command {name}`.
- `crates/octet-hub/src/server.rs`: the `bridge_command` mapping, and an audit line
  `command <name>`.
- `crates/octet-tui/src/bridge.rs` `run_remote`. Every arm leaves the desk note "<what> from
  <device>".
  - **new:** refused while `desk` is busy (the existing editor and `!` refusals), while a turn is
    open (`TURN_OPEN`), or while the desk has a draft (`DESK_DRAFT`). Otherwise it sets
    `Action::Exit(Exit::New)` and acks "Starting a fresh conversation".
  - **session:** acks with exactly the desk's `/session` text, built by the same function. Extract
    it from `commands.rs` `Cmd::Session` into `session_text(app)`.
  - **help:** acks with this text:
    "Commands from the phone: /model · /mode · /new · /session · /export · /help. Everything else runs
    at the desk."
  - **export:** starts the desk's `export_job(app, "")`, and acks "Exporting the journal to <path>".
    The job's outcome reaches the desk as today.
- `apps/mobile/src/protocol.ts`: regenerated.

- [ ] **Step 1: failing tests.**
  - Parser: each name is accepted; an unknown name, a path or extra fields are refused as
    `invalid`.
  - Hub: before hello the command is refused as `unpaired`; it is audited; an unknown session gives
    `unavailable`.
  - Bridge:
    - `new_is_refused_like_the_desk`: editor, `!`, draft and turn-open each give the desk's words
      and no exit; idle exits New;
    - session text equals the desk's text;
    - the help text;
    - `export_answers_with_the_desk_words`: the default path, and no argument accepted;
    - each command leaves a desk note naming the device.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the Rust gates and `make mobile-check` (for `protocol.ts`). Commit: "Phone commands: /new, /session, /help and /export through the desk's own handlers".

### Task 2: Hub session facts and per-kind push (Rust)

**Files:**
- `crates/octet-hub/src/sessions.rs`. `Live` tracks three facts from the records it tails:
  - `usage`: the latest `usage` record's text, clipped to 200 characters;
  - `errors`: a count of `error` records seen since the hub first saw the session;
  - `question`: the trimmed tail (at most 256 characters) of the reply text since the last `user`
    record. At `finished`, the row's `question` is set to `tail.ends_with('?')`. It is cleared at
    the next `user` record.

  On first sight of a session, it seeds `usage` and `question` from the newest journal's last
  256 KiB, read once with a bounded read.

  The row gains `usage?: string` and `question?: bool` (true only), and `Snapshot` gains `errors`.
- `crates/octet-hub/src/push.rs`: `Kind::Error` ("Session error", priority "high", channel
  "errors").
  - Trigger: a session's `errors` count rises after seeding.
  - Ranking: Approval > Error > Turn.
  - Per-device kinds filter what gets sent.
- `crates/octet-core/src/remote/protocol.rs`: `register_push {token, kinds?: {approvals, errors, turns}}`
  (`opt` object). Also the SessionInfo additions.
- `crates/octet-hub/src/store.rs`: `Device.push_kinds`, read and written with backward-compatible
  defaults (approvals on, errors on, turns off). A `register_push` without `kinds` keeps the stored
  value.

- [ ] **Step 1: failing tests.**
  - `question_tail_is_bounded`: 10,000 chunks, the tail is at most 256 characters, and it's O(1) per
    chunk (no re-scan).
  - A reply ending "?" at `finished` gives `question: true`; the next `user` clears it.
  - `usage` is latest-wins and clipped.
  - The seed reads at most 256 KiB.
  - `errors_push_once_per_window_and_rank_after_approvals`.
  - `kinds_default_and_persist`: an older `devices.json` loads with the defaults; kinds round-trip;
    a turn push is filtered when `turns` is off.
  - The error push body is content-free.
- [ ] **Step 2.** Implement, regenerate `protocol.ts`, and add the app's cleaning for the new row
  fields: `usage` with `field` at 200 characters, `question` boolean only.
- [ ] **Step 3.** Run the Rust gates, Linux Docker and `make mobile-check`. Commit: "Hub: sessions report usage, errors and questions; push kinds per device".

### Task 3: App — the slash commands and the Usage screen

**Files:**
- `src/components/SlashMenu.tsx` and `PromptLine.tsx`: rows for `/new`, `/session`, `/export` and
  `/help`.
- Create `src/components/CommandSheet.tsx`, which shows an ack's text, monospace, selectable.
- Create `app/usage.tsx`.
- `src/components/ConvoHeader.tsx`: ⋯ opens Usage, and Session info stays in the ▾ menu.
- `src/store.ts`: `useSessionUsage`, `useTurns(id)` (count of `finished` in the view), and `journals`.

**Rulings:**
- **`/new`:** a confirm sheet ("Start a fresh conversation? The current one stays in the journal.",
  with Cancel and Start fresh), then `command new`. On the ack the conversation shows the new
  transcript as the hub replays it. A refusal shows its words.
- **`/session` and `/help`:** send `command`, and the ack's text opens in CommandSheet.
- **`/export`:** send `command export`, and the ack shows as a toast.
- Each row is disabled while offline, while an approval waits, or while a command is out.
- **Usage screen** (prototype lines 250-282):
  - Header: ‹ · Usage, with "<name> · <short id>" under it.
  - "This session": ENGINE, USAGE (the latest text from the view, or the row's `usage`, or "No usage
    reported yet"), TURNS, and JOURNAL (the newest journal file name).
  - "Today": every listed session's row `usage`, by most recent activity, each line "<name>
    <usage>".
  - Buttons: "Export journal /export" and "Fresh context /new" (with the same confirm).
  - No context-% bar.

- [ ] **Step 1: failing tests.**
  - The slash menu lists the 6 rows.
  - `/new` confirms, then sends.
  - `/session` opens the sheet with the ack text.
  - `/export` toasts.
  - Each is disabled offline, while waiting, or while busy.
  - The Usage screen's rows, Today's order, and its buttons.
  - Hub text renders as Text only.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the gates. Commit: "App: /new, /session, /export and /help from the phone; the Usage screen".

### Task 4: App — notification kinds, question cards, and Phase A leftovers

**Files:**
- `app/settings.tsx` and `src/hub.ts`. Three switches ("An action needs approval", "A turn
  finishes", "A session hits an error") send `register_push {token, kinds}`. The existing rules
  stay: the switch changes on the ack only, and stays put offline. The master token registration
  stays as is.
- `src/inbox.ts` and `InboxCard.tsx`. A row with `question: true` and no approval goes in Needs
  you, as "● question", with the line "Ended on a question" and a **Reply** button. Reply opens the
  conversation with the prompt focused and the placeholder "Answer, or describe a change".
  Sending anything clears the question locally until the next row.

**Phase A leftovers** (from its final review):
- The "Wait a moment" note clears once armed.
- Inbox card taps go through `openSession`.
- A reduced edit approval is SHORTENED on the Permission card: files of `["+N more"]` with none
  shown count as cut.
- The PromptLine doc comment sits on the right constant.

- [ ] **Step 1: failing tests.**
  - Each switch sends its kinds; offline leaves it unchanged; a refusal reverts it.
  - A question row lands in Needs you, Reply focuses the prompt, and it clears after a send.
  - Each leftover has its own test.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the gates, then `cargo build -p octet` and `make mobile-contract`, extending
  the contract test with `command session` and `register_push` kinds. Commit: "App: per-kind notifications, question cards".
