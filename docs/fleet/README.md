# Fleet workflows

`octet fleet` runs a multi-step, multi-provider workflow on a git work tree: research ⇄ research
review → plan ⇄ plan review → implement → test → final review, looping back after each rejection
until the final reviewer approves. Each step can use a different provider, model, effort and
permission mode. Rust decides when coding may start and whether the run may finish; no model
decides either.

A run has one ID. It survives a pause, Ctrl+C, a crash or a reboot: `octet fleet resume ID`
picks it up where the log says it stopped. Fleet is headless for now (no terminal UI view yet).

The design, with its reasoning, is
[the fleet workflows spec](../superpowers/specs/2026-11-22-fleet-workflows-design.md). This guide
says how to use it.

- [Quick start](#quick-start)
- [Writing a workflow](#writing-a-workflow)
- [The trust prompt](#the-trust-prompt)
- [Watching a run, and write steps' approvals](#watching-a-run-and-write-steps-approvals)
- [Pauses and decisions](#pauses-and-decisions)
- [Failures and retries](#failures-and-retries)
- [Exit codes](#exit-codes)
- [What a completed run guarantees](#what-a-completed-run-guarantees)
- [Where things live](#where-things-live)
- [Providers](#providers)
- [Not in v1](#not-in-v1)

## Quick start

```sh
# Copy the shipped workflow into the project and adjust its profiles.
mkdir -p .octet/workflows
cp docs/fleet/research-plan-build.yaml .octet/workflows/

octet fleet validate research-plan-build          # static checks; starts nothing
octet fleet run research-plan-build --task "Add a --json flag to the export command"
octet fleet status                                # every run, one line each
octet fleet resume f1795467341-a39b               # after a pause, Ctrl+C or crash
octet fleet stop f1795467341-a39b                 # end a run; the tree is left as it is
```

```
octet fleet run WORKFLOW (--task TEXT | --task-file PATH | --task -) [--cwd PATH]
                 [--journal-dir PATH] [--trust] [--output text|json]
octet fleet resume RUN_ID [--decision D] [--rounds N] [--output text|json]
octet fleet stop RUN_ID
octet fleet status [RUN_ID]
octet fleet validate WORKFLOW [--cwd PATH]
```

`WORKFLOW` is a name, looked up as `.octet/workflows/NAME.yaml` under `--cwd` (default: the
current directory), or a path (anything with a `/` or ending in `.yaml`/`.yml`). Every command
takes `--journal-dir PATH`, the data directory where runs live (see
[Where things live](#where-things-live)); the commands Octet prints back name it when you gave
one.

`--task -` reads the task from stdin. Nothing can then be asked on the terminal, so a workflow
not trusted before needs `--trust`, and write steps' approvals are denied.

## Writing a workflow

A workflow is a YAML file. The shipped example is
[`research-plan-build.yaml`](research-plan-build.yaml):

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
  researcher: { kind: document, instructions: "Investigate the code: what exists, …" }
  planner:    { kind: document, instructions: "Write an implementation plan from …" }
  reviewer:   { kind: verdict,  instructions: "Review the artifact you are given …" }
  coder:      { kind: change,   instructions: "Implement the approved plan …" }
  tester:     { kind: tests,    instructions: "Write or select the tests …" }
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

Every key is checked: a misspelt key (`approval_form:`) is a validation error naming it, never
silently ignored.

### Top level

| Key | Meaning |
|---|---|
| `version` | Must be `1`. |
| `name` | The workflow's name, shown in `status`. |
| `limits.max_rounds` | Backward jumps allowed per edge (default 5: the first attempt plus five revisions). |
| `limits.transient_retries` | Automatic retries of a transient failure per attempt (default 3). |
| `profiles` | Named provider settings, below. |
| `roles` | Named kinds of work, below. |
| `steps` | The steps, in order. |

### Profiles

| Key | Meaning |
|---|---|
| `engine` | `claude`, `codex`, `opencode` or `antigravity`. |
| `model` | The model ID. Not checked statically: a model the vendor refuses fails the run at the first turn. |
| `effort` | Reasoning effort, checked against what the engine takes. |
| `mode` | `ask` (default), `accept-edits`, `auto` or `full-access`. |
| `binary` | The vendor CLI to run (default: the provider's usual binary on `PATH`). |
| `approval_timeout` | How long an approval request may wait for an answer (default `2m`). |

Durations are `<n>s`, `<n>m` or `<n>h`, with `n` at least 1.

A `binary` can be a wrapper script, which is how to pin vendor settings the profile has no key
for. For example, to run Codex on a model its config file does not name:

```sh
#!/bin/sh
exec codex -c model="gpt-5.6-terra" -c model_reasoning_effort="low" "$@"
```

### Roles

A role is `kind` and `instructions`. The instructions open the step's prompt; Octet adds the
task, the artifacts the step builds on and, for verdicts, the reply format.

### Steps

Every step has an `id` (letters, digits, `_` and `-`), a `role` and a `profile`. What else it
takes depends on its role's kind:

| Kind | Keys (required) | Produces | Accepted outcomes |
|---|---|---|---|
| `document` | optional `inputs` | a document (the reply text): research, a plan, … | produced |
| `verdict` on a document | `reviews: <document step>` | the document's approval | approved, changes requested |
| `change` | `inputs` (at least one) | a change: a new candidate tree | produced |
| `tests` | `change_from`, `commands`, optional `timeout` (default `20m`) | the tester's edits, then Octet's test evidence | tests passed, tests failed |
| `verdict` on a change | `reviews: <change step>`, `evidence_from: <tests step>` | the change's approval | approved, changes requested |

**`inputs`** lists the documents a step builds on, each as `{from: <document step>,
approval_from: <verdict step>}`. A document can only be consumed through its approval: there is
no way to name a document without the verdict step that approves it, so nothing downstream of
research can start before research is approved. An entry with `from` and no `approval_from` is a
validation error.

**Edges go to the producer.** A negative outcome always returns to the step that produced what
was rejected: a document review's rejection to its `reviews` step, a change review's rejection to
its `reviews` step, a test failure to its `change_from` step. You may spell the edge
(`on_changes_requested`, `on_tests_failed`) for readability; it must then name that producer.

**`session`**: `fresh` (the default for document and verdict steps) starts a new vendor session
for every attempt; `continue` (the default for change and tests steps) resumes the step's own last
vendor session, so a coder sent back by a failing test or a review keeps its context.

**Read-only is not a setting.** Document and verdict steps are always read-only and run in a
scratch clone (see [the guarantees](#read-only-steps-isolation-not-a-sandbox)); their profiles
must have mode `ask` (or none). Change and tests steps always write, in the live work tree.

**`commands`** are run by Octet itself, in order, in the work tree, each with `sh -c` in its own
process group, killed (SIGTERM, then SIGKILL after 5 s) at `timeout`. Only the workflow's
commands run; Octet never runs a command a model proposes as evidence.

### What `validate` and `run` check

`octet fleet validate` is static: it starts no vendor and runs no command. A run refuses to start
unless:

- `version` is 1; step IDs are unique; every role, profile and reference resolves to an
  **earlier** step of the right kind; any spelled edge names the producer; every engine is known,
  its binary is found, `effort` is one the engine takes, and `mode` is a mode.
- **The approval gate**: every `inputs` entry names a document step and an `approval_from`
  verdict step whose `reviews` is that same document step; every change step has at least one
  input.
- **The completion gate**: the last step is a verdict reviewing a change step, with
  `evidence_from` a tests step whose `change_from` is that same change step.
- Document and verdict profiles have mode `ask` (or none); anything else is rejected, not
  overridden.
- `commands` is non-empty for tests steps.

`run` also checks that `--cwd` is in a git work tree that is not a sparse checkout, and that no
other run still owns the work tree (see [Process ownership](#process-ownership)).

The generic guarantee is: *a change step runs only when every document in its input chain has a
current approval.* A minimal workflow with one document, its review and a coder is valid.
"Implementation starts only when the research **and** the plan are approved" is a property of the
shipped `research-plan-build` workflow, whose coder's chain is plan → research.

### The verdict reply

Octet appends the reply format to every verdict prompt. The reviewer ends its reply with one
fenced JSON block:

```json
{"verdict": "approved" | "changes_requested",
 "new_findings": [{"severity": "blocking" | "minor", "summary": "…", "file": "…"}],
 "previous": [{"id": "review_change/F3", "status": "resolved" | "open"}]}
```

- Octet assigns finding IDs (`<step>/F<n>`), so a reviewer cannot renumber.
- `previous` must list exactly the step's findings still open before this round (all of them,
  each once). The reviewer receives every open finding with its ID.
- `approved` requires no open **blocking** finding afterwards (open minor findings are allowed and
  reported). `changes_requested` requires at least one open finding afterwards.
- A reply that fails these checks gets one corrective prompt in the same session; if it is still
  invalid the run pauses (`invalid_output`). An invalid reply never counts as a round.

## The trust prompt

Running a project's workflow is a trust decision: it names binaries to execute and shell
commands to run. At `run`, Octet resolves the workflow and shows what it would execute, each
step's absolute binary path and mode, and every test command:

```
Workflow real-run would run:
  plan: claude /Users/me/.local/bin/claude (mode ask)
  review_plan: antigravity /Users/me/.local/bin/agy (mode ask)
  implement: codex /Users/me/bin/codex-terra.sh (mode accept-edits)
  test: opencode /Users/me/.opencode/bin/opencode (mode accept-edits)
      $ python3 -m unittest -q
  review_change: claude /Users/me/.local/bin/claude (mode ask)
Trust this workflow and run it? [y/N]
```

On a terminal you answer y or n. Otherwise `--trust` is required (a refusal exits 2 and starts
nothing). The answer is stored under `<data_dir>/fleet/trusted/<hash>`, keyed by the hash of the
resolved content, so a changed workflow asks again. `resume` runs the content frozen in the run's
`run.json`, which was already trusted.

## Watching a run, and write steps' approvals

Text output shows a header per attempt, the vendor's streamed text (control characters
stripped), each outcome, and at the end the parts of the completion record:

```
run f1795467341-a39b
▸ plan · round 1 · claude claude-sonnet-5-5
…
  → document_produced
▸ implement · round 1 · codex
…
  candidate 1 (change)
  → change_produced
▸ test · round 1 · opencode opencode/deepseek-v4.1-flash-free
  $ python3 -m unittest -q
  → tests_passed
▸ review_change · round 1 · claude claude-sonnet-5-5
…
  → change_approved
completed f1795467341-a39b
  candidate 2
  tree b578594907aa1f29d92e03ab9034175c16e47f6d
  evidence artifact 5
  approval artifact 6
  observed tree b578594907aa1f29d92e03ab9034175c16e47f6d
  git diff 5393cdf80cbdf44e72474843f748244f1dd3b233 b578594907aa1f29d92e03ab9034175c16e47f6d
```

The last line shows everything the run changed. Fleet never commits, branches or pushes: the
change is left in the work tree for you.

`--output json` writes every event as one JSON line instead (`run_started`, `attempt_started`,
`session` with the vendor's own event and the step, `candidate`, `evidence_command`,
`attempt_finished`, `retrying`, `paused`, `needs_decision`, `completed`); guidance then goes to
stderr.

**Approvals.** Read-only steps never ask you: every approval they raise is denied. Write steps
use their profile's mode. On a terminal, `octet fleet` asks y/n on stderr, showing the action,
the command or files, and what allowing grants. With no terminal, they are denied with a warning,
as `octet --print` does. For unattended runs, set `mode: auto` or `full-access` in the profile;
the trust prompt shows it.

## Pauses and decisions

When the run cannot decide what comes next on its own, it pauses: it records why, prints the
command that resumes it, and exits 3. On a terminal you choose the decision at once; otherwise
pass it to `resume` (off a terminal, resuming a run that awaits a decision without `--decision`
exits 2 and names the decisions it offers):

```sh
octet fleet resume RUN_ID --decision keep
octet fleet resume RUN_ID --decision rounds --rounds 8
```

Every pause can also be ended with `stop`. After a decision is logged, the next action is exact:

| Pause | Why | Decisions → next action |
|---|---|---|
| `reconcile` (change step) | A coder's attempt ended unfinished (crash, signal or transient failure) and left the tree changed. | `keep` → continue the same vendor session (or a fresh one if none was logged), told it was interrupted and its partial changes are in place · `adopt` → the current tree becomes the change's candidate; next is the tests step · `stop` |
| `reconcile` (tests step) | The tester's turn ended unfinished and left the tree changed. | `keep` → continue the tester · `adopt` → the tester's edits become a derived candidate; next is the evidence runner on it · `stop` |
| `round_limit` | A jump back would exceed `max_rounds` on that edge. | `continue` → the pending jump is taken (one round past the limit) · `rounds N` → raise the cap, then the same · `stop` |
| `stalled` | A round made no progress (below). | `continue` → go on past the stall · `rounds N` · `stop` |
| `unstable_tests` | The tree changed during the test commands on three runs of the list. | `retry` → the evidence runner starts afresh on the current tip, restart count reset · `stop` |
| `drift` | The live tree is not the tree the run last recorded. | `adopt` → the live tree becomes a derived candidate (origin `drift`); next is whatever step is now due: once a change exists, its tests and then its review run again on the new candidate (the coder does not); before that, the run goes on to the next document step · `stop` |
| `access_violation` | A read-only step changed its scratch clone. Nothing it produced is used. | `retry` → a new attempt of the same step in a fresh clone · `stop` |
| `processes_running` | A process verified as the run's is still alive. | `kill` → verified processes signalled, then the check runs again · `wait` → the check runs again · `stop` (refused while verified processes are alive) |
| `processes_unverified` | A process in a registered group is alive but cannot be shown to be the run's. | `release` → the listed PIDs are logged as not the run's, then the check runs again · `wait` · `stop` (refused until released) |
| `invalid_output` | A reply stayed invalid after the corrective prompt. | `retry` → a new attempt of the same step · `stop` |
| `transient_exhausted`, `local_failure`, `unknown` | Retries ran out, Octet's own side failed, or a failure Octet does not recognise. | `retry` → a new attempt of the same step (if the tree changed, `reconcile` first) · `stop` |
| `interrupted` | Ctrl+C or SIGTERM. | No decision: `resume` goes straight on (below). |

**Stalls** pause early, before the round cap:

- a verdict step requests changes and resolves none of its own previously open findings;
- a change attempt given feedback leaves the tree exactly as it was given it;
- the same tests step fails twice in a row with the same failing commands on the same tree.

**No decision produces `TestsPassed` or `Approved`.** Only Octet's own evidence runner can
produce passing tests, and only a valid review can produce an approval.

**Interrupts.** Ctrl+C (or SIGTERM) interrupts the running turn or command, records it, and exits
130 (or 143). `resume` needs no decision for it; what the log and the tree show decides:

- a read-only step runs again in a fresh clone;
- a write step whose vendor never started (Octet had not released it): tree unchanged → it runs
  again; tree changed → `drift`;
- a coder or tester that left the tree unchanged runs again (a coder continues its vendor
  session); one that changed the tree pauses `reconcile`;
- an interrupted evidence run starts its command list again from the first command, without
  running the tester again, if the tree is still its candidate; otherwise → `drift`.

The same rules apply after a crash or a reboot.

**Stopping.** `stop` (or `octet fleet stop RUN_ID`) records the run as stopped and leaves the tree
as it is. It prints the command that would discard the run's changes:

```
git -C TOP read-tree --reset -u BASE_TREE && git -C TOP clean -fd && git -C TOP reset -q
```

That restores the work tree to the run's starting snapshot (so it also discards any edit made
since), removes untracked files that are not in that snapshot (ignored files are kept) and leaves
the index unstaged. Octet never runs it for you. `stop` is refused (exit 3) while a verified
process of the run is alive or an unverified one has not been released.

## Failures and retries

How a turn that did not reply failed decides what happens:

| Class | Examples | Action |
|---|---|---|
| Transient | usage or rate limit, overloaded, network error, the vendor process exiting mid-turn | retry, if the tree is unchanged |
| Config error | model not found, binary missing, not logged in | fail the run at once, with the error |
| Local failure | Octet's journal or disk, a snapshot that failed | pause |
| Unknown | anything not matched | pause |

Retries wait 1, 5, then 15 minutes, or until the reset time a usage-limit error names when that
is later, but never more than a day. Claude names its reset as a Unix time (`…|<epoch>`). Codex
names a local clock time ("…or try again at 9:04 AM."): Octet reads the local clock (it runs
`date` once) and waits for the next time the clock shows it, today if still ahead, otherwise
tomorrow; across a daylight-saving change that can be an hour off. A text whose time cannot be
read falls back to the backoff. The wait is in the log, so a restarted run still waits it out.
After `transient_retries`, the run pauses `transient_exhausted`; `resume --decision retry` tries
again.

The errors Octet recognises per provider (from their real texts) are in
`crates/octet-fleet/src/classify.rs`, each a test case naming where it was seen.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Completed (or stopped by `stop`). |
| 1 | Failed: a config error, or a failure the run cannot pause on. |
| 2 | Usage or validation error; an untrusted workflow off a terminal; a work tree that is not a git work tree or is a sparse checkout; `resume` of a paused run off a terminal without the `--decision` it needs, or with one the pause does not offer; no such run; or a run refused because another run owns the work tree. Nothing ran. |
| 3 | Paused; the resume command is printed. Also a `stop` the run refuses. |
| 130 | Interrupted by SIGINT (Ctrl+C). |
| 143 | Interrupted by SIGTERM. |

## What a completed run guarantees

The guarantees below are quoted from the design (the `>` blocks are its exact words; `<run>` is
the run's directory). The plain text around them explains and is not part of the quotation.

### The invariant

> The run is recorded `Completed` only when:
>
> **The final reviewer approved candidate K with tree T, under evidence record E and the
> document approvals A (in the shipped workflow, the plan's and through it the research's); E is
> a complete passing record for T (for every command, the tree Octet observed immediately before
> releasing it and immediately after it exited, both persisted in E, equals T); E and
> the approval were produced under A, each of which is the current approval of the current
> document; and the runner's last observation of the live workspace,
> taken immediately before writing `completed` and persisted in that record, equals T.**

The `completed` lines at the end of a run are these parts: candidate K, tree T, evidence E,
approval, and the observed tree.

### The test evidence

The tester's own turn is never evidence. After it, Octet snapshots the tree (the tester's edits
become a derived candidate) and runs the workflow's commands itself.

> **The guarantee: for every command, the tree Octet observed immediately before releasing it and
> immediately after it exited equals T, and both observations are persisted in the evidence
> record.**

On the first observation that differs, the record so far is void: the observed tree becomes a
new candidate and the whole command list starts again (at most 2 restarts; a third pauses
`unstable_tests`).

> This catches a command that changes a fixture and a later one that uses and restores it (the
> first post-observation differs), and an edit made by anyone between two commands (the next
> pre-observation differs). It cannot see a change made and undone within a single command's
> lifetime, by that command or by anyone else

### Read-only steps: isolation, not a sandbox

K is the candidate the step is given.

> v1 promises **isolation of the user's workspace, plus detection**, not OS-level write
> prevention:
>
> - Every document and verdict attempt runs in a **scratch clone**: `git clone --shared
>   --no-checkout` of the repo into `<run>/attempts/<id>/tree`, then a detached checkout of the
>   candidate's commit, with the vendor's cwd at the same relative path as `--cwd`. The reviewer
>   sees exactly K, and an edit, an index change or a ref change lands in the clone, not in the
>   user's checkout or refs.
> - The runner **denies every approval** a read-only step raises.
> - The clone's tree is compared before and after; any change is `AccessViolation`. The verdict
>   is discarded and the run pauses with the diff. Nothing an `AccessViolation` produced is
>   adopted.
> - **Not promised**: a vendor writing outside its working directory by absolute path. Such a
>   write into the user's workspace is still caught by the drift check at the next transition.

### Snapshots: sparse checkouts and submodules

> `snapshot::take` builds a tree without touching the user's index, HEAD or branches.

Each snapshot is kept alive by a ref, `refs/octet-fleet/<run>/<seq>`.

> **sparse checkouts are refused** at validation (`core.sparseCheckout` set, or any index entry
> with `skip-worktree`): clearing the flag would record the absent files as deleted, and keeping
> it would hide edits. The run's own state lives in `<data_dir>`, never in the work tree. A dirty
> start is fine: the base candidate includes it, and the patch is relative to it. Submodules are
> captured as their recorded gitlink; content changes inside a submodule are not seen

"At validation" means when `octet fleet run` checks the work tree before it starts (exit 2);
`octet fleet validate` itself does not look at git.

### Accepted work is never repeated

> The guarantee is precisely: **a step whose `attempt_finished` is durable is never repeated.** A
> child that finished just before the parent crashed is an unfinished attempt and goes to
> reconciliation.

### Process ownership

> **No execution before registration.** No vendor binary and no test command runs until its
> identity is durable in the log. Both are started **held** by an Octet-controlled gate and
> released only afterwards.

Each vendor is started through a per-attempt gate script that records its PID and waits for
Octet's go-ahead; each test command waits on a pipe for its go line. Processes carry an
`OCTET_FLEET_ATTEMPT=<run>/<attempt>` marker in their environment. While a run is driven, Octet
holds a lock on the run and on the work tree, and keeps a record of every run that may still have
a writer on the work tree.

> A writer is admitted only when no run other than the one being driven is listed.

`run`, `resume` and `stop` all check that record first, and a refusal names the run to `resume`
or `stop` first. Octet signals only processes it verifies as the run's (the marker, or a PID and
start time it recorded):

> `kill` signals **only verified processes** (SIGTERM, SIGKILL after 5 s), then the check runs
> again.

## Where things live

`<data_dir>` is `--journal-dir`, else `$XDG_DATA_HOME/octet/rust-preview`, else
`~/.local/share/octet/rust-preview`.

| Path | What |
|---|---|
| `<data_dir>/fleet/<run-id>/run.json` | The resolved workflow, its hash, the task, the work tree, the starting snapshot. Written once. |
| `<data_dir>/fleet/<run-id>/log.jsonl` | The durable log: the authority on what was accepted. |
| `<data_dir>/fleet/<run-id>/artifacts/` | Documents, verdicts, patches and evidence, by artifact ID. |
| `<data_dir>/fleet/<run-id>/attempts/<n>/` | Each attempt's gate, process registry, vendor journal and (while it runs) scratch clone. |
| `<data_dir>/fleet/trusted/<hash>` | Workflows you trusted. |
| `<data_dir>/fleet/worktrees/<hash>.lock`, `.json` | The work tree's lock and ownership record. |
| `refs/octet-fleet/<run>/<seq>` in your repo | Keep-alive refs for the run's snapshots. |

Run IDs look like `f1795467341-a39b` (start time in seconds, then a random tag), so they sort by
time. Everything is owner-only. The log and the pause details record leftover processes with
their full command line, which may include anything a vendor passed on its command line.

## Providers

Checked on a real run on 2026-11-24, each driving at least one step: Claude Code 2.1.289, Codex
CLI 0.154.0, opencode 1.18.25 and Antigravity (`agy`) 1.5.5.

- **Claude**: a fresh session gets its ID with its first turn, so an attempt interrupted before
  that has no session to continue and starts fresh.
- **Codex**: if your `~/.codex/config.toml` names a model your account cannot use, point the
  profile's `binary` at a wrapper that pins one (above). Codex's usage window is small; see
  [Failures and retries](#failures-and-retries).
- **opencode**: its free models share one account-wide quota. Octet's own agents enforce the
  profile's mode, and a `continue` step resumes the opencode session. Fleet does not delete the
  opencode sessions a run creates; `opencode session delete <id>` does.
- **Antigravity**: read-only steps run in scratch clones, which are new paths, so a project's
  `.agents` configuration does not load there. Octet runs `agy` with its own settings folder
  under `<data_dir>/antigravity/`.

## Not in v1

- A terminal UI view of runs (the JSON event stream is there for one).
- Parallel steps, or edges other than back to the producer.
- Running test commands a model proposes; discarding partial edits automatically; committing,
  branching or pushing; showing runs on the phone.
- An OS sandbox for read-only steps (see above).
- Sparse checkouts (refused) and changes inside submodules (not seen).
- A time limit on a vendor's turn: a turn that never ends (a vendor retrying a spent quota on its
  own, for example) waits until you press Ctrl+C.
