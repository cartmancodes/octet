# Fleet workflows: a self-improving multi-provider loop

Status: revision 4 (2026-11-22). The owner approved the Rust architecture (the `octet-fleet`
crate, a pure `machine`, independent roles and profiles, headless first, provider replies kept
apart from the evidence Octet collects) and has twice requested changes to the execution
contract. Both reviews and a builder check are folded in; see "Changed after review" at the end.
Fulfils the `octet-fleet` crate named in [the harness v3 design](2026-09-30-rust-harness-v3-design.md)
§2 and replaces its §9 (the legacy Python fleet's config aliases, quota routing and
model-as-orchestrator are not carried over).

## Why

The owner ran two long pipelines on music-decoder in November (Claude plans and implements,
Codex reviews) by hand: a shell script chaining `octet --print` calls, `--resume` IDs copied
between stages, a `VERDICT:` line grepped out of the reply, and a manual wait-and-retry when
Codex hit its usage limit. Nothing stopped a stage from being skipped, nothing noticed a crash
halfway through coding, and the loop had to be babysat.

The owner wants Octet to run that loop itself: **research ⇄ research review → plan ⇄ plan
review → implement → test → final review, and keep improving after review, implementation, test
and the final review until the final reviewer passes it.** Research is a stage of its own with
its own review loop; **implementation starts only once the research and the plan are both
approved.** Each stage can use a different provider and model. Rust, not a model, decides
whether coding may start and whether the run may finish.

## The invariant

Everything below exists to make one rule executable. The run is recorded `Completed` only when:

> **The final reviewer approved candidate K with tree T, under evidence record E and the
> document approvals A (the plan's, and through it the research's); E is a complete passing
> record for T (every command started on T, and the tree was T when each command ended); E and
> the approval were produced under A, each of which is the current approval of the current
> document; and the runner's last observation of the live workspace,
> taken immediately before writing `completed` and persisted in that record, equals T.**

`machine` decides from the log whether the logged artifacts satisfy the first four parts; the
runner supplies the last observation. Routing, recovery and replay all decide what may run next
by asking which parts are not yet satisfied, so no path can skip it.

## Goals

- **One run, one ID.** `octet fleet run` starts a run; `octet fleet resume ID` continues it after a
  pause, a Ctrl+C, a crash or a reboot.
- **Per-step provider, model, effort and permissions**, from a YAML workflow in the project.
- **The loops**: rejected research goes back to the researcher; a rejected plan goes back to the
  planner; failed tests go back to the coder; a rejected change goes back to the coder, then
  tests, then a fresh final review, until the invariant holds. Each loop has its own round cap
  and stall checks.
- **Implementation is gated**: a change step runs only when every document it builds on (the
  plan, and the research the plan builds on) has a current approval.
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
- **A durably accepted step is never repeated**, and no writer starts while another writer from
  this or an earlier run may still be alive, or on a tree with unexplained changes.
- **Modularity visible in the YAML**: every step names the artifacts it consumes, so a research
  stage, a second plan or an architecture review is a YAML edit and a role, with no scheduler
  change. (The research stage itself is an example: it is a `document` step and a `verdict` step,
  nothing more.)

## Non-goals (v1)

- A TUI view of runs. The runner emits a typed event stream so one can plug in next.
- Parallel steps or any graph beyond sequential steps with backward edges to the producer.
- Running test commands a model proposes. Only the workflow's commands run.
- Discarding partial edits automatically. Octet prints the git command; the user runs it.
- Showing runs on the phone. Committing, branching or pushing.
- An OS sandbox (see "Read-only steps" for what v1 does promise).
- Sparse checkouts (refused) and changes inside submodules (not seen); see "Snapshots".

## Anti-goals

- A run recorded `Completed` when the invariant does not hold.
- A read-only step changing the user's workspace, index or refs through its working directory.
- A coder or tester started on a tree with changes the run cannot account for, or while an
  earlier writer may be alive.
- A recovery decision that manufactures an approval or passing evidence.
- Signalling a process Octet cannot show is its own.
- A loop that spends usage indefinitely without the owner seeing it.

## Constraints

- **Built on `octet_core::Session`**: `Session::open(Config, dir)`, `Event`s in, `Command::Answer`
  for approvals, `Session::shutdown`. The one addition below `octet-fleet`: a session reports its
  vendor's process identity (see "Process ownership").
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
| `machine` | pure: artifacts, candidates, currency, next action, rounds, stalls, the invariant |
| `contract` | prompts per role kind; parsing and validating replies into typed outcomes |
| `step` | one attempt: register, open the session, drive events, approvals, background, classify |
| `evidence` | run the test commands against a candidate, checking it at every command boundary |
| `ownership` | run and worktree locks, the worktree ownership record, drift, process checks |
| `runner` | ties the above together, emits `FleetEvent`s |

The `octet` binary adds the `fleet` subcommand and a headless front end.

### The workflow file

`<cwd>/.octet/workflows/<name>.yaml` for a bare name, or a path. Example (ships as
`docs/fleet/research-plan-build.yaml`):

```yaml
version: 1
name: research-plan-build
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
  researcher: { kind: document, instructions: "Investigate the code: what exists, what the task touches, risks …" }
  planner:    { kind: document, instructions: "Write an implementation plan from the approved research …" }
  reviewer:   { kind: verdict,  instructions: "Review against the task …" }
  coder:      { kind: change,   instructions: "Implement the approved plan …" }
  tester:     { kind: tests,    instructions: "Write or select tests for the change …" }
steps:
  - id: research
    role: researcher
    profile: claude-research
  - id: review_research
    role: reviewer
    profile: codex-review
    reviews: research          # rejection goes back to research (the producer)
  - id: plan
    role: planner
    profile: claude-research
    inputs:
      - { from: research, approval_from: review_research }
  - id: review_plan
    role: reviewer
    profile: codex-review
    reviews: plan              # rejection goes back to plan
  - id: implement
    role: coder
    profile: codex-code
    inputs:
      - { from: plan, approval_from: review_plan }
  - id: test
    role: tester
    profile: opencode-test
    change_from: implement     # failure goes back to implement (the producer)
    commands: ["cargo test --workspace"]
    timeout: 20m
  - id: review_change
    role: reviewer
    profile: claude-review
    reviews: implement         # rejection goes back to implement
    evidence_from: test
```

A **profile**: `engine` (any `Engine::parse` name), optional `model`, `effort`, `mode` (default
`ask`), `binary` (default: the provider's `default_binary`), `approval_timeout`.
A **role**: `kind` and `instructions`. A **step**: `id`, `role`, `profile`, and by kind:

| Kind | References (required) | Produces | Accepted outcomes |
|---|---|---|---|
| `document` | optional `inputs` | document `D` (reply text): research, a plan, … | `Produced` |
| `verdict` on a document | `reviews: <document step>` | document approval `A(D)` | `Approved`, `ChangesRequested` |
| `change` | `inputs` (at least one) | change `C`, a new candidate | `Produced` |
| `tests` | `change_from` | a derived candidate (tester edits), then evidence `E` | `TestsPassed`, `TestsFailed` |
| `verdict` on a change | `reviews: <change step>`, `evidence_from: <tests step>` | change approval `R(K, E)` | `Approved`, `ChangesRequested` |

**`inputs`** lists the documents a step builds on, each as `{from: <document step>,
approval_from: <verdict step>}`. **A document can only be consumed through its approval**: there
is no way to name a document without the verdict step that approves it, so nothing downstream of
research can start before research is approved. The plan's own input is the approved research,
and the coder's input is the approved plan, so the coder transitively requires both.

Plus `session` (`fresh` default for verdict and document steps, `continue` for change and tests),
and `commands` and `timeout` (tests). Read-only is not a setting: document and verdict steps are
always read-only, change and tests steps always write.

**Edges go to the producer.** In v1 a negative outcome always routes to the step that produced
the rejected artifact: a document review's rejection to its `reviews` step (research review →
research, plan review → plan), a change review's to its
`reviews` step, a test failure to its `change_from` step. The YAML may spell the edge
(`on_changes_requested`, `on_tests_failed`) for readability; if it does, it must name that
producer. So every negative outcome has a route, and no edge can target a step that cannot revise
what was rejected.

### Validation

`octet fleet validate` is static and side-effect free: it starts no vendor and runs no command.
A run refuses to start unless:

- `version` is 1; step IDs are unique; every role, profile and reference resolves to a step of
  the right kind that comes **earlier**; any spelled edge names the producer; every engine parses,
  its binary resolves, `effort` passes the engine's check, `mode` parses.
- **The approval gate**: every `inputs` entry names a document step and an `approval_from` verdict
  step whose `reviews` is that same document step; every change step has at least one input. A
  step cannot run until every approval it requires, directly or through its inputs' inputs, is
  current, so the coder waits for both the research and the plan approval.
- **The completion gate**: the last step is a verdict reviewing a change step, with
  `evidence_from` a tests step whose `change_from` is that same change step.
- Document and verdict profiles have mode `ask` (or none); anything else is rejected, not
  overridden.
- `commands` is non-empty for tests steps.
- The workspace is in a git work tree that is not a sparse checkout (see "Snapshots").
- **Ownership is resolved**: no live process holds the work tree, and no earlier run left an
  unresolved write attempt on it (see "Process ownership").

**Trust.** Running a project workflow is a trust decision, like opening an untrusted workspace's
`.agents` in Antigravity. At `run`, Octet resolves the workflow and shows what it would execute:
each profile's absolute binary path and mode, and every test command. On a terminal the user
answers y/n; otherwise `--trust` is required. The answer is stored under
`<data_dir>/fleet/trusted/<hash>` keyed by the hash of that resolved content, so a changed
workflow asks again. `resume` runs the content frozen in `run.json`, already trusted.

Models are not checked statically (that needs a probe). A model the vendor rejects is a
`ConfigError` at the first turn.

### Artifacts, candidates and the machine

**Candidates.** The run keeps a lineage of candidates. Each has a stable ID, its tree, its
parent candidate, and its origin:

| Origin | Made by | Parent |
|---|---|---|
| `base` | run start | — |
| `change` | a change step's accepted attempt | the candidate it started from |
| `tests` | a tests step's tester turn, when it edited the tree | the candidate under test |
| `evidence` | the evidence runner, when a command changed the tree | the candidate it ran on |
| `drift` | `Drift → adopt` | the candidate the drift was found against |

The **tip** is the latest candidate. A change artifact `C` names the candidate its step made; it
does not go stale because a later candidate derives from it. Tester edits, evidence-run edits and
adopted drift each make a **derived** candidate: they move the tip forward without invalidating
the coder's change, so the coder is not re-run for them. Evidence and change approvals bind to an
exact candidate; only a candidate equal to the tip satisfies the invariant.

**Artifacts** each record what they **require** (must stay current for the artifact to be
current) separately from their **history** (inputs kept to explain the attempt: feedback,
earlier findings, the previous artifact being revised). History never affects currency.

| Artifact | Requires | History |
|---|---|---|
| document `D` (research `D_r`, plan `D_p`) | the approvals of its `inputs` (the plan: `A(D_r)`) | the rejection and findings that prompted a revision |
| document approval `A(D)` | `D` | earlier findings of this review step |
| change `C` (candidate K) | the approvals of its `inputs` (`A(D_p)`, hence `A(D_r)`) | the feedback that prompted it |
| evidence `E(K)` | what `C` requires; K is the tip | — |
| change approval `R(K, E)` | what `C` requires, `E(K)`; K is the tip | earlier findings of this review step |

Requirements are transitive through currency: `A(D_p)` requires `D_p`, which requires `A(D_r)`,
which requires `D_r`. A new research document would make everything built on it stale, plan
included. (In v1 no edge can send the run back to research once the plan exists, since edges go
to the producer; the rule matters for workflows with more stages.)

An artifact is **current** when it is the latest of its step, everything it requires is current,
and any "K is the tip" condition holds. A **rejection** (a `ChangesRequested` or `TestsFailed`)
is an outcome, not an artifact: it marks the artifact it rejected as **needing revision** and
becomes history for the producer's next attempt. When the producer's next artifact is accepted,
the mark is cleared, because the rejected artifact is no longer the latest of its step.

`machine::next(state)` is a pure function of the replayed log:

1. Paused or awaiting a decision: wait.
2. Otherwise run the **first step, in order, whose artifact is missing, not current, or needs
   revision**, giving it the current artifacts it requires and its history.
3. Every step's artifact current, none needing revision, final approval `Approved`, and the
   logged parts of the invariant hold: **ready to complete**. The runner then takes its last
   observation of the live tree; equal to T → `completed`, otherwise `Drift`.

Worked traces (each a unit test):

- `research(D1) → review_research rejects D1 → research(D2)`: D1 needs revision → research runs
  with the rejection as history → D2 is accepted and is now the latest research, D1's mark clears,
  D2 requires nothing → `A(D2)` is missing → next is exactly `review_research(D2)`.
- `research(D2) approved → plan`: before `A(D2)` exists, `plan` and `implement` are never next;
  after it, next is exactly `plan`, given `D2` under `A(D2)`.
- `plan(P1) → review_plan rejects P1 → plan(P2)`: as for research; next is exactly
  `review_plan(P2)`, and `A(D2)` stays current throughout (the plan loop never re-runs research).
- `implement(C1 = K1) → tester edits (K2, parent K1) → evidence(K2) passes → review approves
  R(K2, E(K2))`: C1 stays current (its requirements `A(P)` and `A(D)` are current); E and R bind
  to K2 = tip → ready to complete; **implement ran once**.
- `… → evidence(K2) fails`: K2's change needs revision → implement runs with the failure as
  history, from the live tree (K2, tester files included) → K3 → tests → …
- `… → review_change rejects R(K2)`: C needs revision → implement → K3 → E(K2) and R are no longer
  for the tip → tests and review run again on K3's lineage.
- Adopted drift after approval → K' derived from K2 → E(K2), R(K2) not for the tip → tests, then
  review, run again; implement does not.

**Rounds.** Each edge (rejector → producer) counts its jumps. A jump that would exceed
`max_rounds` pauses (`RoundLimit`) instead; `max_rounds: 5` means the initial attempt plus five
revisions.

**Stalls** (owner's choice B), each pausing (`Stalled`):

- A verdict step returns `ChangesRequested` and resolves none of its own previously open findings
  (applies only when there were previous findings, and only within the same review step).
- A change attempt given feedback produces a tree equal to its input.
- Two consecutive `TestsFailed` from the same tests step with the same failing commands on the
  same tree.

**Replay determinism.** For every durable prefix of a log, `machine::next` on the replayed prefix
equals the decision the uninterrupted run made at that point (a property test over recorded
release-test logs).

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
`contract` builds from the current artifacts a step requires plus its history, naming each by ID
and tree:

| Step | Receives |
|---|---|
| document (research) | task; history (the rejection and findings) |
| verdict on research | task; research `D_r`; its own open findings |
| document (plan) | task; approved research `D_r`; history |
| verdict on a plan | task; approved research; plan `D_p`; its own open findings |
| change | task; approved plan (and the approved research it builds on); history (findings, or failing evidence) |
| tests | task; approved plan; the patch base..tip |
| verdict on a change | task; approved plan; the patch base..K; evidence `E(K)`; its own open findings |

In general a step receives its own `inputs` in full and, for documents further up the chain,
their approved text too *(builder's call: whether to cut upstream documents to a summary when the
prompt budget is tight; the step's direct inputs are never cut)*.

The patch is `git diff --binary <base> <K>`: everything this run changed, including tester files.
Over the prompt budget, the prompt carries the `--stat`, and the full patch is a file **inside the
reviewer's working directory** (the scratch clone below), which every provider reads without an
approval; the prompt names it and requires the review to cover it. Write steps get the same file
under `<run>/attempts/<id>/` and can also run `git diff` themselves.

### Read-only steps: isolation, not a sandbox

v1 promises **isolation of the user's workspace, plus detection**, not OS-level write prevention:

- Every document and verdict attempt runs in a **scratch clone**: `git clone --shared --no-checkout` of
  the repo into `<run>/attempts/<id>/tree`, then a detached checkout of the candidate's commit, with
  the vendor's cwd at the same relative path as `--cwd`. The reviewer sees exactly K, and an edit,
  an index change or a ref change lands in the clone, not in the user's checkout or refs.
- The runner **denies every approval** a read-only step raises.
- The clone's tree is compared before and after; any change is `AccessViolation`. The verdict is
  discarded and the run pauses with the diff. Nothing an `AccessViolation` produced is adopted.
- **Not promised**: a vendor writing outside its working directory by absolute path. Such a write
  into the user's workspace is still caught by the drift check at the next transition. The
  documentation says this plainly.

### Snapshots

`snapshot::take` builds a tree without touching the user's index, HEAD or branches. Every git
command it runs sets `-c core.fsmonitor=false -c core.untrackedCache=false -c core.ignoreStat=false`,
so no cache can hide a change.

1. Copy the real index (`git rev-parse --git-path index`) to `<run>/index.tmp`, so tracked
   paths, including tracked files that `.gitignore` covers and staged additions, are kept.
2. In the copy, **clear every `assume-unchanged` flag** (`git ls-files -v` lists them;
   `git update-index --no-assume-unchanged` on those paths), so a flagged file's live contents
   are read, not its indexed ones.
3. `GIT_INDEX_FILE=<run>/index.tmp git add -A` over the **whole work tree** (top level, not
   `--cwd`), overlaying modifications, deletions and untracked non-ignored files.
4. `git write-tree`; then `git commit-tree` and `git update-ref refs/octet-fleet/<run>/<seq>`, so
   the tree survives `git gc` and a scratch clone can check it out.

Rules: **sparse checkouts are refused** at validation (`core.sparseCheckout` set, or any index
entry with `skip-worktree`): clearing the flag would record the absent files as deleted, and
keeping it would hide edits. The run's own state lives in `<data_dir>`, never in the work tree.
A dirty start is fine: the base candidate includes it, and the patch is relative to it.
Submodules are captured as their recorded gitlink; content changes inside a submodule are not
seen (documented). A failed snapshot is a `LocalFailure`. *(Builder's call: removing the refs of
finished runs.)*

### Test evidence

The tests step has two halves:

1. **The tester's turn** (write, in the live workspace) authors or selects tests. If it changed
   the tree, the result is a derived candidate (origin `tests`).
2. **Octet's evidence runner**, on the tip K with tree T. **The guarantee: every command started
   on T, and the tree was T when each command ended.**
   - Snapshot; it must equal T (otherwise `Drift`).
   - For each command in order: register it (see "Process ownership"), run it with `sh -c` in the
     workspace (own process group, killed at `timeout`), then **snapshot at the command boundary**.
   - On the first boundary where the tree is not T, the record so far is void: the new tree
     becomes a derived candidate (origin `evidence`) and the **whole** list starts again on it. At
     most 2 such restarts per tests attempt; a third pauses (`UnstableTests`).
   - When every command has run with every boundary equal to T: the evidence record `E(K)` lists
     each command, exit status (or timeout), duration, and the last 16 KiB of output. All zero →
     `TestsPassed`; any other → `TestsFailed`.

This catches a command that changes a fixture and a later one that restores it (the first
boundary differs). It does not see a change that a single command makes and undoes within its own
run; the documentation states the guarantee in exactly the words above. Running commands in an
immutable checkout was rejected for v1: it loses the work tree's build caches, so every round
rebuilds from nothing.

The tester's own turn is never evidence. Only this runner writes evidence, and only an evidence
record for the tip counts.

### Process ownership

**No writer before registration.** Every process that can write the workspace is registered in
the log before it is given anything to do:

- **Vendors**: `attempt_started` is durable before `Session::open`. The session reports the
  vendor's identity (process group, leader PID, and the leader's start time) through a new
  read-only accessor; `octet-proc` already spawns vendors with `process_group(0)`. The runner
  makes `attempt_session {attempt, identity}` durable, and **only then sends the first prompt**.
  A vendor that has had no prompt has done no work, and its stdin pipe closes when Octet dies, so
  it exits.
- **Test commands**: each is spawned as `sh -c 'read _ || exit 125; exec sh -c "$1"' sh CMD` with
  a stdin pipe. The runner makes `evidence_command {attempt, command, identity}` durable, then
  writes the go line and closes stdin. If Octet dies before that, `read` sees end of file and the
  command never starts.

**Locks.** While driving a run, Octet holds OS locks (`File::lock`) on `<run>/lock` and on
`<data_dir>/fleet/worktrees/<hash of canonical top level>.lock`. Locks die with the process.

**The worktree ownership record**, `<data_dir>/fleet/worktrees/<hash>.json`, names the run that
last drove the work tree and whether it has an unfinished write attempt. It is written before the
first write attempt and on every change. A **new run** on that work tree is refused while the lock
is held, or while the record names a run with an unfinished write attempt or registered processes
not yet shown to be gone; the message names the run and says to `resume` or `stop` it.

**Leftover processes.** Before a recovery snapshot or any new writer, the runner checks every
registered identity of unfinished attempts. A process counts as Octet's only if a process with
the logged leader PID exists **and** its start time matches the logged one; a group whose leader
is gone is checked member by member the same way. *(Builder's call: reading start times on macOS
and Linux, for example `ps -o lstart=`.)* If any is alive, the run pauses (`ProcessesRunning`)
naming them. `--decision kill` signals only processes verified this way (SIGTERM, SIGKILL after
5 s, then checks again); a group that cannot be verified is reported and left alone.

**Drift.** At every transition (before each attempt, before acceptance, on resume, and the final
observation before `completed`), the live tree is compared with the tip's tree. A difference the
run cannot account for pauses (`Drift`) with the diff.

### Failures

`step` classifies how a turn ends:

| Class | Examples | Action |
|---|---|---|
| `Transient` | usage or rate limit, overloaded, network error, the vendor process exiting mid-turn | retry, if the tree is unchanged |
| `ConfigError` | model not found, binary missing, not logged in | fail the run at once, with the error |
| `LocalFailure` | `Event::Error` from the core pump (journal write, a stalled consumer), snapshot failure | pause |
| `Unknown` | anything not matched | pause |

*(Builder's call: the per-provider match patterns, from the vendors' real error texts.)*
Retries are logged with their count and next eligible time (1, 5, then 15 minutes, or the reset
time a usage-limit error names), so a restarted run waits out the backoff. Before accepting a
turn's output, `step` waits for the vendor's background tasks to finish and the grace period to
pass, reusing the `Background` tracker from `headless.rs` (moved into `octet-core`).

### Persistence: the run directory

`<data_dir>/fleet/<run-id>/`, owner-only:

- `run.json`, written once: the resolved workflow (inlined), its hash, the task, the work tree and
  `--cwd`, the base candidate, Octet's version.
- `log.jsonl`: the authority on accepted work. Each record is synced before the runner acts on it.
  On replay, a torn trailing record is cut back to the last newline before anything is appended.
  - `attempt_started {attempt, step, round, input_candidate, requires, history}`, before
    `Session::open`; the attempt directory `<run>/attempts/<attempt>/` is derived from the ID.
  - `attempt_session {attempt, journal, identity}`, before the first prompt;
    `vendor_session {attempt, id}`, as soon as known (the latest non-empty `Event::Ready`).
  - `candidate {id, tree, parent, origin}`, after its ref exists.
  - `evidence_command {attempt, run, index, command, identity}`, before the go line;
    `evidence_result {attempt, run, index, exit, boundary_tree}`.
  - `attempt_finished {attempt, outcome, candidate, artifacts}`, only after its artifact files are
    synced and its candidate is recorded.
  - `retry {attempt, class, count, eligible_at}`, `paused {reason, detail}`, `decision {kind}`,
    `completed {candidate, tree, evidence, approval, observed_tree}`, `stopped`, `failed {error}`.
- `artifacts/`: plans, verdicts, patches, evidence, by artifact ID.
- `attempts/<id>/`: the child journal, the scratch clone (removed once the attempt is accepted), the
  patch file.

*(Builder's call: the run ID format, sortable by time and typeable.)*

The guarantee is precisely: **a step whose `attempt_finished` is durable is never repeated.** A child
that finished just before the parent crashed is an unfinished attempt and goes to reconciliation.

### Recovery and decisions

`octet fleet resume <id>` takes the locks, replays the log, checks leftover processes, takes a
snapshot, then acts on the last unfinished thing:

| State found | What happens |
|---|---|
| last attempt durable | drift check, then `machine::next` |
| read-only attempt unfinished | discard its clone; a new attempt of the same step |
| write attempt unfinished, no `attempt_session` | its vendor never had a prompt: a new attempt |
| write attempt unfinished, tree unchanged | a new attempt, continuing its vendor session if logged |
| write attempt unfinished, tree changed (crash, signal, **or** transient failure) | pause `Reconcile` |
| evidence run unfinished | pause `ProcessesRunning` if a command is alive, else the whole list again on a fresh snapshot |

Every pause has its decisions and, after the decision is logged, an exact next action:

| Pause | Decision → next action |
|---|---|
| `Reconcile` (change step) | `keep` → continue the same vendor session (or a fresh one if none was logged), told it was interrupted and its partial changes are in place · `adopt` → the current tree becomes the change's candidate; next is `machine::next` (tests) · `stop` |
| `Reconcile` (tests step) | `keep` → continue the tester · `adopt` → the tester's edits become a derived candidate; next is the evidence runner on it · `stop` |
| `RoundLimit`, `Stalled` | `continue` → the pending jump is taken (one round past the limit, or past the stall) · `rounds N` → raise the cap, then the same · `stop` |
| `UnstableTests` | `retry` → the evidence runner starts afresh on the current tip, restart count reset · `stop` |
| `Drift` | `adopt` → a derived candidate (origin `drift`); next is `machine::next` · `stop` |
| `AccessViolation` | `retry` → a new attempt of the same step in a fresh clone · `stop` |
| `ProcessesRunning` | `kill` → verified processes signalled, then the check runs again · `wait` → the check runs again · `stop` (refused while verified processes are alive) |
| `InvalidOutput` | `retry` → a new attempt of the same step · `stop` |
| `Transient` exhausted, `LocalFailure`, `Unknown` | `retry` → a new attempt of the same step (if the tree changed, `Reconcile` first) · `stop` |

No decision produces `TestsPassed` or `Approved`. `stop` records `stopped`, clears the work tree's
ownership record once no registered process is alive, and leaves the tree as it is, printing the
command that would discard the run's changes. `octet fleet stop RUN_ID` is `resume --decision stop`.

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
octet fleet stop RUN_ID
octet fleet status [RUN_ID]
octet fleet validate WORKFLOW [--cwd PATH]
```

Text output: a header per attempt (`▸ implement · round 2 · codex gpt-6-astra`), the child's
streamed text (control characters stripped, as `--print` does), each outcome, and at the end the
invariant's parts (candidate, tree, evidence, approval, observed tree) with
`git diff <base> <final>`. `--output json` writes every `FleetEvent` as a line, child events
wrapped with step and attempt. Exit codes: 0 completed, 1 failed, 2 usage or validation error,
3 paused (with the resume command), 130, 143.

`FleetEvent`: `RunStarted`, `AttemptStarted`, `Session { attempt, event }`, `Approval`,
`Candidate`, `EvidenceCommand`, `AttemptFinished { outcome }`, `Retrying`, `Paused { reason }`,
`NeedsDecision`, `Completed`.

### Testing

**Milestone 1 (before any provider is wired): the artifact transitions.** The pure machine,
driven by a scripted step executor, runs the trace `coder(K1) → tester adds a file (K2) →
evidence(K2) passes → reviewer approves R(K2) → Completed`, asserting: the coder ran once, the
reviewed patch contains the tester's file, the approval names the exact evidence ID, and replaying
every durable prefix of the log gives the same next action as the uninterrupted run. The other
worked traces in "Artifacts, candidates and the machine" are milestone-1 tests too.

- **Unit (pure):** validation (the approval gate, including a change step whose input names a
  document without its approval step, and a plan whose input skips the research approval; the
  completion gate; producer-only edges; wrong-kind references); the machine: requires vs history,
  the `D1 → rejection → D2 → review_research(D2)` and `P1 → rejection → P2 → review_plan(P2)`
  traces, implement never next before both approvals, derived candidates, staleness, rounds, every stall rule, every pause's decision and next
  action, the invariant; verdict validation order and precedence; torn-tail repair.
- **Snapshots** against real `git init` repos: tracked-but-ignored files kept, staged additions
  kept, an `assume-unchanged` file edited `v1 → v2` captured as `v2`, a sparse checkout refused,
  untracked files, deletions, the user's index and HEAD byte-identical afterwards, `--cwd` below
  the top level, refs created, a scratch clone checks out the candidate.
- **Scripted providers.** `protocol-child` gains a script-file mode: `OCTET_FLEET_SCRIPT` names a
  JSON file keyed by `<step>#<round>` (from the runner's prompt header) whose entries say which
  files to write, what to reply, or to hold (before or after its first write); every turn appends
  its prompt, its cwd and whether it was resumed to a log the test reads. It works under the
  codex and claude dialects.
- **Release tests** (process tests, `crates/octet/tests/fleet.rs`; fake Claude researching and
  reviewing, fake Codex coding and testing):
  1. **The full run**: review_research rejects the first research → research revises → approved
     → plan → review_plan approves → implement (its prompt carries the approved plan and the
     approved second research, never the rejected first) → the tester holds, Octet is SIGKILLed once
     `attempt_started test` is durable → resume re-runs the tester, **the coder ran once** → a
     command fails, **the run cannot complete** → implement round 2 (resumed session) → tests pass
     → review_change requests changes → implement → test → a **fresh** review approves; its
     prompt names the passing evidence's candidate, and its patch includes the tester's new file.
  2. **Wrong-revision approval**: command A passes, command B edits a fixture and passes, A fails on
     the new tree → the run cannot complete.
  3. **Change, use, restore**: A changes a tracked fixture, B passes with it, C restores it → the
     boundary after A voids the record; no `TestsPassed` names a record in which B ran on another
     tree.
  4. **Crash at the acceptance boundary**: kill after the child finishes but before
     `attempt_finished` is durable → reconciliation; kill after it is durable → the step is
     skipped.
  5. **Crash before registration**: kill Octet after the coder's vendor is spawned and before
     `attempt_session` is durable, with the fake holding before its first write; then both
     `resume` and a new `run` on the same work tree. Neither admits a second writer while ownership
     is unresolved; the held fake exits on its closed stdin and never writes.
  6. **Interrupted tester**: kill after the tester writes a file, before any command → no
     `--decision` yields passing evidence without the commands running.
  7. **Workspace ownership**: a second run on the same work tree is refused; an edit to a paused
     run's tree surfaces as `Drift` before the next step.
  8. **The completion boundary**: an edit to the workspace after the final approval, before
     completion → `Drift`, not `Completed`; the `completed` record of a clean run carries the
     observed tree.
  Plus: a stall pause (exit 3), a round-limit pause, an access violation (a reviewer that writes a
  file in its clone), a leftover test process found on resume, a transient retry that survives a
  restart and waits out its backoff, `UnstableTests → retry`.
- **Real providers** before merge: one small run on a throwaway repo, with each of Claude, Codex,
  opencode and Antigravity driving at least one step.

## Left to the builder

Marked inline: ref cleanup, the run ID format, the per-provider error patterns, reading process
start times per platform. Also: module boundaries below the table, prompt wording (the JSON
contract is fixed), text layout, `FleetEvent` naming, and Antigravity's workspace-config trust in
scratch clones (they are new paths, so their `.agents` configuration does not load there; v1
accepts that).

## Changed after review

From the owner's request after revision 3 (revision 4):

- **A research stage with its own review loop** before planning: research ⇄ review_research →
  plan ⇄ review_plan → implement → test → review_change. The `plan` kind became `document`, used
  by both the researcher and the planner; steps name their documents through `inputs`, each with
  the verdict step that approves it, so a document can only be consumed once approved and the
  coder transitively requires both approvals. No new scheduler concept was needed.

From the owner's second review (revision 3):

- **Candidate lineage**: a change has a stable identity; tester edits, evidence-run edits and
  adopted drift make derived candidates that do not re-run the coder; evidence and approvals bind
  to an exact candidate, which must be the tip.
- **Evidence guarantee stated and checked at every command boundary**, with full restarts; the
  change/use/restore case is a release test.
- **Requires vs history** for every artifact; rejections are outcomes, not artifacts; the
  `P1 → rejection → P2` trace is a unit test.
- **Snapshots** clear `assume-unchanged` in the temporary index, disable fsmonitor and the
  untracked cache, and refuse sparse checkouts.
- **Registration before work**: no prompt before `attempt_session` is durable; test commands wait
  for a go line after `evidence_command`; a worktree ownership record makes a new run check an
  earlier run's unresolved attempts; signals go only to processes verified by PID and start time.
- **Edges go to the producer**, so every negative outcome has a route; every pause has decisions
  and an exact next action; `UnstableTests` has `retry`; `fleet stop` added.
- **Plan changes**: milestone 1 is the tester-authored-file trace on the pure machine, with
  durable-prefix replay; release tests 3, 5 and 8 added; the completion boundary is the runner's
  persisted last observation.

From the owner's first review (revision 2): the frozen-candidate evidence model, recovery by step
kind, the plan gate, isolation over prevention for read-only steps, the trust decision, snapshots
from the real index, OS locks and drift checks, failure classes with persisted backoff, the log's
write order, verdict precedence, release tests for wrong-revision approval, the acceptance
boundary, an interrupted tester and workspace ownership.

From the builder check: carried-over findings tracked by Octet-assigned IDs; the oversized patch as
a file inside the reviewer's clone; leftover vendor processes checked on resume; all four
providers on a real run before merge; this design replaces v3 §9.
