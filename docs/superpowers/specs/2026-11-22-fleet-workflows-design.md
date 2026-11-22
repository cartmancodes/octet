# Fleet workflows: a self-improving multi-provider loop

Status: revision 2 (2026-11-22). The owner approved the Rust architecture (the `octet-fleet`
crate, a pure `machine`, independent roles and profiles, headless first) and requested changes to
the execution contract; a builder check raised overlapping questions. Both are folded in; see
"Changed after review" at the end. Fulfils the `octet-fleet` crate named in
[the harness v3 design](2026-09-30-rust-harness-v3-design.md) §2 and replaces its §9 (the legacy
Python fleet's config aliases, quota routing and model-as-orchestrator are not carried over).

## Why

The owner ran two long pipelines on music-decoder in November (Claude plans and implements,
Codex reviews) by hand: a shell script chaining `octet --print` calls, `--resume` IDs copied
between stages, a `VERDICT:` line grepped out of the reply, and a manual wait-and-retry when
Codex hit its usage limit. Nothing stopped a stage from being skipped, nothing noticed a crash
halfway through coding, and the loop had to be babysat.

The owner wants Octet to run that loop itself: **research → plan review → implement → test →
final review, and keep improving after review, implementation, test and the final review until
the final reviewer passes it.** Each stage can use a different provider and model. Rust, not a
model, decides whether coding may start and whether the run may finish.

## The invariant

Everything below exists to make one rule executable, checked by `machine` before a run may be
recorded `Completed`:

> **The final reviewer approved candidate tree T, T is covered by a complete passing evidence
> record E (every command ran against T and T was unchanged afterwards), E and the approval
> were both produced under the currently approved plan, and the live workspace equals T.**

Routing, recovery and replay all decide "what may run next" by asking which parts of this rule
are not yet satisfied, so no path can skip it.

## Goals

- **One run, one ID.** `octet fleet run` starts a run; `octet fleet resume ID` continues it after a
  pause, a Ctrl+C, a crash or a reboot.
- **Per-step provider, model, effort and permissions**, from a YAML workflow in the project.
- **The loop**: a rejected plan goes back to research; failed tests go back to the coder; a
  rejected change goes back to the coder, then tests, then a fresh final review, until the
  invariant holds.
- **Capped and stall-aware** (owner's choice B): `max_rounds` backward jumps per edge (default 5,
  so at most six attempts of the step that rejects), and an early pause when a round makes no
  progress.
- **Typed outcomes.** Business results (`Approved`, `ChangesRequested`, `TestsPassed`,
  `TestsFailed`) are distinct from execution results (`Invalid`, `Transient`, `ConfigError`,
  `LocalFailure`, `Unknown`, `AccessViolation`, `Interrupted`). Only `Transient` retries
  automatically.
- **Evidence Octet gathers itself**: Octet captures the workspace change and runs the tests,
  bound to exact trees. Only the evidence runner can produce `TestsPassed`; only a valid review
  can produce `Approved`.
- **A durably accepted step is never repeated**, and no writer starts on a tree with unexplained
  changes.
- **Modularity visible in the YAML**: every step names the artifacts it consumes, so a second plan
  or an architecture review is a YAML edit and a role, with no scheduler change.

## Non-goals (v1)

- A TUI view of runs. The runner emits a typed event stream so one can plug in next.
- Parallel steps or any graph beyond sequential steps with backward edges.
- Running test commands a model proposes. Only the workflow's commands run.
- Discarding partial edits automatically. Octet prints the git command; the user runs it.
- Showing runs on the phone. Committing, branching or pushing.
- An OS sandbox (see "Read-only steps" for what v1 does promise).
- Seeing changes inside submodules (see "Snapshots").

## Anti-goals

- A run recorded `Completed` when the invariant does not hold.
- A read-only step changing the user's workspace, index or refs through its working directory.
- A coder or tester started on a tree with changes the run cannot account for.
- A recovery decision that manufactures an approval or passing evidence.
- A loop that spends usage indefinitely without the owner seeing it.

## Constraints

- **Built on `octet_core::Session`**: `Session::open(Config, dir)`, `Event`s in, `Command::Answer`
  for approvals, `Session::shutdown`. The one addition below `octet-fleet` is that a session
  reports its vendor's process group (see "Process ownership").
- **No read-only mode exists** in Octet (ask, accept-edits, auto, full-access), and a vendor's own
  allowlist can skip an approval request.
- **Git is required**: snapshots are git trees, the workspace must be in a git work tree.
- **`octet-core` must not depend on `octet-fleet`**; the binary composes them.
- **Workspace rules**: edition 2024, clippy pedantic, `missing_docs`, `forbid(unsafe_code)` in the
  new crate (signals and process groups go through `octet-proc`, which already has them),
  `make rust-check` green, Linux parity. File locks use `std::fs::File::lock` (stable since 1.89).
- **Dependencies pass `deny.toml`**: new `serde` (derive) and `serde_norway` (maintained fork of
  `serde_yaml`; MIT/Apache).
- **Every provider verified on a real run before merge** (owner's standing rule): Claude, Codex,
  opencode and Antigravity each drive at least one step of a real small run.

## Approaches considered

1. **A generic step graph in YAML, run by a small pure Rust state machine (chosen).** The
   scheduler knows step kinds, artifact references and edges, so new steps are YAML edits.
2. **Five stages hard-coded in Rust, YAML only picking providers.** Every new step would be a
   scheduler change. Rejected.
3. **A model as orchestrator.** The model would decide whether coding may start. Rejected.

## Design

### Crate layout

`crates/octet-fleet` (library), depending on `octet-core`, `octet-proc` (process groups),
`octet-store` (private files), `tokio`, `serde`, `serde_json`, `serde_norway`, `thiserror`.

| Module | Owns |
|---|---|
| `definition` | YAML structs, static validation, the resolved workflow, its content hash |
| `snapshot` | candidate trees, commits and keep-alive refs, diffs, scratch clones |
| `store` | the run directory, the durable log, torn-tail repair, replay into `RunState` |
| `machine` | pure: artifact validity, next action, rounds, stalls, the invariant; no I/O |
| `contract` | prompts per role kind; parsing and validating replies into typed outcomes |
| `step` | one attempt: open the session, drive events, approvals, background work, classify |
| `evidence` | run the test commands against a frozen candidate |
| `ownership` | run and worktree locks, drift checks, leftover-process checks |
| `runner` | ties the above together, emits `FleetEvent`s |

The `octet` binary adds the `fleet` subcommand and a headless front end.

### The workflow file

`<cwd>/.octet/workflows/<name>.yaml` for a bare name, or a path. Example (ships as
`docs/fleet/five-stage.yaml`):

```yaml
version: 1
name: five-stage
limits:
  max_rounds: 5            # backward jumps per edge
  transient_retries: 3
profiles:
  claude-research: { engine: claude, model: claude-opus-5-5, effort: high }
  codex-review:    { engine: codex,  model: gpt-6-astra }
  codex-code:      { engine: codex,  model: gpt-6-astra, mode: full-access }
  opencode-test:   { engine: opencode, mode: accept-edits }
  claude-review:   { engine: claude, model: claude-opus-5-5 }
roles:
  researcher: { kind: plan,    instructions: "Investigate the code and write a plan …" }
  reviewer:   { kind: verdict, instructions: "Review against the task …" }
  coder:      { kind: change,  instructions: "Implement the approved plan …" }
  tester:     { kind: tests,   instructions: "Write or select tests for the change …" }
steps:
  - id: research
    role: researcher
    profile: claude-research
  - id: review_plan
    role: reviewer
    profile: codex-review
    reviews: research
    on_changes_requested: research
  - id: implement
    role: coder
    profile: codex-code
    plan_from: research
    approval_from: review_plan
  - id: test
    role: tester
    profile: opencode-test
    change_from: implement
    commands: ["cargo test --workspace"]
    timeout: 20m
    on_tests_failed: implement
  - id: review_change
    role: reviewer
    profile: claude-review
    reviews: implement
    evidence_from: test
    on_changes_requested: implement
```

A **profile**: `engine` (any `Engine::parse` name), optional `model`, `effort`, `mode` (default
`ask`), `binary` (default: the provider's `default_binary`), `approval_timeout`.
A **role**: `kind` and `instructions`. A **step**: `id`, `role`, `profile`, and by kind:

| Kind | References (required) | Produces | Accepted outcomes |
|---|---|---|---|
| `plan` | — | plan artifact (reply text) | `Produced` |
| `verdict` reviewing a plan | `reviews: <plan step>` | approval of that plan artifact | `Approved`, `ChangesRequested` |
| `change` | `plan_from`, `approval_from` | change artifact: candidate tree, captured by Octet | `Produced` |
| `tests` | `change_from` | evidence record, produced by Octet's runner | `TestsPassed`, `TestsFailed` |
| `verdict` reviewing a change | `reviews: <change step>`, `evidence_from: <tests step>` | approval of (tree, evidence) | `Approved`, `ChangesRequested` |

Plus `session` (`fresh` default for verdict and plan steps, `continue` for change and tests),
`commands` and `timeout` (tests), and edges: `on_changes_requested` (verdict),
`on_tests_failed` (tests). Read-only is not a setting: plan and verdict steps are always
read-only, change and tests steps always write.

### Validation

`octet fleet validate` is static and side-effect free: it starts no vendor and runs no command.
A run refuses to start unless:

- `version` is 1; step IDs are unique; every role, profile and reference resolves to a step of
  the right kind that comes **earlier**; edges point to an earlier step of a kind that can revise
  the rejected artifact (a plan review's edge to a plan step, a tests or change review's edge to
  a change step); every engine parses, its binary resolves, `effort` passes the engine's check,
  `mode` parses.
- **The plan gate**: every change step's `approval_from` is a verdict step whose `reviews` is that
  change step's `plan_from`. A change step cannot run until that approval is valid.
- **The completion gate**: the last step is a verdict reviewing a change step, with
  `evidence_from` a tests step whose `change_from` is that same change step.
- Plan and verdict profiles have mode `ask` (or none); anything else is rejected, not overridden.
- `commands` is non-empty for tests steps.
- The workspace is in a git work tree, and no other run holds it (see "Process ownership").

**A negative outcome without an edge pauses the run** (`NoRoute`), as does any outcome the
machine has no rule for.

**Trust.** Running a project workflow is a trust decision, like opening an untrusted workspace's
`.agents` in Antigravity. At `run`, Octet resolves the workflow and shows what it would execute:
each profile's absolute binary path and mode, and every test command. On a terminal the user
answers y/n; otherwise `--trust` is required. The answer is stored under
`<data_dir>/fleet/trusted/<hash>` keyed by the hash of that resolved content, so a changed
workflow asks again. `resume` runs the content frozen in `run.json`, already trusted.

Models are not checked statically (that needs a probe). A model the vendor rejects is a
`ConfigError` at the first turn.

### Artifacts and the machine

Each accepted outcome produces an **artifact** with an ID, the attempt that made it, and the
artifact IDs it consumed:

- plan `P` (consumed: the task, and feedback if revising)
- plan approval `A(P)`
- change `C` = candidate tree T, under `A(P)`
- evidence `E(T)` under `A(P)`: passing or failing
- change approval `R(T, E)` under `A(P)`

An artifact is **current** when it is the latest of its step and everything it consumed is still
current. A tree-bound artifact is also current only while its tree is the run's current
candidate. Producing a new plan makes `A`, and everything under it, stale; a new candidate tree
makes `E` and `R` stale. Nothing stale is ever handed to a step or counted for the invariant.

`machine::next(state)` is a pure function of the replayed log:

1. If the run is paused or awaiting a decision, wait.
2. If an outcome is pending routing: a negative outcome with an edge attaches its feedback to the
   edge's target and marks the target's artifact as needing revision; without an edge, pause.
3. Otherwise, run the **first step, in order, whose artifact is not current or needs revision**.
4. If every step's artifact is current and the invariant holds, `Completed`.

Rounds and stalls are counted here too, so a resumed run makes the same decisions.

**Rounds.** Each edge counts its jumps. A jump that would exceed `max_rounds` pauses
(`RoundLimit`) instead; `max_rounds: 5` means the initial attempt plus five revisions.

**Stalls** (owner's choice B), each pausing (`Stalled`):

- A verdict step returns `ChangesRequested` and resolves none of its own previously open findings
  (applies only when there were previous findings, and only within the same review step).
- A change attempt that was given feedback produces a tree equal to its input.
- Two consecutive `TestsFailed` from the same tests step with the same failing commands on the
  same tree.

### Verdicts

A verdict reply ends with one fenced JSON block, as the prompt specifies exactly:

```json
{"verdict": "approved" | "changes_requested",
 "new_findings": [{"severity": "blocking" | "minor", "summary": "…", "file": "…"}],
 "previous": [{"id": "review_change/F3", "status": "resolved" | "open"}]}
```

- **Octet assigns finding IDs** (`<step>/F<n>`, numbered across the whole run for that step), so
  IDs are unique and a reviewer cannot renumber.
- **`previous` must list exactly** the step's findings still open before this round (all of them,
  not only the last round's), each once.
- Validation order: block present and well-formed → `previous` exact → consistency:
  `approved` requires no open **blocking** finding afterwards (open minor findings are allowed
  and reported at completion); `changes_requested` requires at least one open finding afterwards.
- Anything failing validation is `Invalid`: one corrective prompt in the same session, then pause
  (`InvalidOutput`). Invalid never counts as a round and never triggers a stall check.
- The reviewer receives every open finding with its ID, so it can check what was fixed.

### What a step receives

The user's timeline (every child event, streamed) is separate from a step's input, which
`contract` builds from current artifacts only, naming each by its ID and tree:

| Step | Receives |
|---|---|
| plan | task; findings that routed here |
| verdict on a plan | task; plan `P`; its own open findings |
| change | task; plan `P` under `A(P)`; findings or failing evidence that routed here |
| tests | task; plan; the patch base..T |
| verdict on a change | task; plan; the patch base..T; evidence `E(T)`; its own open findings |

The patch is `git diff --binary <base> <T>`: everything this run changed, including new files.
Over the prompt budget, the prompt carries the `--stat`, and the full patch is a file **inside the
reviewer's working directory** (see the scratch clone below), which every provider reads without
an approval; the prompt names it and requires the review to cover it. Write steps get the same
file under `<run>/attempts/<id>/` and can also run `git diff` themselves.

### Read-only steps: isolation, not a sandbox

v1 promises **isolation of the user's workspace, plus detection**, not OS-level write prevention:

- Every plan and verdict attempt runs in a **scratch clone**: `git clone --shared --no-checkout` of
  the repo into `<run>/attempts/<id>/tree`, then a detached checkout of the candidate commit, with
  the vendor's cwd at the same relative path as `--cwd`. The reviewer sees exactly T (not whatever
  the live tree holds), and an edit, an index change or a ref change lands in the clone, not in
  the user's checkout or refs.
- The runner **denies every approval** a read-only step raises.
- The clone's tree is compared before and after; any change is `AccessViolation`. The verdict is
  discarded and the run pauses with the diff. The only way on is `retry` (a fresh clone, a new
  attempt) or `stop`. Nothing an `AccessViolation` produced is ever adopted.
- **Not promised**: a vendor writing outside its working directory by absolute path. Such a write
  into the user's workspace is still caught by the drift check at the next transition. The
  documentation says this plainly.

### Snapshots

`snapshot::take` builds a tree without touching the user's index, HEAD or branches:

1. Copy the real index (`git rev-parse --git-path index`) to `<run>/index.tmp`, so tracked
   paths, including tracked files that `.gitignore` covers and staged additions, are kept.
2. `GIT_INDEX_FILE=<run>/index.tmp git add -A` over the **whole work tree** (top level, not
   `--cwd`), so modifications, deletions and untracked non-ignored files are overlaid.
3. `git write-tree`; then `git commit-tree` and `git update-ref refs/octet-fleet/<run>/<seq>`, so
   the tree survives `git gc` and a scratch clone can check it out.

Rules: the run's own state lives in `<data_dir>`, never in the work tree. A dirty start is fine:
the base snapshot includes it, and the patch is relative to it. Submodules are captured as their
recorded gitlink; content changes inside a submodule are not seen (documented; v1 does not try).
A failed snapshot is a `LocalFailure`. *(Builder's call: removing the refs of finished runs.)*

### Test evidence

The tests step has two halves:

1. **The tester's turn** (write, in the live workspace) authors or selects tests.
2. **Octet's evidence runner**: freeze candidate T = snapshot; run every command in order with
   `sh -c` in the workspace (own process group, killed at `timeout`, pgid logged); snapshot again.
   - If the tree changed, the pass is void: T' becomes the candidate and the **whole** list runs
     again on T'. At most 2 such reruns; a third change pauses (`UnstableTests`).
   - Otherwise the evidence record `E(T)` lists each command, exit status (or timeout), duration,
     and the last 16 KiB of output. All zero → `TestsPassed`; any other → `TestsFailed`.

The tester's own turn is never evidence. Only this runner writes evidence, and only an evidence
record whose tree equals the current candidate counts.

### Process ownership

- **Locks.** While driving a run, Octet holds an OS lock (`File::lock`) on `<run>/lock` and on
  `<data_dir>/fleet/worktrees/<hash of canonical top level>.lock`. A second run on the same work
  tree is refused with the ID of the run that holds it. Locks die with the process.
- **Drift.** At every transition (before each attempt, before acceptance, on resume), the live
  tree is compared with the last accepted tree. A difference the run cannot account for pauses
  (`Drift`) with the diff. Decisions: `adopt` (the drift becomes a new candidate, so its evidence
  and approval must be produced afresh) or `stop`.
- **Leftover processes.** Each session reports its vendor's process group (a small addition: the
  engine already spawns vendors with `process_group(0)` in `octet-proc`; `Handle` gains a
  read-only accessor that the session exposes). The runner logs every vendor and test-command
  process group. Before a recovery snapshot or a new writer, it checks those groups are gone; if
  one is alive, it pauses (`ProcessesRunning`) naming them. `--decision kill` sends SIGTERM, then
  SIGKILL after 5 s, then checks again.

### Failures

`step` classifies how a turn ends:

| Class | Examples | Action |
|---|---|---|
| `Transient` | usage or rate limit, overloaded, network error, the vendor process exiting mid-turn | retry, if the tree is unchanged |
| `ConfigError` | model not found, binary missing, not logged in | fail the run at once, with the error |
| `LocalFailure` | `Event::Error` from the core pump (journal write, a stalled consumer), snapshot failure | pause |
| `Unknown` | anything not matched | pause |

*(Builder's call: the per-provider match patterns, from the vendors' real error texts.)*
Retries are logged with their count and the next eligible time (1, 5, then 15 minutes, or the reset
time a usage-limit error names), so a restarted run waits out the backoff instead of retrying at
once. Before accepting a turn's output, `step` waits for the vendor's background tasks to finish
and the grace period to pass, reusing the `Background` tracker from `headless.rs` (moved into
`octet-core` so both use it).

### Persistence: the run directory

`<data_dir>/fleet/<run-id>/`, owner-only:

- `run.json`, written once: the resolved workflow (inlined), its hash, the task, the work tree and
  `--cwd`, the base snapshot, Octet's version.
- `log.jsonl`: the authority on accepted work. Each record is synced before the runner acts on it.
  On replay, a torn trailing record is cut back to the last newline before anything is appended.
  - `attempt_started {attempt, step, round, input_tree, consumed}`: written **before**
    `Session::open`; the attempt directory `<run>/attempts/<attempt>/` is derived from the ID.
  - `attempt_session {attempt, journal, process_group}`, `vendor_session {attempt, id}`: as soon
    as known (the vendor session ID is the latest non-empty `Event::Ready`).
  - `evidence_command {attempt, command, process_group}`, `evidence_result {…}`.
  - `attempt_finished {attempt, outcome, output_tree, artifacts}`: appended only after its artifact
    files are synced and its snapshot ref exists.
  - `retry {attempt, class, count, eligible_at}`, `paused {reason, detail}`,
    `decision {kind}`, `completed {tree, evidence, approval}`, `stopped`, `failed {error}`.
- `artifacts/`: plans, verdicts, patches, evidence, by artifact ID.
- `attempts/<id>/`: the child journal, scratch clone (removed once the attempt is accepted), patch
  file.

*(Builder's call: the run ID format, sortable by time and typeable.)*

The guarantee is precisely: **a step whose `attempt_finished` is durable is never repeated.** A child
that finished just before the parent crashed is an unfinished attempt and goes to reconciliation.

### Recovery and decisions

`octet fleet resume <id>` takes the locks, replays the log, checks leftover processes, takes a
snapshot, then:

- **Last attempt durable**: drift check, then `machine::next`.
- **Unfinished read-only attempt**: discard its clone and run a new attempt (nothing it did can
  have reached the workspace; drift is still checked).
- **Unfinished write attempt, tree unchanged**: run it again (continuing its vendor session if one
  was logged).
- **Unfinished write attempt, tree changed** (a crash, a signal, *or a transient failure*: every
  interrupted write takes this path): **reconciliation**, showing the partial diff. Decisions
  depend on the step kind:
  - change step: `keep` (continue the same vendor session, told it was interrupted and its partial
    changes are in place; with no logged session, a fresh one is told the same), `adopt` (the
    current tree becomes the change artifact, so it must still pass tests and review), `stop`.
  - tests step: `keep` (continue the tester), `adopt` (accept the tester's edits as authored, and
    go to the evidence runner on the current tree), `stop`. **No decision produces `TestsPassed`.**
- **An evidence run interrupted**: it runs again in full on a fresh snapshot.
- **Paused**: the reason and open findings are shown; decisions by reason: `RoundLimit` and
  `Stalled` → `continue` (one more round) or `rounds N`, or `stop`; `Drift` → `adopt` or `stop`;
  `AccessViolation` → `retry` or `stop`; `ProcessesRunning` → `kill` or wait; `Transient` exhausted,
  `LocalFailure`, `Unknown`, `InvalidOutput`, `NoRoute` → `retry` or `stop`. **No decision produces
  `Approved`.**

On a terminal the user picks interactively; otherwise `--decision` is required. Ctrl+C interrupts
the running turn, records `paused {interrupted}`, exits 130; SIGTERM the same, 143.

### Write steps' approvals

Write steps use their profile's mode. Their approvals go to the front end: on a terminal,
`octet fleet` asks y/n on stderr; with no terminal, they are denied with a warning, as `--print`
does. Unattended runs set `mode: auto` or `full-access` in the profile (shown in the trust prompt).

### Front end: `octet fleet`

```
octet fleet run WORKFLOW (--task TEXT | --task-file PATH | --task -) [--cwd PATH]
                 [--journal-dir PATH] [--trust] [--output text|json]
octet fleet resume RUN_ID [--decision D] [--rounds N] [--output text|json]
octet fleet status [RUN_ID]
octet fleet validate WORKFLOW [--cwd PATH]
```

Text output: a header per attempt (`▸ implement · round 2 · codex gpt-6-astra`), the child's
streamed text (control characters stripped, as `--print` does), each outcome, and at the end the
invariant's parts (tree, evidence, approval) with `git diff <base> <final>`. `--output json` writes
every `FleetEvent` as a line, child events wrapped with step and attempt. Exit codes: 0 completed,
1 failed, 2 usage or validation error, 3 paused (with the resume command), 130, 143.

`FleetEvent`: `RunStarted`, `AttemptStarted`, `Session { attempt, event }`, `Approval`,
`EvidenceCommand`, `AttemptFinished { outcome }`, `Retrying`, `Paused { reason }`,
`NeedsDecision`, `Completed`.

### Testing

- **Unit (pure):** validation (including the plan and completion gates, missing edges, wrong-kind
  references); `machine` over hand-built logs: staleness after a new plan or tree, routing,
  rounds, every stall rule, the invariant; verdict validation order and precedence; torn-tail
  repair.
- **Snapshots** against real `git init` repos: tracked-but-ignored files kept, staged additions
  kept, untracked files, deletions, the user's index and HEAD byte-identical afterwards, `--cwd`
  below the top level, refs created, scratch clone checks out the candidate.
- **Scripted providers.** `protocol-child` gains a script-file mode: `OCTET_FLEET_SCRIPT` names a
  JSON file keyed by `<step>#<round>` (from the runner's prompt header) whose entries say which
  files to write, what to reply, or to hold; every turn appends its prompt, its cwd and whether it
  was resumed to a log the test reads. It works under the codex and claude dialects.
- **Release tests** (process tests, `crates/octet/tests/fleet.rs`; fake Claude researching and
  reviewing, fake Codex coding and testing):
  1. **The five-stage run**: plan approved → implement → the tester holds, Octet is SIGKILLed once
     `attempt_started test` is durable → resume re-runs the tester, **the coder ran once** → a
     command fails, **the run cannot complete** → implement round 2 (resumed session) → tests pass
     → review_change requests changes → implement → test → a **fresh** review approves; its
     prompt names the passing evidence's tree, and its patch includes the tester's new file.
  2. **Wrong-revision approval**: command A passes, command B edits a fixture and passes, A fails
     on the new tree → the run cannot complete.
  3. **Crash at the acceptance boundary**: kill after the child finishes but before
     `attempt_finished` is durable → reconciliation; kill after it is durable → the step is
     skipped.
  4. **Interrupted tester**: kill after the tester writes a file, before any command → no
     `--decision` yields passing evidence without the commands running.
  5. **Workspace ownership**: a second run on the same work tree is refused; an edit to a paused
     run's tree surfaces as `Drift` before the next step.
  Plus: a stall pause (exit 3), a round-limit pause, an access violation (a reviewer that writes a
  file in its clone), a leftover test process found on resume, a transient retry that survives a
  restart and waits out its backoff.
- **Real providers** before merge: one small run on a throwaway repo, with each of Claude, Codex,
  opencode and Antigravity driving at least one step.

## Left to the builder

Marked inline: ref cleanup, the run ID format, the per-provider error patterns. Also: module
boundaries below the table, prompt wording (the JSON contract is fixed), text layout, `FleetEvent`
naming, and Antigravity's workspace-config trust in scratch clones (they are new paths, so their
`.agents` configuration does not load there; v1 accepts that).

## Changed after review

From the owner's review:

- **Evidence**: frozen candidate, the whole suite reruns when a command changes the tree (capped),
  and the final approval is bound to tree + evidence ID ("Test evidence", the invariant).
- **Recovery by step kind**; `adopt` replaces `accept`; every interrupted write, transient
  failures included, goes through reconciliation; no decision makes `TestsPassed` or `Approved`.
- **The plan gate in validation**, explicit artifact references in the YAML, `NoRoute` pauses,
  stale artifacts after an upstream change ("Artifacts and the machine").
- **Read-only**: v1 promises isolation through scratch clones plus detection, not prevention, and
  says what it does not cover. Project workflows need a trust decision; `validate` is static.
- **Snapshots** seeded from a copy of the real index; whole work tree; state outside the tree;
  dirty starts, submodules and `--cwd` defined.
- **Ownership**: OS locks per run and per work tree, drift checks at every transition, leftover
  processes checked (needs the process-group accessor in core).
- **Failure classes**; persisted retry timing; background work reused from headless.
- **The log**: attempt recorded before the session opens, artifacts durable before
  `attempt_finished`, torn-tail repair, the guarantee stated precisely.
- **Verdict precedence**: Octet-assigned IDs, exact `previous`, blocking findings closed before
  approval, stalls only on `ChangesRequested` in the same step; `max_rounds` defined as jumps.
- **Release tests 2–5** added.

From the builder check: carried-over findings are tracked by ID (above); the oversized patch is a
file inside the reviewer's clone; leftover vendor processes are checked on resume; all four
providers must pass a real run; this design replaces v3 §9.
