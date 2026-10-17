# Octet mobile prototype, Phase D: more than one Mac — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the phone pairs with up to 4 Macs, keeps them all connected in the foreground, and shows their sessions merged in the Inbox and drawer. Every action goes to the Mac that owns its session.

**Architecture:**
- **One zustand store.** Every session is keyed `"<mac>/<session>"`, where `<mac>` is the phone's id for the pairing. Per-Mac link state and session lists sit inside it.
- **Feeding the store.** `connectStore` takes a `mac` and rewrites ids on the way in. A send helper resolves a key to its Mac's `HubClient` and its raw id on the way out.
- **Coalescing.** One coalescer keeps the store to one commit per 50 ms across all Macs.
- **The fleet.** A `Fleet` controller owns up to 4 `MacLink`s. Each `MacLink` holds today's single-Mac controller logic.
- **The hub** adds only `mac`, its own published host, to push data.

**Tech Stack:** the Expo app (as in Phases A–C) and Rust (octet-hub push).

**Spec:** `docs/superpowers/specs/2026-10-15-mobile-prototype-design.md`, the "Phase D" section plus "Rules that hold in every phase".

## Global Constraints

- **Gates**
  - App tasks: `make mobile-check` and `npm run export:check`.
  - The Rust task: `make rust-check`, Linux clippy and Linux Docker for octet-hub (`--init`).
  - The last task: `scripts/rust-env.sh cargo build -p octet`, then `make mobile-contract`.
  - Commit only on exit 0, with gates run unpiped. Never use bare `git stash`.
- **Pairings**
  - At most 4. Each is `{id, hubUrl, deviceName, macName}`.
  - `id` is made by the phone: `"m"` followed by 8 lowercase hex characters from a CSPRNG (`expo-crypto` `getRandomBytes`).
  - Each token sits in its own keychain entry, `octet.token.<id>` (`octet.token.dev.<id>` in dev builds), with `WHEN_UNLOCKED_THIS_DEVICE_ONLY`.
  - `macName` defaults to the first DNS label of the hub host. The user can rename it (1–40 characters, cleaned with `field`).
  - Pairing a hub whose host matches an existing pairing replaces that pairing and keeps its `id`.
  - A 5th pairing is refused with "Unpair a Mac first (4 at most)".
- **Migration**, on first launch of this version.
  - Today's `Settings.hubUrl` and `octet.token` become pairing 1, and the token moves to its new keychain entry.
  - The old entry is deleted only after the new one is saved and read back.
  - The migration is idempotent. If the phone dies part-way, the next launch finishes the move.
- **Keys**
  - A session key is `` `${mac}/${session}` ``, and the raw hub id never appears in a route or store key.
  - Every send to a Mac strips the prefix. A frame from Mac `m` can only create or change keys that start with `m/`.
- **Connections**
  - One `HubClient` per Mac, each with its own backoff, keepalive and cursors.
  - All are open in the foreground and closed in the background.
  - At most 4.
- **Offline is per Mac.** A Mac that isn't `ready` shows "asleep", and its sessions grey out with their actions disabled. Other Macs keep working.
- **Budgets**
  - One store commit per 50 ms while streaming, across all Macs.
  - The 4 live session views are counted across all Macs.
  - Inbox ordering and tab counts are O(sessions) per batch.
  - The existing `src/perf*` tests stay green.
- **Push**
  - The hub adds `"mac": "<published host>"` to every push's `data` when it knows its published URL, and leaves it out otherwise.
  - The body stays content-free.
  - The phone maps `mac` to the pairing whose `hubUrl` host matches, case-insensitively. With a single pairing and no `mac`, that pairing is used. Otherwise a tap opens the Inbox.
- **Phases A–C still hold:** Face ID, the 400 ms arm, Text-only hub strings, Allow once only.
- **Commits:** stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`. Use the trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Two Macs report the same raw session id** (ids are `{pid}-{started}`). They get two keys, two views and two approvals. An Allow or prompt reaches only the owning Mac. Test: Task 1 `same_raw_id_on_two_macs`.
2. **One Mac asleep while another streams.** The asleep Mac's rows grey out and its actions are disabled. The other Mac's Allow, prompt and commit still work. The header reads "2 Macs · 1 online". Test: Task 3 `one_mac_asleep`.
3. **A migration interrupted at each step,** or with an unreadable old token. The next launch ends with exactly one pairing holding the token, or with Pair shown. Nothing is left in the old entry once the move is confirmed. Test: Task 2 `migration_is_idempotent`.
4. **Unpairing one Mac** while its conversation, Changes or Usage screen is open, or while its approval toast shows. Its keys, views, drafts and toasts are dropped. Screens showing its sessions go back to the Inbox. Other Macs are untouched. Test: Task 2 `unpair_one_mac`, and Task 4 for the screens.
5. **A push tap naming an unknown host,** a host that matches no pairing, or a session that has ended. It falls back to the Inbox, and nothing is sent. Test: Task 4 `tap_routes_by_mac`.

---

### Task 1: The store keyed by Mac (app)

**Files:**
- Modify `apps/mobile/src/store.ts`:
  - `OctetState.sessions` holds the merged list, and each item's `id` is the key. Add a client-side `mac` field: export a type `ListedSession = SessionInfo & { mac: string; raw: string }`.
  - `hubs: ReadonlyMap<string, HubState>` replaces the single `hub`. Add a derived `hub` selector that gives the best state across Macs, for the screens that keep it.
  - `applyBatch(mac, frames)`: a `sessions` frame replaces only that Mac's slice.
  - `reset(mac?)`: no argument resets everything.
  - `listed` becomes per Mac.
- Modify the functions in `store.ts`:
  - `connectStore(store, client, {mac})` rewrites every frame's session to the key.
  - `watch`, `unwatch` and `releaseSession` take a key and resolve its client through a `Links` registry, `{ client(mac): HubClient | null }`.
- Create `apps/mobile/src/macs.ts`. It is pure and holds:
  - `keyOf(mac, raw)`;
  - `splitKey(key)`, which gives `{mac, raw}` or null;
  - `macOfHost(pairings, host)`;
  - `macNameOf(url)`.
- Create `apps/mobile/src/coalesce.ts`, a cross-client batch coalescer.
  - Each `connectStore` hands its tagged batch to one shared coalescer. It commits at once when no commit happened in the last 50 ms; otherwise it commits at the 50 ms mark.
  - All pending Macs go into one `set`, in arrival order.
- Modify `apps/mobile/src/components/send.ts`: `useSend(key?)` resolves the key's Mac client and replaces `body.session` with the raw id. With no key it uses the only Mac, or refuses as `unsent` "Choose a Mac" when there are several.
- Update every send site to pass its key:
  - `app/index.tsx`, `app/new.tsx`, `app/changes.tsx`, `app/usage.tsx`, `app/session/[id].tsx`;
  - `src/approvals.ts`, `PromptLine`, `CommitSheet`, `deskCommand`, `ModelModeMenu`.
- Check `navigation.ts`. Keys hold `/`, so the route param must round-trip, and `isSessionPath` must match the encoded form.

**Rulings:**
- **Single Mac.** In this task the app still has exactly one Mac, with id `"m0"` until Task 2 provides real ids. Every existing test must pass with keys, updated only where it asserts a raw id in a route or the store.
- **Raw ids** appear only inside `HubClient` calls and wire frames.
- **Unknown sessions.** A `refused` or `subscribe failed` for a raw id on Mac `m` affects only `m/<id>`.

- [ ] **Step 1 (failing tests):**
  - `macs.ts`: round-trips, including a raw id containing `/`; hosts compared case-insensitively; `macName` from `wss://studio.tail1.ts.net/…` gives `studio`.
  - `same_raw_id_on_two_macs`: two fake clients, `m1` and `m2`, both list `123-456`. There are two rows and two views. An answer sent on `m2/123-456` goes out only on `m2`'s socket, with session `123-456`.
  - A `sessions` frame from `m1` leaves `m2`'s rows untouched (same identity).
  - The coalescer: two Macs streaming 20 batches each within 200 ms give at most 5 commits. A lone batch commits at once.
  - The view budget counts views across Macs: with 4 live views, a 5th on the other Mac evicts the oldest.
  - `reset("m1")` keeps `m2`.
  - Route round-trip: `openSession` with a key opens `/session/<encoded>`, and the screen reads the key back.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run `make mobile-check` and `npm run export:check`. Commit: "App: sessions keyed by Mac".

### Task 2: Pairings and the Fleet (app)

**Files:**
- Modify `apps/mobile/src/persist.ts`:
  - `Settings.pairings: Pairing[]` (at most 4, cleaned on load, duplicates by id dropped).
  - `hubUrl` is read for migration only.
  - Push settings (`notifications`, `pushKinds`) stay global: one choice for every Mac.
- Modify `apps/mobile/src/hub.ts`, splitting it into:
  - `MacLink`: one pairing's client, token store, pairing flow, unpair, push sync and notice. This is today's `HubController` logic.
  - `Fleet`: `links: MacLink[]`, `init()` (load, migrate, start), `setForeground`, `pair(link, name)` (adds or replaces by host), `unpair(mac)`, `rename(mac, name)`, and the push on/off and `setPushKind` across every link.
  - Keep the `HubContext` and hook names working:
    - `useHubSnapshot()` becomes `useFleet()`, `{status, macs: MacInfo[], offer, notices}`.
    `MacInfo` is `{id, macName, host, state: HubState, tried: boolean}`; `tried` turns true after
    the link's first connection attempt ends, which Task 3's "asleep" needs.
  - Task 1's fixed `"m0"` goes: migration gives pairing 1 a fresh id (store state is in memory
    only, so nothing persisted is keyed by `"m0"`).
    - `useHubClient()` stays for the one-Mac case, and is deprecated in a comment.
- Modify `apps/mobile/app/_layout.tsx` (init and foreground) and `app/pair.tsx` (pairing adds a Mac; "Pair another machine" mode).

**Rulings:**
- **Push on or off** applies to every Mac in turn, queued as today. A Mac that fails gives a notice naming its `macName`, and the switch reflects the result from the Macs that are reachable.
- **Unpair a Mac.**
  - The phone tells it "notifications off" when on, then waits the same goodbye of 2 s, then revokes and deletes its keychain entry.
  - Then it runs `store.reset(mac)` and drops that Mac's drafts, sent history and toasts.
  - Unpairing the last Mac shows Pair.
- **The Mac unpaired the phone.** When a Mac revokes this phone (the `unpaired` close), only that pairing is removed. The notice names the Mac.
- **Pairing a 5th Mac** is refused before any network call.

- [ ] **Step 1 (failing tests):**
  - `migration_is_idempotent`, with a stub keychain that fails at each step in turn: the old token is copied and read back before the old entry is deleted, and the next init completes the move.
  - Pairing adds a second Mac; pairing the same host replaces it and keeps the id; a 5th is refused.
  - Foreground and background open and close every link.
  - `unpair_one_mac`: the other Mac's link, keys and views stay. Its token is deleted, and push goodbye is sent first when notifications are on.
  - Rename is cleaned and persisted.
  - Push on reaches every Mac. One Mac failing gives a notice naming it.
  - The Mac-side unpair removes only that pairing.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run the gates. Commit: "App: up to four Macs, all connected".

### Task 3: Merged screens (app)

**Files:**
- Modify:
  - `src/inbox.ts` and `InboxCard.tsx`: cards name their Mac (`macName`, muted) when more than one is paired.
  - `src/drawer.ts` and `Drawer.tsx`: sessions grouped by Mac. Each group has a header showing `macName` and "asleep" when the Mac isn't ready, and its rows are greyed and disabled.
  - The Inbox header: "2 Macs · 1 online" when more than one is paired, and today's line otherwise.
  - `app/new.tsx` and `src/start.ts`: a Mac chip cycles the online Macs, and `start` goes to the chosen Mac. With one Mac there is no chip.
  - `app/recent.tsx` and `src/recent.ts`: recent sessions from every online Mac, merged by time and labelled by Mac when there are several. Resuming goes to that Mac.
  - The per-session offline checks move from the global `hub` to the session's Mac, in the Conversation, Changes, Usage, ApprovalToasts and Inbox Allow.

**Rulings:**
- **"asleep"** is shown for a Mac whose link is `connecting` or `offline` after its first failed attempt. A Mac in `connecting` before any attempt shows "connecting…".
- **Ordering.** The merged Inbox orders as today, across Macs. Ties break by `macName`, then by key.
- **Performance.** Group headers are FlashList items with their own type. Rows remain memoised, and the per-Mac state is selected narrowly, so one Mac's link change re-renders only that Mac's header and rows.

- [ ] **Step 1 (failing tests):**
  - Cards show their Mac only when two or more are paired.
  - The drawer groups by Mac with "asleep".
  - `one_mac_asleep`: Mac `m1` is offline and `m2` is ready. `m1` rows are greyed, and its Allow and prompt are disabled. `m2`'s Allow sends on `m2`. The header reads "2 Macs · 1 online".
  - The New chip cycles only the online Macs, and `start` goes to the chosen one.
  - Recent merges and labels by Mac.
  - A perf test: a link change on one Mac doesn't re-render the other Mac's rows.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run the gates. Commit: "App: the Inbox and drawer across Macs".

### Task 4: Settings › Machines, push routing, hub `mac`, contract and docs

**Files:**
- Modify:
  - `app/settings.tsx`: a "Machines" section. Each Mac shows a row with `macName`, its host (muted) and its state. Rename opens a field. Unpair confirms ("Unpair <name>? Its sessions leave this phone.") and runs `fleet.unpair(mac)`. "+ Pair another machine" opens Pair, and is disabled at 4.
  - `src/notifications.ts`: `tapFromData(data)` gives `{host?, session}`. `watchTaps` resolves the key through `macOfHost`, and falls back to the Inbox.
  - `crates/octet-hub/src/push.rs` and `server.rs`: the pusher reads the hub's current publication. `Due::message` adds `"mac": host` when the host is known.
  - `apps/mobile/contract/hub.contract.test.ts`: a second real hub with its own run dir, port and fake bridge. Use the same raw session id on both if the fake bridge allows it. Then assert:
    - the phone, as two `HubClient`s fed into one store, lists both;
    - an answer reaches only the right hub's bridge;
    - unpairing one leaves the other streaming.
  - Docs: `docs/remote-control.md` (more than one Mac, push `mac`), `apps/mobile/README.md` and `CHANGELOG.md`.

**Rulings:**
- **The `mac` value** is the host of the hub's published URL (`Publication` in server.rs), lowercased. It's validated as a DNS name of at most 253 characters, or left out.
- **Push stays content-free.** The host is the address the phone already holds, and it names no workspace, command or file.
- **Unpair while open.** A screen whose key belongs to an unpaired Mac navigates to the Inbox.

- [ ] **Step 1 (failing tests):**
  - Rust: `data.mac` appears when published and is left out when not; a hostile host is left out; the body is unchanged.
  - App:
    - `tap_routes_by_mac`: a known host opens `m/<session>`; an unknown host, or none with two Macs, opens the Inbox; none with one Mac uses it.
    - Machines: rename, unpair confirm, "+ Pair another" disabled at 4.
    - A screen of an unpaired Mac goes back to the Inbox.
  - The contract steps.
- [ ] **Step 2.** Implement it.
- [ ] **Step 3.** Run all the gates: Rust, Linux clippy and Docker for octet-hub, `make mobile-check`, export, then `cargo build -p octet` and `make mobile-contract`. Commit: "Machines in Settings; pushes name their Mac".
