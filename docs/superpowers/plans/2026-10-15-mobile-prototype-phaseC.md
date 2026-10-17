# Octet mobile prototype, Phase C: Changes — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the phone shows what a session changed: files, diffs, checks and branch. It can commit them behind Face ID. There is no push or PR from the phone.

**Architecture:**
- A new `crates/octet-hub/src/git.rs` runs git in the session's registered workspace, with literal argv, a 10 s timeout, and at most 2 calls at once.
- Three phone commands, `changes`, `diff` and `commit`, plus two reply frames, `changes` and `diff`.
- On the phone, a Changes screen reached from Usage, from Session info, and from a file-edit tool entry's "view ›".

**Tech Stack:** Rust (octet-core protocol, octet-hub), the Expo app, and the system `git`.

**Spec:** `docs/superpowers/specs/2026-10-15-mobile-prototype-design.md` (Phase C, plus "Rules that hold in every phase").

## Global Constraints

- **Gates**
  - Rust: `make rust-check`, Linux clippy, and Linux Docker for octet-hub (`--init`).
  - App: `make mobile-check` and `npm run export:check`.
  - Last task: `cargo build -p octet`, then `make mobile-contract`.
  - Regenerate `protocol.ts` with the octet-core test.
  - Commit only on exit 0, with the gates run unpiped.
  - Never use bare `git stash`.
- **Git calls**
  - The workspace comes from the session's registration, never from the phone.
  - argv is fixed by the hub, with `-C <workspace>`.
  - The phone supplies no options, flags, refs or `-c`.
  - Environment: `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, `LC_ALL=C`. `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` are removed from the child's environment.
  - Read calls add `--no-optional-locks`.
  - Each call times out after 10 s and is killed on drop. At most 2 run at once; a third is refused as `busy` with "Git is busy; try again".
- **Limits**
  - At most 500 files, with `more` counting the rest.
  - A diff is clipped at 64 KiB or 2,000 lines, with `clipped: true`.
  - Untracked files are counted by line from at most 1 MiB each. A NUL byte in the first 8 KiB means binary.
  - Paths are cleaned for display and kept as git's `-z` bytes, decoded as UTF-8, lossy.
- **The `diff` path** must be one of the paths in the most recent `changes` reply to that connection for that session. Otherwise it's refused as `invalid` with "Ask for the changes first". The list is kept per connection, bounded at 500 entries per session.
- **`commit`**
  - Refused while the session's phase is `running`, or while an approval waits: "Finish or cancel the current turn first".
  - Refused when nothing has changed: "Nothing to commit".
  - The message must be 1–2,000 characters with no NUL: "Write a commit message".
  - It runs `git add -A`, then `git commit -F -` with the message on stdin. Hooks run.
  - The ack is "Committed <short hash>".
  - Audited as `commit` with the device name. The message is never logged.
- **Phase A and B rules still hold:** Face ID, hub text rendered as Text only, the performance budgets.
- **Commits:** stage by path. Never stage `output/` or `crates/octet-tui/src/tests/keys.rs`. Trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A workspace that isn't a git repository, or a bare or corrupt one.** It's refused with "This folder isn't a git repository", and the hub stays healthy. Test: Task 1 `not_a_repository`.
2. **Hostile file names:** newlines, quotes, leading `-`, unicode, a `..` that git reports. `-z` parsing keeps them intact, and nothing passes them to a shell. Test: Task 1 `hostile_names_round_trip`.
3. **A diff for a path outside the last list,** or a stale list after new edits. It's refused, then works after a fresh `changes`. Test: Task 1 `diff_needs_a_listed_path`.
4. **A commit racing a turn,** or a hook that hangs or fails. Refused while running. A hook failure gives git's own words, clipped to 2,000 characters. A hang ends at the timeout, and the index stays as git left it. Test: Task 1 `commit_refusals_and_hook_failure`.
5. **A 20,000-file change** (a vendored directory). The list is capped at 500 with `more`, the reply frame stays under MAX_MESSAGE, and it costs one `status` and one `numstat` call. Test: Task 1 `huge_change_is_capped`.

---

### Task 1: Hub git APIs (Rust)

**Files:**
- Create `crates/octet-hub/src/git.rs`.
- Modify `crates/octet-core/src/remote/protocol.rs`:
  - `PhoneCommand::{Changes{session}, Diff{session, path}, Commit{session, message}}`;
  - frames `changes_frame(req, ChangesInfo)` and `diff_frame(req, DiffInfo)`, with schema entries;
  - `ChangesInfo {branch?, upstream?, ahead, behind, files: [FileChange{path, add, del, binary, status}], more}`;
  - `DiffInfo {path, text, clipped, binary}`.
- Modify `crates/octet-hub/src/server.rs`: routing, the per-connection last list, audit, and refusals.
- Modify `crates/octet-hub/src/lib.rs`.
- Regenerate `apps/mobile/src/protocol.ts`.

**What `git.rs` runs:**
- **`changes`**
  - `git -C ws --no-optional-locks status --porcelain=v2 --branch -z --untracked-files=all`, for the branch, upstream, ahead/behind, and each entry's status letter (M, A, D, R, ?, U).
  - `git -C ws --no-optional-locks diff --numstat -z HEAD`, for tracked add and delete counts. Binary files show as `-`. A repository with no HEAD gets an empty tree diff.
  - Untracked files are counted by line, bounded.
- **`diff`**
  - Tracked: `git -C ws --no-optional-locks diff --no-color --no-ext-diff HEAD -- <path>`.
  - Untracked: `git -C ws --no-optional-locks diff --no-color --no-index -- /dev/null <path>`. Exit code 1 means differences, not an error.
- **`commit`**: `git -C ws add -A`, then `git -C ws commit -F -`, then `git -C ws rev-parse --short HEAD`.

- [ ] **Step 1 (failing tests)**, with temp repos created by `git init` in tests. Set `user.name` and `user.email` through the environment: `GIT_AUTHOR_*`, `GIT_COMMITTER_*`.
  - parse tests for porcelain v2 and numstat, `-z` included;
  - `not_a_repository`;
  - `hostile_names_round_trip`;
  - `diff_needs_a_listed_path`;
  - `commit_refusals_and_hook_failure`, using a failing `pre-commit` hook and a sleeping hook against a short test timeout;
  - `huge_change_is_capped`;
  - concurrency: a third call is refused as `busy`;
  - the parser refuses extra fields, a NUL in the message, and a path longer than 4,096 characters;
  - audit has no message text.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the Rust gates and `make mobile-check`. Commit: "Hub: changes, diffs and commits for a session's workspace".

### Task 2: App — the Changes screen

**Files:**
- Create `app/changes.tsx`, `src/changes.ts` (pure: the bar cells, the summary, and the Checks heuristic), and `src/components/FileRow.tsx`.
- Modify:
  - `src/components/TranscriptRow.tsx` / `ToolRun.tsx`: a file-edit tool entry ends in " view ›" (accent), which opens Changes with that file expanded;
  - `app/usage.tsx` and the ▾ menu's Session info: a "Changes ›" row;
  - `app/_layout.tsx`: register the route.

**Rulings** (prototype lines 205-248):
- **Header:** ‹ · Changes, with "<name> · N files · +a −d" under it, or "no changes".
- **File rows:**
  - Each row shows the name, its directory (muted), "+a −d", and a 5-cell bar. The bar uses `min(5, max(1, ceil((a+d)/8)))` accent `■`, with the rest in edge colour.
  - The status letter shows as a muted prefix (M, A, D, R or ?).
  - A tap expands the row and requests `diff` once per expansion, cached for the screen.
  - In the diff:
    - `+` lines sit on the selected background, with an accent sign;
    - `−` lines are muted and struck through;
    - `@@` lines are muted;
    - other lines are plain.
  - A clipped diff ends with "The rest of this diff is on the Mac." A binary file shows "binary file".
- **"Checks" card:**
  - It finds the last tool call in the session view whose command matches `/^(cargo (nextest run|test)|npm (run )?test|pnpm (run )?test|yarn (run )?test|pytest|go test|npx (jest|vitest)|jest|vitest)\b/`.
  - It shows ✓ (accent) when the call is done, ✗ (amber) when it failed, with the command. Otherwise "No tests run in this session".
  - It's a heuristic, labelled in code.
- **"Branch" card:**
  - The branch name, then "N ahead of <upstream>" when ahead.
  - Otherwise "uncommitted changes" when files are listed, or "clean".
  - With no branch (detached HEAD): "detached".
- **List:** FlashList. Rows are memoised. Pull to refresh re-requests `changes`.
- **States:**
  - Refusals show the hub's words, in fg.
  - Offline greys the screen and disables every action.
  - "This folder isn't a git repository" shows as the screen's empty state.

- [ ] **Step 1 (failing tests):**
  - the summary and bar maths;
  - the Checks heuristic: pass, fail, none, and a command that's not a test;
  - the screen lists files, a tap requests `diff` and renders the lines, the clipped and binary notes, pull to refresh, a refusal, offline, and not-a-repository;
  - "view ›" opens Changes with that file expanded;
  - a hostile path renders as plain text.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the gates. Commit: "App: the Changes screen".

### Task 3: App — commit, "Open PR on the desk", contract and docs

**Files:**
- Modify `app/changes.tsx`.
- Create `src/components/CommitSheet.tsx`.
- Extend `apps/mobile/contract/hub.contract.test.ts` with a temp git workspace in the harness.
- Update `docs/remote-control.md`, `apps/mobile/README.md` and `CHANGELOG.md`.

**Rulings:**
- **Commit** (border edge, fg) opens the CommitSheet.
  - The field is prefilled with the session's first prompt as one line, at most 72 characters, from the earliest `user` entry in the view; otherwise it's empty.
  - It's editable, multiline, and at most 2,000 characters.
  - "Commit" asks for Face ID with `confirmBiometric("Commit the changes")`, whatever the Face ID setting, then sends `commit`.
  - On the ack: toast "Committed <hash> on <branch>", close the sheet and refresh.
  - A refusal shows the hub's words in the sheet. The text is kept.
  - One at a time. Disabled offline or while an approval waits.
- **The PR button** reads "Open PR on the desk". It opens a small sheet: "Run this on the Mac:" and `gh pr create` in a selectable monospace box, with "Close". Nothing is sent.
- **Contract test.** It covers `changes`, `diff` and `commit` against the real hub binary, using a temp git repo as the fake bridge's workspace. Assert:
  - the file list and the diff text;
  - a diff of an unlisted path is refused;
  - commit gives "Committed <hash>";
  - a second commit gives "Nothing to commit".
- **Docs:** the Changes screen, what the phone can and can't do with git, the commit audit, and that hooks run.

- [ ] **Step 1 (failing tests):**
  - the sheet prefill;
  - Face ID is always asked, and a failure sends nothing;
  - commit sends the message and refreshes on the ack;
  - a refusal keeps the text;
  - double-tap safety;
  - offline and pending states;
  - the PR sheet sends nothing;
  - the contract steps.
- [ ] **Step 2.** Implement.
- [ ] **Step 3.** Run the gates, then `cargo build -p octet` and `make mobile-contract`. Commit: "App: commit behind Face ID; open a PR on the desk".
