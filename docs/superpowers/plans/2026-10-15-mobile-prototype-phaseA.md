# Octet mobile prototype, Phase A: the new UI on today's hub — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** rebuild every screen of `apps/mobile` to match the Claude Design prototype (Pair, Inbox,
drawer, New session, Conversation, Settings, toasts), on top of today's hub and protocol.

**Architecture:** the hardened core stays and only the screens change. That core is
`src/client/HubClient.ts`, `src/store.ts`, `src/model/*`, `src/hub.ts`, `src/auth.ts`,
`src/components/fullAccess.ts`, `src/components/send.ts`, `src/notifications.ts`, plus the approval
logic in `ApprovalSheet.tsx`, which gets extracted into a hook.
- **Primitives:** a small set in `src/ui/` (Card, Label, Segmented, Button, Chip, Toast) gives
  every screen the terminal look.
- **Drawer:** a custom drawer in `app/_layout.tsx` wraps the stack.
- **Pure TypeScript:** inbox classification lives in `src/inbox.ts`, testable without React.
- **One hub change:** each `sessions` row carries its front approval, so Inbox cards can answer
  approvals of sessions that are not open.

**Tech stack:** Expo SDK 57, RN 0.86, expo-router, zustand, FlashList 2, jest-expo and RNTL. No new
dependencies: the drawer gesture uses RN's own `PanResponder` and `Animated`.

**Spec:** `docs/superpowers/specs/2026-10-15-mobile-prototype-design.md`, Phase A and "Rules that
hold in every phase". **Prototype:** `docs/design/mobile-prototype/octet-mobile-prototype.dc.html`.

## Global Constraints

- **Gates.**
  - `make mobile-check` (typecheck, lint, jest) and `cd apps/mobile && npm run export:check`.
  - When Rust is touched: `make rust-check` and Linux clippy
    (`scripts/rust-env.sh cargo clippy --locked --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`).
  - `make mobile-contract` after Task 1.
  - Commit only after the gates exit 0. Run them unpiped, with output redirected to a log.
- **TDD.** RNTL behaviour tests are written first, with a captured RED run. Assert what shows and
  what a tap sends. No snapshot tests.
- **Type.** One monospace face: `Platform.select({ios: "Menlo", android: "monospace", default: "monospace"})`.
  - Body 14 / 20.
  - Labels 12 / 16, upper-case and muted.
  - Transcript text 12, 13 or 14 px (default 13), saved as `textSize` in `src/persist.ts`.
- **Colours.**
  - Theme tokens only.
  - Amber only for approvals, warnings and errors.
  - Card radius 12, with a 1 px `edge` border.
  - Touch targets at least 44 × 44.
- **Security (unchanged).**
  - Allow is once only, with **no "Session" option and no `a` key**.
  - Face ID (when the setting is on) and the 400 ms arm apply on every Allow surface.
  - The hidden-padding fold applies wherever a command shows.
  - Hub strings render as Text only.
  - Full access stays in the guarded flow (`useFullAccessArm` and `FullAccessConfirm`).
- **Performance.**
  - Memoise rows; no inline object or function props in list rows.
  - Use FlashList for lists.
  - Select narrowly from the store.
  - Keep the existing perf budgets green.
- **Copy.** Plain, active voice. Use the exact strings from the spec's Phase A section.
- **Commits.**
  - Stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`.
  - Messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **An approval answered elsewhere while its Inbox card is on screen, or while Face ID is up.**
   The card disappears and nothing is sent. Test: Task 2 `an_inbox_card_answered_elsewhere_sends_nothing`.
2. **"Allow all N once" while one of the N closes or arrives mid-way.** It answers only approvals
   still open that were shown and armed, each once. Test: Task 2 `allow_all_skips_closed_and_new`.
3. **The drawer and edge swipe stealing scroll or taps in the transcript.** The edge zone is 18 pt
   wide, and a vertical drag never opens the drawer. Test: Task 3 `a_vertical_drag_never_opens_the_drawer`.
4. **The key bar while offline, while a send is out, and while an approval waits.** Nothing sends
   offline or twice, and `esc` with an approval waiting denies exactly that approval. Test: Task 6
   `keys_follow_state_and_never_double_send`.
5. **A session with no catalog yet, opened from the drawer.** The ▾ dropdown shows "Waiting for
   the Mac…" and no models. Test: Task 5 `dropdown_without_catalog`.

---

### Task 1: The hub's `sessions` row carries the front approval (Rust + protocol)

**Files:**
- `crates/octet-hub/src/sessions.rs` (`Live::row`)
- `crates/octet-core/src/remote/protocol.rs` (the `SessionInfo` schema: optional `approval`)
- the regenerated `apps/mobile/src/protocol.ts`
- the app's `src/model/session.ts` or `src/store.ts` cleaning for the new field

**Rulings:**
- `approval` is the first entry of the bridge state's `approvals` array, as `{id, summary}`. The
  bridge already clips it, so the hub copies it as is.
- It is omitted when nothing waits.
- The app cleans it with the same cleaner it uses for state approvals.

- [ ] **Step 1 (failing tests).**
  - Rust (`sessions.rs` tests): a state frame with two approvals gives a row whose
    `approval == {"id":1,"summary":{...}}`; an empty list gives no key.
  - App (`store.test.ts`): a sessions frame with `approval` exposes it through a new hook
    `useFrontApproval(id): ApprovalInfo | null`, cleaned, so a hostile summary is cleaned the same
    as in state.
- [ ] **Step 2.** Implement. Regenerate with
  `OCTET_WRITE_PROTOCOL=1 scripts/rust-env.sh cargo test -p octet-core the_checked_in`.
- [ ] **Step 3.** Run `make rust-check`, Linux clippy and `make mobile-check`. Commit: "Hub: a sessions row carries its front approval".

### Task 2: The look, the primitives, and the Inbox

**Files:**
- Create:
  - `src/ui/{Card,Label,Segmented,Button,Chip,Toast}.tsx` and `src/ui/index.ts`;
  - `src/toast.ts` (a store slice `toast: {text, action?, label?} | null`, plus `showToast(text, action?, label?)` with a 3.4 s timeout);
  - `src/inbox.ts` (pure);
  - `src/approvals.ts` (the `useApprovalAnswer` hook, extracted from `ApprovalSheet.tsx`);
  - `src/components/InboxCard.tsx`;
  - the rewritten `app/index.tsx` (Inbox).
- Modify:
  - `src/theme.ts` (adds `type.body` 14/20 mono, `type.label` 12/16 mono, `radius.card = 12`; the
    existing exports keep working);
  - `src/persist.ts` (`textSize`);
  - `ApprovalSheet.tsx`, which uses the extracted hook. Its behaviour is unchanged and its tests stay
    green.
- Delete: `src/components/SessionCard.tsx`, once nothing uses it.

**Interfaces:**
- `classify(sessions: SessionInfo[], fronts: Map<string, ApprovalInfo|null>): {needs: InboxItem[], running: InboxItem[], done: InboxItem[]}`.
  - `InboxItem = {id, name, ago, status, statusTone: "amber"|"accent"|"muted", line, approval: ApprovalInfo|null, editApproval: boolean}`.
  - Needs = `approvals > 0`. Running = phase `running`. Done = everything else.
  - Each list is sorted by `last_activity`, newest first.
  - The status line:
    - approval: `● <action> · <command or first file>`;
    - running: `● <PHASE WORD upper> · <engine>`;
    - else: `<mac> · <engine>`, with `mac` taken from the hub host's first label.
  - The excerpt line:
    - approval: `summary.scope`, or the cwd;
    - running: `now`;
    - else: the goal, or "idle".
  - `editApproval` = the summary has files and no command.
- `useApprovalAnswer(session, approval)` returns `{armed, busy, failure, allow(): Promise<void>, deny(): Promise<void>}`.
  - It carries over every existing ApprovalSheet guard:
    - the 400 ms arm, re-armed on a summary change and on offline→online;
    - Face ID when the setting is on;
    - the generation and alive checks after the await;
    - no double send;
    - the open-id check.
- `allowAll(items, answerOnce)`: one `confirmBiometric` for the batch, then each still-open,
  still-armed approval answered in order.

**Screen** (prototype lines 63–105):
- **Header:** ☰ · "Inbox" with "● <mac> · online|offline" under it · +.
- **Tabs:** "Needs you N", "Running N", "Done".
- **Cards:** approval cards have a panel background and the Allow once / Deny (or Review) row.
- **Bottom button:** "Allow all N once" when needs ≥ 2 approvals, else "New session". New session
  navigates to `/new`, which Task 4 adds; until then point it at `/recent`.
- **Empty states:** the success mascot (59 px wide) with "Nothing needs you.", or the sleeping one
  with "Nothing running." / "No finished sessions.".
- **Offline:** the existing grey and no actions.

- [ ] **Step 1 (failing tests).**
  - `inbox.test.ts`: classification, sorting and the status and excerpt rules.
  - `inboxScreen.test.tsx`:
    - tabs and counts;
    - Allow once sends `answer allow:true` after Face ID and the arm, and never before 400 ms;
    - Deny sends `allow:false` with no Face ID;
    - Review opens `/session/<id>`;
    - `an_inbox_card_answered_elsewhere_sends_nothing`: the front approval changes during Face ID,
      and nothing is sent;
    - `allow_all_skips_closed_and_new`: one Face ID; a closed one is skipped; an approval that
      arrived after the tap isn't answered;
    - the empty states;
    - offline greying.
  - `ui.test.tsx`: Card's notched title, Segmented's selection, Button's 44 pt hit area, and Toast's
    auto-hide and action.
- [ ] **Step 2.** Implement. Move the guards from ApprovalSheet into `useApprovalAnswer` without
  changing behaviour; the existing ApprovalSheet tests must pass untouched.
- [ ] **Step 3.** Run `make mobile-check` and `export:check`. Commit: "App: the terminal look, the primitives and the Inbox".

### Task 3: The drawer and navigation shell

**Files:** create `src/components/Drawer.tsx` and `src/drawer.ts` (open-state store slice). Modify
`app/_layout.tsx`.

**Rulings:**
- **Size and motion.** The drawer is 304 pt wide, over a 0.6-opacity ink scrim. It opens with
  `Animated.timing` over 180 ms, or instantly with reduced motion.
- **Contents:**
  - "**Octet**" with "N need you · M running" or "M running" under it, and a + button (new session);
  - **Inbox**, with an amber badge showing the needs count;
  - the Mac's group label, "● <mac> · online" or "○ <mac> · offline", then its sessions sorted like
    the Inbox. Each session's status word is approve (amber), a phase word (accent), done (accent),
    or a time ago (muted).
  - **Settings**.
- **Gestures.**
  - An 18 pt edge zone on Inbox, Conversation and Settings opens the drawer on a horizontal drag
    of more than 30 pt, when the horizontal movement is at least twice the vertical.
  - A drag left of more than 40 pt on the drawer closes it, and so does a scrim tap.
  - Android back closes it.
- **Hidden.** The drawer is closed and disabled on Pair and New.

- [ ] **Step 1 (failing tests).** `drawer.test.tsx`:
  - opens from ☰ and closes on the scrim;
  - the badge count;
  - a tap on a session opens it and closes the drawer;
  - Settings navigates;
  - `a_vertical_drag_never_opens_the_drawer`, with simulated PanResponder moves;
  - a horizontal drag opens it;
  - back closes it.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the gates. Commit: "App: the drawer".

### Task 4: New session

**Files:** create `app/new.tsx` and `src/components/NewChips.tsx`. Modify `app/_layout.tsx` to
register `new`. Reuse the start and launch-watch logic and `useFullAccessArm` from
`NewForm.tsx` and `app/recent.tsx`; extract shared pieces rather than copying them.

**Rulings** (prototype lines 107–124):
- **Header:** Cancel · "New session" · "Start ↑", which is accent with text and muted without.
- **Field:** a full-screen multiline field, "What should the agent do?".
- **Chips:**
  - Mac (a single Mac in Phase A; a tap shows the toast "Sessions start on <mac>.");
  - folder (cycles the offered workspaces, as NewForm does);
  - "engine / model" (cycles claude and codex, with each engine's first catalog model, or "default");
  - mode (cycles ask → accept-edits → auto). Long-press opens the guarded full-access flow, and once
    it is armed the chip reads "full access" in amber.
- **Start:**
  - Sends `start {engine, workspace, model?, mode?}`, the same as NewForm, with the full-access
    confirm only when armed.
  - On the ack, it waits for the launched session as recent.tsx does.
  - It then sends `prompt {session, text}` with the draft, and navigates to `/session/<id>` with
    `router.replace`.
  - Empty text shows the toast "Write a prompt first.".
  - Refusals show as a toast with the hub's words.
- **Resume:** a muted "Resume a past session ›" row under the chips opens the existing
  `/recent` list, which stays for resume only.

- [ ] **Step 1 (failing tests).** `newScreen.test.tsx`:
  - chips cycle;
  - Start with empty text shows the toast;
  - Start sends `start` and then, when the session appears, `prompt` with the text, and opens the
    conversation;
  - long-press full access needs Face ID and the typed words;
  - a refusal shows the toast;
  - offline disables Start.
- [ ] **Step 2.** Implement, and point the Inbox's New session at `/new`.
- [ ] **Step 3.** Run the gates. Commit: "App: New session with chips".

### Task 5: The conversation — header, dropdown and transcript

**Files:** create `src/components/ConvoHeader.tsx`, `src/components/ModelModeMenu.tsx` and
`src/components/TranscriptHeader.tsx`. Modify `app/session/[id].tsx` and
`src/components/TranscriptRow.tsx` (labels).

**Rulings** (prototype lines 126–169):
- **Header:** ☰ · "<name> ▾" with "<model> · <mode>" under it · "Aa" · "⋯".
- **▾ menu:**
  - "MODEL · <engine>": rows for the catalog's models of the session's engine, with ✓ on the
    current one. A tap sends `set_model` through the existing send path; the provider-switch
    confirm stays as in ModeModelSheet.
  - "PERMISSION MODE": a Segmented control for ask · accept-edits · auto, sending `set_mode`, with
    the spec's help line under it.
  - "Full access…": opens the existing guarded flow.
  - "Session info ›" with the first 6 characters of the session id, which opens the info sheet.
  - Without a catalog: "Waiting for the Mac…" (`dropdown_without_catalog`).
  - Refusals show in the menu, in fg.
- **Aa** cycles `textSize` 12 → 13 → 14 → 12 and saves it. The transcript re-renders, but the
  header doesn't.
- **⋯** opens a Session info sheet: engine / model, mode, workspace path, and the session id. Phase B
  turns it into Usage.
- **Transcript header:**
  - the mascot pose (44 px, existing `poseOf`);
  - "**Octet** v<app version> · <mac>";
  - "**<mode>** · <engine> / <model>", with the mode in accent unless it's ask;
  - "<PHASE> · <workspace>", with the phase in accent, or amber for approval.
- **Entries.** `TranscriptRow` shows a bold label line (YOU accent, OCTET fg, TOOL muted, NOTE muted,
  ERROR amber) and then the body. Grouping, Markdown and the bounds are unchanged, and so are the
  perf budgets.

- [ ] **Step 1 (failing tests).**
  - `convoHeader.test.tsx`:
    - the title and subtitle;
    - the menu lists models with ✓;
    - a model tap sends `set_model`;
    - the Segmented sends `set_mode` and shows the help text;
    - full access opens the guarded flow;
    - `dropdown_without_catalog`;
    - Aa cycles and persists.
  - `transcriptRow.test.tsx`: each label and colour.
- [ ] **Step 2.** Implement, and replace the old ModeModelSheet entry point. Keep ModeModelSheet's
  full-access pieces in use and delete the rest if nothing uses it.
- [ ] **Step 3.** Run the gates, including perf. Commit: "App: the conversation header, model and mode menu, and labelled transcript".

### Task 6: The conversation — permission card, prompt line, slash menu and key bar

**Files:** create `src/components/PermissionCard.tsx`, `src/components/PromptLine.tsx`,
`src/components/SlashMenu.tsx` and `src/components/KeyBar.tsx`. Modify `app/session/[id].tsx`.
Retire `Composer.tsx` and `SessionControls.tsx`, keeping their tests' behaviours by moving them to
the new components.

**Rulings** (prototype lines 170–201):
- **Permission card.**
  - An amber border, with "**Permission required** #<id>" notched into it.
  - Rows: TOOL (action), CMD or FILE (command, folded with `foldCommand` and the shortened marker,
    or the files), and REASON (scope or cwd).
  - Allow (accent) and Deny, using `useApprovalAnswer`.
  - Queued approvals show "N more waiting".
- **Prompt line.**
  - `>` plus a single-line field, with the state placeholders from the spec.
  - Sending while an approval waits sends `answer allow:false` for the front approval, then
    `prompt` with the text. Both go through the existing send path, and the second only after the
    first acks.
  - While a Codex turn runs it sends `steer`; while a Claude turn runs, `prompt` (it queues).
  - The 64 KiB limit and the desk slash rule (`isSlashCommand`) are unchanged.
- **Slash menu.**
  - Shown while the text starts with `/` and has no space.
  - Rows: `/model` (opens the ▾ menu) and `/mode` (cycles the mode like ⇧tab).
  - The top match is marked ▸. Tab completes it, Enter runs it.
  - Other `/` text sent gives the existing note.
- **Key bar.**
  - No approval waiting: esc (`cancel` while running, accent; inert otherwise) · ⇧tab (cycle
    ask → accept-edits → auto, sending `set_mode`; never full access) · ↑ (the last text sent from
    this phone for this session, kept in memory, up to 20) · / (sets the text to `/`) · ⏎ (send,
    accent with text).
  - Approval waiting: y (Allow) · n (Deny) · esc (Deny).
  - Every key goes through the same guards as its button: offline disables them all, and a send in
    flight blocks a second.
- **Kept from today's controls:** End (in the ⋯ sheet, with confirm, when idle) and Steer. They keep
  their old behaviour.

- [ ] **Step 1 (failing tests).**
  - `permissionCard.test.tsx`: the rows, the fold, Allow and Deny guards, and queued.
  - `promptLine.test.tsx`: the placeholders; sending denies then prompts; steer on Codex; slash.
  - `keyBar.test.tsx`: `keys_follow_state_and_never_double_send`, covering offline, a send in
    flight and pending esc denying the front approval; ⇧tab cycles and skips full access; ↑ history.
  - `slashMenu.test.tsx`.
  - Port the Composer and SessionControls tests.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the gates. Commit: "App: the permission card, prompt line, slash menu and key bar".

### Task 7: Pair, Settings, toasts and clean-up

**Files:** modify `app/pair.tsx`, `app/settings.tsx`, `app/_layout.tsx` (Toast host) and
`app/session/[id].tsx` (approval toasts). Update `apps/mobile/README.md` and `CHANGELOG.md`.

**Rulings** (prototype lines 42–61 and 303–329):
- **Pair.**
  - A panel background with "Scan the code from **octet pair**".
  - The camera fills the area inside four accent corner brackets (240 pt).
  - A bottom sheet: "PAIRED", each paired Mac (in Phase A, the current one if re-pairing), and
    "Enter code instead". That reveals the existing paste and name fields. The confirm step and
    every existing pairing rule are unchanged.
- **Settings.**
  - Cards: "Machines" (the Mac, with ● online / ○ offline and Unpair), "Notify me when" (switches
    for "An action needs approval" and "A turn finishes"), and "Face ID to allow".
  - Footer: "Octet v<version> · talks to your Macs directly over your tailnet; nothing passes
    through a relay."
  - The existing behaviours are unchanged: the notification switch's ack rules, and the Face ID
    switch's biometric-off rule.
  - The "Notify me when" switches map onto today's single notifications setting until Phase B adds
    kinds. Both read the same value, and the turn switch is disabled with "Coming with the next hub
    update".
- **Toasts.**
  - When an approval arrives for a session other than the one on screen (Inbox excluded): "<name>
    needs approval · View", which opens it.
  - "Paired with <mac>" after pairing.
- **Clean-up.** Delete unused components and tests, and update the README's screenshots section to
  describe the new screens.

- [ ] **Step 1 (failing tests).**
  - `pairScreen` and `settingsScreen` updates for the new layout, keeping every existing behaviour
    test.
  - `toast.test.tsx` for the approval toast: not shown for the session on screen.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run all gates, plus `make mobile-contract`. Commit: "App: Pair, Settings and toasts in the new look".
