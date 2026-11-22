# Fleet Workflows Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new `octet-fleet` crate and an `octet fleet` subcommand that run a YAML workflow (research ⇄ review → plan ⇄ review → implement → test → final review) across providers, looping until the final reviewer approves a tested candidate, with resumable, crash-safe runs.

**Architecture:** A pure `machine` decides the next action from a replayed append-only log; everything with side effects (git snapshots, sessions, test commands, locks) lives in separate modules that the `runner` composes. Vendors and test commands start held behind a gate and are released only after their identity is durable. The `octet` binary adds a headless front end.

**Tech Stack:** Rust 2024 (rust 1.98), tokio, serde + serde_json, serde_norway (YAML), thiserror, git CLI, `octet-core` sessions, `octet-proc` process groups, `octet-testkit` fake vendors.

**Spec:** `docs/superpowers/specs/2026-11-22-fleet-workflows-design.md` (revision 5). Read it first; this plan cites its sections by name and does not repeat them.

## Global Constraints

- Edition 2024, `rust-version = "1.98"`; workspace lints (clippy pedantic, `missing_docs`, `unreachable_pub`, `unwrap_used` outside tests); `#![forbid(unsafe_code)]` in `octet-fleet`.
- Gate: `make rust-check` (fmt, clippy `-D warnings`, rustdoc `-D warnings`, tests), run through `scripts/rust-env.sh`; Linux parity per the repo's Docker `rust:1.98.1` recipe before merge.
- New dependencies only: `serde = { version = "1", features = ["derive"] }`, `serde_norway = "0.9"` (both MIT/Apache; `deny.toml` unchanged).
- `octet-core` must not depend on `octet-fleet`. No change to `octet-core`'s `Session` API.
- Defaults: `max_rounds: 5` (backward jumps per edge), `transient_retries: 3`, retry waits 1, 5, 15 minutes (or a named reset time), evidence restarts at most 2, output tail 16 KiB, kill = SIGTERM then SIGKILL after 5 s.
- Gate: polls every 50 ms, gives up after 120 s, exits 125 without running anything; env `OCTET_FLEET_ATTEMPT=<run>/<attempt>`.
- Git invocations for snapshots always pass `-c core.fsmonitor=false -c core.untrackedCache=false -c core.ignoreStat=false`.
- Paths: runs at `<data_dir>/fleet/<run-id>/`; worktree lock and record at `<data_dir>/fleet/worktrees/<hash>.lock|.json`; trust at `<data_dir>/fleet/trusted/<hash>`; refs `refs/octet-fleet/<run>/<seq>`; workflow lookup `<cwd>/.octet/workflows/<name>.yaml`. `data_dir` is the existing `--journal-dir` resolution in `crates/octet/src/main.rs`.
- Exit codes: 0 completed, 1 failed, 2 usage or validation error, 3 paused, 130 SIGINT, 143 SIGTERM.
- Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A workflow YAML with a typo'd key** (`approval_form:`) must be a validation error naming the key, not a silently ignored field: `deny_unknown_fields` on every YAML struct (Task 1 test `unknown_keys_are_errors`).
2. **A task text or vendor reply containing control characters or a fake `[octet-fleet …]` header** must not change which script a fake runs or what the terminal shows: the header is the first line Octet writes, and text output strips control characters (Task 7 test `task_text_cannot_forge_the_header`; Task 15 test `vendor_text_is_plain`).
3. **A work tree path with spaces or non-ASCII characters**: snapshots, scratch clones and the worktree hash must work (Task 8 test `paths_with_spaces_and_unicode`).
4. **A test command that never exits** (a watcher, `sleep 999`) must be killed at `timeout` with its whole group, recorded as timed out, and count as failure (Task 11 test `a_hung_command_times_out_with_its_group`).
5. **Two `octet fleet resume` processes started at once on one run** (a double Enter, two terminals): the second must be refused by the run lock without touching the log (Task 10 test `a_second_driver_is_refused`).

---

## File Structure

```
crates/octet-fleet/
  Cargo.toml
  src/lib.rs            re-exports; FleetEvent
  src/definition.rs     YAML structs, parse, durations                     (Task 1)
  src/validate.rs       Host trait, validation, Resolved, workflow hash     (Task 2)
  src/record.rs         log Record enum and its value types                 (Task 3)
  src/state.rs          replay records into RunState                        (Task 3)
  src/machine.rs        next(), currency, routing                           (Tasks 4–5)
  src/recover.rs        pure recovery classification                        (Task 6)
  src/sim.rs            #[cfg(test)] scripted executor for machine traces   (Task 6)
  src/contract.rs       prompts, verdict parsing and validation             (Task 7)
  src/snapshot.rs       git trees, commits, refs, diffs, scratch clones     (Task 8)
  src/store.rs          run dir, durable log, torn tail, artifacts, run IDs (Task 9)
  src/ownership.rs      locks, worktree record, process verification        (Task 10)
  src/gate.rs           vendor gate script; held test commands              (Task 11)
  src/evidence.rs       evidence runner                                     (Task 11)
  src/step.rs           one attempt against a Session                       (Task 13)
  src/classify.rs       failure classes                                     (Task 13)
  src/runner.rs         run, resume, stop, decisions, completion            (Task 14)
docs/fleet/research-plan-build.yaml                                         (Task 2)
docs/fleet/README.md                                                        (Task 18)
crates/octet-proc/src/signal.rs   pid liveness and signalling               (Task 10)
crates/octet-core/src/background.rs  Background tracker moved from headless (Task 13)
crates/octet/src/fleet.rs         CLI front end                             (Task 15)
crates/octet/src/args.rs          `fleet` subcommand parsing                (Task 15)
crates/octet-testkit/src/scenario.rs  FLEET_SCRIPT_ENV constant             (Task 12)
crates/octet-testkit/src/bin/protocol-child/fleet.rs  script mode           (Task 12)
crates/octet/tests/fleet.rs       release tests                             (Tasks 16–17)
```

---

## Milestone 1 — the pure machine and validation

### Task 1: Crate scaffold and workflow parsing

**Files:**
- Create: `crates/octet-fleet/Cargo.toml`, `crates/octet-fleet/src/lib.rs`, `crates/octet-fleet/src/definition.rs`
- Modify: `Cargo.toml` (workspace `members`, `[workspace.dependencies]` entries `octet-fleet`, `serde`, `serde_norway`)

**Interfaces:**
- Produces:
  - `pub fn parse(yaml: &str) -> Result<Workflow, DefinitionError>`
  - `pub struct Workflow { pub version: u32, pub name: String, pub limits: Limits, pub profiles: BTreeMap<String, Profile>, pub roles: BTreeMap<String, Role>, pub steps: Vec<StepDef> }`
  - `pub struct Limits { pub max_rounds: u32, pub transient_retries: u32 }` (serde defaults 5 and 3)
  - `pub struct Profile { pub engine: String, pub model: Option<String>, pub effort: Option<String>, pub mode: Option<String>, pub binary: Option<PathBuf>, pub approval_timeout: Option<String> }`
  - `pub enum Kind { Document, Verdict, Change, Tests }` (serde `snake_case`)
  - `pub struct Role { pub kind: Kind, pub instructions: String }`
  - `pub enum SessionPolicy { Fresh, Continue }`
  - `pub struct InputRef { pub from: String, pub approval_from: String }`
  - `pub struct StepDef { pub id: String, pub role: String, pub profile: String, pub reviews: Option<String>, pub inputs: Vec<InputRef>, pub change_from: Option<String>, pub evidence_from: Option<String>, pub commands: Vec<String>, pub timeout: Option<String>, pub session: Option<SessionPolicy>, pub on_changes_requested: Option<String>, pub on_tests_failed: Option<String> }`
  - `pub fn parse_duration(text: &str) -> Result<Duration, DefinitionError>` (`<n>s|m|h`, n ≥ 1)
  - `#[derive(Debug, thiserror::Error)] pub enum DefinitionError { Yaml(String), Duration(String) }`

- [ ] **Step 1: Write failing tests** in `definition.rs` `mod tests`:
  - `the_shipped_shape_parses`: the spec's "The workflow file" YAML parses; `steps.len() == 7`; `limits.max_rounds == 5`; `steps[2].inputs[0] == InputRef { from: "research", approval_from: "review_research" }`.
  - `limits_default_to_five_and_three`: YAML with no `limits` → `Limits { max_rounds: 5, transient_retries: 3 }`.
  - `unknown_keys_are_errors`: `approval_form:` inside an input → `Err(DefinitionError::Yaml(msg))` with `msg` containing `approval_form`.
  - `durations`: `"20m"` → 1200 s, `"30s"` → 30 s, `"2h"` → 7200 s; `"0m"`, `"5"`, `"m"`, `"5d"` → `Err(Duration(_))`.
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet-fleet` → fails to compile (crate missing).
- [ ] **Step 3: Implement.** `#[serde(deny_unknown_fields)]` on every struct; `inputs` and `commands` default to empty. Crate Cargo.toml follows `crates/octet-store/Cargo.toml` (workspace package keys, `[lints] workspace = true`); deps: `octet-core`, `octet-proc`, `octet-store`, `tokio`, `serde`, `serde_json`, `serde_norway`, `thiserror`; dev-deps `octet-testkit`, `tokio` with `test-util`. `lib.rs`: `#![forbid(unsafe_code)]`, crate doc, `pub mod definition;`.
- [ ] **Step 4: Run** the tests → PASS; `scripts/rust-env.sh cargo clippy -p octet-fleet --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `Fleet: crate and workflow parsing`.

### Task 2: Validation and the resolved workflow

**Files:**
- Create: `crates/octet-fleet/src/validate.rs`, `docs/fleet/research-plan-build.yaml`

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `pub trait Host { fn engine_known(&self, engine: &str) -> bool; fn check_effort(&self, engine: &str, effort: &str) -> Result<(), String>; fn resolve_binary(&self, engine: &str, given: Option<&Path>) -> Option<PathBuf>; }`
  - `pub struct CoreHost;` implementing `Host` with `octet_core::Engine::parse`, `Engine::check_effort`, `engine.provider().default_binary`, and a `PATH` search for bare names.
  - `pub fn validate(workflow: &Workflow, host: &dyn Host) -> Result<Resolved, Vec<ValidationError>>` (static checks only; the git/sparse and ownership checks are Tasks 8 and 10).
  - `pub struct Resolved { pub name: String, pub limits: Limits, pub steps: Vec<Step> }` (`Serialize + Deserialize + Clone + PartialEq`)
  - `pub struct Step { pub id: String, pub kind: Kind, pub instructions: String, pub profile: ResolvedProfile, pub session: SessionPolicy, pub refs: Refs }`
  - `pub enum Refs { Document { inputs: Vec<Input> }, ReviewDocument { document: usize }, Change { inputs: Vec<Input> }, Tests { change: usize, commands: Vec<String>, timeout_secs: u64 }, ReviewChange { change: usize, evidence: usize } }` (indices into `steps`)
  - `pub struct Input { pub document: usize, pub approval: usize }`
  - `pub struct ResolvedProfile { pub engine: String, pub binary: PathBuf, pub model: Option<String>, pub effort: Option<String>, pub mode: String, pub approval_timeout_secs: u64 }`
  - `impl Step { pub fn read_only(&self) -> bool }` (document and verdict kinds)
  - `impl Resolved { pub fn producer_of(&self, rejector: usize) -> usize; pub fn approvals_required(&self, step: usize) -> Vec<usize> }` (transitive approval steps over `inputs`)
  - `pub enum ValidationError` with one variant per rule below, each `Display`ing the step ID and key.

- [ ] **Step 1: Write failing tests** (a `FakeHost` knowing engines `claude`, `codex`, `opencode`, `antigravity`, accepting every effort but `"bogus"`, resolving every binary to `/bin/<engine>`):
  - `the_shipped_workflow_validates_and_gates_on_both_approvals`: parse `docs/fleet/research-plan-build.yaml` (`include_str!`), validate → `Ok`; `approvals_required(implement)` contains both `review_plan` and `review_research` indices.
  - `a_minimal_custom_workflow_is_valid_with_one_approval`: one document, its review, a coder, tests, final review → `Ok`; `approvals_required(coder) == [review_index]`.
  - `an_input_without_its_approval_is_rejected`: `inputs: [{from: research}]` → parse error (`approval_from` missing) — asserted at `parse`.
  - `an_approval_that_reviews_another_document_is_rejected`: `approval_from: review_plan` for `from: research` → `ValidationError::ApprovalMismatch`.
  - `references_must_be_earlier_and_of_the_right_kind`: a forward `change_from`, a `reviews` naming a tests step → errors.
  - `edges_must_name_the_producer`: `on_changes_requested: research` on `review_change` → `ValidationError::EdgeNotProducer`.
  - `the_completion_gate`: last step a document review → error; final review whose `evidence_from` tests a different change step → error.
  - `read_only_profiles_must_ask`: a document step with `mode: auto` → `ValidationError::ReadOnlyMode`.
  - `tests_need_commands`, `unique_step_ids`, `unknown_engine`, `bad_effort`, `bad_mode` (via `octet_core::Mode::parse`), `version_must_be_one`.
  - `all_errors_are_reported_together`: two broken rules → `Err(v)` with `v.len() == 2`.
- [ ] **Step 2: Run** → FAIL (unresolved `validate`).
- [ ] **Step 3: Implement** `validate` per the spec's "Validation" (static bullets, scope of the research guarantee, edges go to the producer). Defaults: session `Fresh` for document/verdict, `Continue` for change/tests; `mode` `"ask"`; `approval_timeout` from `octet_core::DEFAULT_APPROVAL_TIMEOUT`; tests `timeout` default `20m`. Write `docs/fleet/research-plan-build.yaml` exactly as the spec's example, with full role instructions (each three to six sentences; the reviewer's mentions the JSON block, which `contract` appends).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: validation, resolved workflow, shipped workflow`.

### Task 3: Log records and replay

**Files:**
- Create: `crates/octet-fleet/src/record.rs`, `crates/octet-fleet/src/state.rs`

**Interfaces:**
- Produces the persistence contract (serde `tag = "type"`, `rename_all = "snake_case"`); field names are what `log.jsonl` holds:

```rust
pub struct AttemptId(pub u32);      // serde transparent
pub struct CandidateId(pub u32);
pub struct ArtifactId(pub u32);
pub struct Identity { pub pid: u32, pub start: String }
pub enum Origin { Base, Change, Tests, Evidence, Drift }
pub enum Severity { Blocking, Minor }
pub struct Finding { pub id: String, pub severity: Severity, pub summary: String, pub file: Option<String> }
pub enum Status { Resolved, Open }
pub enum Accepted {
    DocumentProduced { document: ArtifactId },
    DocumentApproved { approval: ArtifactId, new_findings: Vec<Finding>, previous: Vec<(String, Status)> },
    ChangesRequested { new_findings: Vec<Finding>, previous: Vec<(String, Status)> },
    ChangeProduced { change: ArtifactId },
    TestsPassed { evidence: ArtifactId },
    TestsFailed { evidence: ArtifactId, failing: Vec<usize> },
    ChangeApproved { approval: ArtifactId, new_findings: Vec<Finding>, previous: Vec<(String, Status)> },
}
pub enum Failure { Interrupted { signal: String }, Transient { detail: String }, ConfigError { detail: String },
                   LocalFailure { detail: String }, Unknown { detail: String }, Invalid { detail: String },
                   AccessViolation { diff_stat: String } }
pub enum PauseReason { Reconcile, RoundLimit, Stalled, UnstableTests, Drift, AccessViolation,
                       ProcessesRunning, ProcessesUnverified, InvalidOutput, TransientExhausted,
                       LocalFailure, Unknown, Interrupted }
pub enum Decision { Keep, Adopt { candidate: CandidateId }, Continue, Rounds { n: u32 }, Retry,
                    Kill, Wait, Release { pids: Vec<u32> }, Stop }
pub enum Record {
    AttemptStarted { attempt: AttemptId, step: String, round: u32, input_candidate: CandidateId,
                     requires: Vec<ArtifactId>, history: Vec<ArtifactId> },
    AttemptSession { attempt: AttemptId, journal: PathBuf, identities: Vec<Identity> },
    VendorSession { attempt: AttemptId, id: String },
    Candidate { id: CandidateId, tree: String, parent: Option<CandidateId>, origin: Origin },
    EvidenceStarted { attempt: AttemptId, candidate: CandidateId, restart: u32 },
    EvidenceCommand { attempt: AttemptId, restart: u32, index: usize, command: String,
                      identity: Identity, pre_tree: String },
    EvidenceResult { attempt: AttemptId, restart: u32, index: usize, exit: Option<i32>,
                     timed_out: bool, duration_ms: u64, post_tree: String },
    AttemptFinished { attempt: AttemptId, outcome: Accepted, candidate: Option<CandidateId> },
    AttemptFailed { attempt: AttemptId, failure: Failure },
    AttemptInterrupted { attempt: AttemptId, signal: String },
    Retry { attempt: AttemptId, class: String, count: u32, eligible_at: u64 },
    Paused { reason: PauseReason, detail: String },
    Decision { decision: Decision },
    Completed { candidate: CandidateId, tree: String, evidence: ArtifactId, approval: ArtifactId,
                observed_tree: String },
    Stopped,
    Failed { error: String },
}
```

  Artifact metadata travels in `artifacts/<id>.json` (Task 9); the machine needs only the IDs above plus `ArtifactMeta { id, step: usize, requires: Vec<ArtifactId>, candidate: Option<CandidateId> }`, supplied to replay alongside the records.
- Produces `pub fn replay(def: &Resolved, records: &[Record], artifacts: &[ArtifactMeta]) -> Result<RunState, ReplayError>` and `RunState` with at least: `candidates: Vec<CommittedCandidate>`, `tip: CandidateId`, `attempts: BTreeMap<AttemptId, AttemptState>`, `latest: Vec<Option<ArtifactId>>` (per step), `needs_revision: Vec<bool>` (per step), `findings: BTreeMap<String, (Finding, Status)>`, `jumps: BTreeMap<usize, u32>` (per rejector), `extra_rounds: BTreeMap<usize, u32>`, `pending_pause: Option<PauseReason>`, `decisions_since_pause: Vec<Decision>`, `completed: bool`, `stopped: bool`.

- [ ] **Step 1: Write failing tests** in `state.rs`:
  - `records_round_trip_as_snake_case_json`: each variant → `serde_json` → back equal; `AttemptStarted` serializes with `"type":"attempt_started"`.
  - `an_uncommitted_candidate_is_ignored`: `Candidate {id: 2}` with no committing record → `tip` stays the base; after `EvidenceStarted {candidate: 2}` → tip 2; after `AttemptFinished { candidate: Some(3) }` → tip 3; after `Decision { Adopt { candidate: 4 } }` → tip 4.
  - `octet_assigns_finding_ids_across_rounds`: two `ChangesRequested` from `review_change` → findings `review_change/F1`, `/F2`, then `/F3`; `previous` statuses update the ledger.
  - `a_rejection_marks_the_producer`: `ChangesRequested` by `review_plan` → `needs_revision[plan] == true`, `jumps[review_plan] == 1`; a later accepted plan clears it.
  - `an_attempt_without_finished_is_unfinished`: `AttemptStarted` (+ `AttemptInterrupted`) → `AttemptState::Unfinished { released: bool }` where `released` is true once `AttemptSession` exists.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `replay` as a fold over records. Findings get IDs at replay (`<step>/F<n>`, `n` from 1 per step, in record order), so the IDs a prompt shows are the IDs replay rebuilds.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: log records and replay`.

### Task 4: The machine: currency and dispatch

**Files:**
- Create: `crates/octet-fleet/src/machine.rs`

**Interfaces:**
- Consumes: `Resolved`, `RunState`.
- Produces:
  - `pub fn next(def: &Resolved, state: &RunState) -> Next`
  - `pub enum Next { Run(Dispatch), Evidence { attempt: AttemptId, candidate: CandidateId, restart: u32 }, Pause(PauseReason, String), ReadyToComplete { candidate: CandidateId, tree: String, evidence: ArtifactId, approval: ArtifactId }, Done }`
  - `pub struct Dispatch { pub step: usize, pub round: u32, pub input_candidate: CandidateId, pub requires: Vec<ArtifactId>, pub history: Vec<ArtifactId>, pub resume_session: Option<String> }`
  - `pub fn is_current(def: &Resolved, state: &RunState, artifact: ArtifactId) -> bool`
  - Round of a dispatch = 1 + the step's accepted attempts so far.

- [ ] **Step 1: Write failing tests**, one per worked trace in the spec's "Artifacts, candidates and the machine" (build `RunState` with `replay` over hand-written records):
  - `research_rejected_then_revised_goes_to_review_research` (`D1 → rejection → D2` ⇒ `Run(review_research)`, `requires == [D2]`, `history` empty; the open findings come from the ledger). And the revision itself: after `D1 → rejection` ⇒ `Run(research)` with `requires == []`, `history == [D1]`.
  - `nothing_downstream_runs_before_research_is_approved`: after D1 only, `next` is `review_research`; never `plan` or `implement`.
  - `plan_runs_with_the_approved_research`: after `A(D2)` ⇒ `Run(plan)`, `requires == [A(D2)]`.
  - `plan_loop_does_not_rerun_research`: `P1 → rejection → P2` ⇒ `Run(review_plan)`; `A(D2)` still current.
  - `a_tester_edit_does_not_rerun_the_coder`: `C1 = K1`, tester commits K2 via `EvidenceStarted`, `TestsPassed(E(K2))`, `ChangeApproved(R(K2))` ⇒ `ReadyToComplete { candidate: K2, evidence: E(K2) }`; `C1` is current.
  - `failing_tests_send_the_run_to_the_coder_from_the_tip`: `TestsFailed(E(K2))` ⇒ `Run(implement)`, `input_candidate == K2`, `round == 2`, `resume_session` = the coder's logged vendor session.
  - `a_change_rejection_reruns_tests_and_review`: after `R` rejects and `implement` makes K3 ⇒ `Run(test)`; after tests ⇒ `Run(review_change)` (a fresh session: `resume_session == None`).
  - `adopted_drift_reruns_tests_not_the_coder`: `Decision Adopt K'` after approval ⇒ `Run(test)`.
  - `an_evidence_phase_resumes_as_evidence`: `EvidenceStarted` with no `TestsPassed/Failed` and no unfinished-attempt recovery pending ⇒ `Evidence { candidate, restart }`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** per the spec: first step, in order, whose latest artifact is missing, not current, or needs revision. `requires` from `Refs`; currency per the spec's requires table, transitively; "K is the tip" for evidence and change approvals.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: machine currency and dispatch`.

### Task 5: Rounds, stalls, decisions and the invariant

**Files:**
- Modify: `crates/octet-fleet/src/machine.rs`

**Interfaces:**
- Produces: `next` returns `Pause(RoundLimit|Stalled, detail)` and honours `Decision`s; `ReadyToComplete` only when the invariant's logged parts hold.

- [ ] **Step 1: Write failing tests:**
  - `five_jumps_then_a_round_limit`: six `ChangesRequested` from `review_change` ⇒ after the fifth, `Run(implement)`; after the sixth, `Pause(RoundLimit)`. `Decision Continue` ⇒ one more `Run(implement)`; `Decision Rounds { n: 8 }` ⇒ runs until 8.
  - `a_reviewer_that_resolves_nothing_stalls`: round 2 `ChangesRequested` with every previous finding `Open` ⇒ `Pause(Stalled)`; with one `Resolved` ⇒ `Run(implement)`.
  - `no_stall_without_previous_findings`: two approvals with empty `previous` ⇒ no stall.
  - `an_unchanged_tree_after_feedback_stalls`: `ChangeProduced` whose candidate tree equals the input candidate's tree, given feedback ⇒ `Pause(Stalled)`.
  - `the_same_failure_twice_stalls`: two consecutive `TestsFailed` from `test` with equal `failing` on the same tree ⇒ `Pause(Stalled)`.
  - `invalid_never_counts`: `AttemptFailed { Invalid }` twice ⇒ `Pause(InvalidOutput)`; `jumps` unchanged.
  - `ready_requires_every_part`: approval for K2 but evidence for K1 ⇒ not `ReadyToComplete`; evidence failing ⇒ not; plan approval stale ⇒ not.
  - `a_completed_run_is_done`, `a_stopped_run_is_done`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** Jump check before dispatching a producer for a rejection from `r`: pause when `jumps[r] > max_rounds + extra_rounds[r]` (or `> n` after `Rounds`). Stall rules exactly as the spec's "Stalls"; stalls apply only to `ChangesRequested` in the same review step.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: rounds, stalls, decisions, the invariant`.

### Task 6: Recovery classification and the milestone-1 acceptance trace

**Files:**
- Create: `crates/octet-fleet/src/recover.rs`, `crates/octet-fleet/src/sim.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces:
  - `pub struct Observation { pub live_tree: String, pub go_created: bool, pub processes: ProcessCheck }`
  - `pub enum ProcessCheck { Clear, Verified(Vec<u32>), Unverified(Vec<u32>) }`
  - `pub fn recover(def: &Resolved, state: &RunState, obs: &Observation) -> Next` — `Next::Run` (a new attempt, with `resume_session` when the table says so), `Next::Evidence`, or `Next::Pause(…)`; falls through to `machine::next` when no attempt is unfinished.
  - `sim::Script` (map `(step id, round)` → a scripted outcome including "tester edits"), `sim::run(def, script) -> SimLog { records: Vec<Record>, artifacts: Vec<ArtifactMeta>, observations: Vec<Observation>, decisions: Vec<Next> }`. Trees are strings the sim invents (`"t0"`, `"t1"`, …); `observations[i]` is what a crash right after the first `i` records would observe (the sim's tree at that point, `go_created` per the last attempt's release, `ProcessCheck::Clear`); `decisions[i]` is the action the sim took after those records.

- [ ] **Step 1: Write failing tests:**
  - One test per row of the spec's "State found" table, named after the row (`read_only_unfinished_is_a_new_attempt`, `never_released_and_unchanged_is_a_new_attempt`, `never_released_and_changed_is_drift`, `released_change_unchanged_continues_the_session`, `released_change_changed_reconciles`, `tester_before_handoff_unchanged_reruns_the_tester`, `tester_before_handoff_changed_reconciles`, `after_handoff_runs_evidence_from_zero`, `after_handoff_with_another_tree_is_drift`).
  - `processes_come_first`: any row with `ProcessCheck::Verified` ⇒ `Pause(ProcessesRunning)`; `Unverified` ⇒ `Pause(ProcessesUnverified)`.
  - `an_interrupt_needs_no_decision`: an `AttemptInterrupted` + `Paused { Interrupted }` log maps to the same row as the uninterrupted crash, with no `Decision` required.
  - `decisions_have_exact_next_actions`: one assertion per row of the spec's decision table (`Reconcile`+`Keep` ⇒ `Run` same step with `resume_session`; `Reconcile`+`Adopt` ⇒ next action of the adopted candidate; `UnstableTests`+`Retry` ⇒ `Evidence{restart: 0}`; …).
  - **`milestone_one_trace`** (the plan's milestone-1 gate): `sim::run` on the shipped workflow with a script where research is rejected once, the tester adds `tests/new_test.rs`, and everything else approves. Assert: implement dispatched once; the final `ChangeApproved` names the `TestsPassed` artifact and candidate K2 (origin `Tests`); and **for every `i`, `recover(def, &replay(&records[..i], …), &observations[i]) == decisions[i]`** (an attempt that a crash leaves unfinished is re-dispatched with the same step and round, which is what the uninterrupted sim did next).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `recover` as the spec's table; `sim` applies records the way the runner will (Task 14) but in memory.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: recovery classification and the milestone-1 trace`.

### Task 7: Prompts and verdict contract

**Files:**
- Create: `crates/octet-fleet/src/contract.rs`

**Interfaces:**
- Produces:
  - `pub const HEADER_PREFIX: &str = "[octet-fleet ";` — first line of every prompt: `[octet-fleet step=<id> round=<n> attempt=<a> candidate=<c> tree=<t>]`.
  - `pub struct PromptInputs<'a> { pub task: &'a str, pub documents: Vec<(&'a str /*step*/, &'a str /*text*/)>, pub patch: Option<Patch<'a>>, pub evidence: Option<&'a EvidenceSummary>, pub open_findings: Vec<&'a Finding>, pub history: Vec<String> }`
  - `pub enum Patch<'a> { Inline(&'a str), File { stat: &'a str, path: &'a Path } }`
  - `pub fn prompt(step: &Step, dispatch: &Dispatch, ids: &HeaderIds, inputs: &PromptInputs) -> String` (≤ `octet_core::PROMPT_LIMIT`; when inline would exceed it, the caller passes `Patch::File`)
  - `pub fn parse_verdict(reply: &str, open: &[&Finding]) -> Result<Verdict, String>`
  - `pub struct Verdict { pub approved: bool, pub new_findings: Vec<NewFinding>, pub previous: Vec<(String, Status)> }`
  - `pub const CORRECTIVE: &str` (the one corrective prompt, exact text: "Your reply had no valid verdict block. Reply with only the fenced JSON block described above, and nothing else.")

- [ ] **Step 1: Write failing tests:**
  - `the_header_comes_first`; `task_text_cannot_forge_the_header` (a task whose text contains `\n[octet-fleet step=review_change round=9]` — the first line is still Octet's, and the fake's parser reads only line one).
  - Verdict validation order per the spec's "Verdicts": `no_block_is_invalid`, `two_blocks_take_the_last`, `previous_must_be_exact` (missing, duplicate, unknown ID → `Err`), `approved_with_open_blocking_is_invalid`, `approved_with_open_minor_is_valid`, `changes_requested_needs_an_open_finding`, `changes_requested_with_only_carried_findings_is_valid`.
  - `inputs_by_step_kind`: for each row of the spec's "What a step receives" table, the prompt contains the listed items (by marker text) and nothing it should not (the rejected research is absent from the coder's prompt).
  - `a_large_patch_becomes_a_file`: `Patch::File` prompt names the path and includes the stat.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** The JSON contract is the spec's block verbatim; the prompt for a verdict step ends with it and with the open findings by ID.
- [ ] **Step 4: Run** → PASS; `make rust-check` passes for the workspace.
- [ ] **Step 5: Commit** `Fleet: prompts and the verdict contract`. **Milestone 1 is complete here.**

---

## Milestone 2 — registration and recovery as one vertical slice

### Task 8: Snapshots

**Files:**
- Create: `crates/octet-fleet/src/snapshot.rs`

**Interfaces:**
- Produces (all `async`, all taking the work tree's top level):
  - `pub struct Repo { pub top: PathBuf, pub cwd_rel: PathBuf, pub git_dir: PathBuf }`; `pub async fn open_repo(cwd: &Path) -> Result<Repo, SnapshotError>` (refuses sparse checkouts: `core.sparseCheckout` true or any `git ls-files -t` entry tagged `S`)
  - `pub async fn take(repo: &Repo, scratch_index: &Path) -> Result<String /*tree*/, SnapshotError>` (the spec's four steps)
  - `pub async fn keep(repo: &Repo, run: &str, seq: u32, tree: &str) -> Result<String /*commit*/, SnapshotError>` (`commit-tree` + `update-ref refs/octet-fleet/<run>/<seq>`)
  - `pub async fn diff(repo: &Repo, from: &str, to: &str, stat: bool) -> Result<String, SnapshotError>` (`git diff --binary` or `--stat`)
  - `pub async fn scratch_clone(repo: &Repo, commit: &str, into: &Path) -> Result<PathBuf /*cwd in clone*/, SnapshotError>` (`clone --shared --no-checkout`, `checkout --detach`)
  - `pub async fn tree_of(dir: &Path, scratch_index: &Path) -> Result<String, SnapshotError>` (snapshot of a clone, for the access check)

- [ ] **Step 1: Write failing tests** against `git init` repos in `octet_testkit::TempDir` (set `user.name`/`user.email` locally): `tracked_ignored_files_are_kept`, `staged_additions_are_kept`, `assume_unchanged_edit_is_captured` (`v1 → v2` captured as `v2`), `a_sparse_checkout_is_refused`, `untracked_and_deleted`, `the_users_index_and_head_are_untouched` (byte-compare `.git/index` and `HEAD` before/after), `cwd_below_the_top_level`, `refs_keep_trees`, `a_scratch_clone_checks_out_the_candidate`, `paths_with_spaces_and_unicode` (Review Focus 3).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** with `tokio::process::Command`; every git call gets the three `-c` flags; `GIT_INDEX_FILE` for the copy; clear assume-unchanged on the paths `git ls-files -v` prints with a lowercase tag.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: git snapshots and scratch clones`.

### Task 9: The run store

**Files:**
- Create: `crates/octet-fleet/src/store.rs`

**Interfaces:**
- Produces:
  - `pub fn new_run_id() -> String` — `f<unix seconds>-<4 lowercase hex>` (sortable, typeable).
  - `pub struct RunDir { pub root: PathBuf }`; `pub async fn create(data_dir: &Path, id: &str, run: &RunFile) -> Result<RunDir, StoreError>`; `pub async fn open(data_dir: &Path, id: &str) -> Result<(RunDir, RunFile), StoreError>`
  - `pub struct RunFile { pub workflow: Resolved, pub hash: String, pub task: String, pub top: PathBuf, pub cwd_rel: PathBuf, pub base: CandidateId, pub base_tree: String, pub octet_version: String }` (`run.json`, written once)
  - `pub struct Log` with `pub async fn open(dir: &RunDir) -> Result<(Log, Vec<Record>), StoreError>` (repairs a torn tail by truncating to the last newline, then fsyncs) and `pub async fn append(&mut self, record: &Record) -> Result<(), StoreError>` (one JSON line, `sync_data` before returning)
  - `pub async fn write_artifact(dir: &RunDir, meta: &ArtifactMeta, body: &[u8], ext: &str) -> Result<(), StoreError>` (body and `<id>.json`, both synced, before any `AttemptFinished` names them)
  - `pub fn attempt_dir(dir: &RunDir, attempt: AttemptId) -> PathBuf` (`attempts/<n>`)
  - `pub async fn workflow_hash(repo_top: &Path, resolved_json: &[u8]) -> Result<String, StoreError>` (`git hash-object --stdin`)
- [ ] **Step 1: Write failing tests:** `a_torn_tail_is_cut_back` (write two records and half a third; `open` returns two and the file ends with `\n`), `append_then_reopen`, `run_json_is_private` (mode `0o600`, dir `0o700`), `artifacts_are_durable_before_named` (write then read back meta), `run_ids_sort_by_time`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** with `octet_store::create_private_dir`, `create_private`, `sync_directory`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: run store and durable log`.

### Task 10: Ownership: locks, the worktree record, process verification

**Files:**
- Create: `crates/octet-fleet/src/ownership.rs`, `crates/octet-proc/src/signal.rs`
- Modify: `crates/octet-proc/src/lib.rs` (`pub mod signal;`)

**Interfaces:**
- Produces in `octet-proc`: `pub fn alive(pid: u32) -> bool` (`kill(pid, 0)`), `pub fn terminate(pid: u32) -> bool` (SIGTERM), `pub fn kill(pid: u32) -> bool` (SIGKILL), with `// SAFETY:` comments in the style of `group.rs`.
- Produces in `ownership`:
  - `pub struct Held { /* the two File locks */ }`; `pub fn lock(data_dir: &Path, run: &RunDir, worktree_hash: &str) -> Result<Held, OwnershipError>` (`File::try_lock`; `OwnershipError::RunBusy` / `WorktreeBusy { holder }`)
  - `pub struct WorktreeRecord { pub runs: BTreeMap<String, Unresolved> }`, `pub struct Unresolved { pub attempt: AttemptId, pub identities: Vec<Identity> }`; `pub fn load(data_dir, hash) -> WorktreeRecord`, `pub fn save(data_dir, hash, &WorktreeRecord)` (only while `Held`)
  - `pub struct ProcessInfo { pub pid: u32, pub pgid: u32, pub start: String, pub marker: Option<String>, pub command: String }`; `pub async fn list_processes() -> Result<Vec<ProcessInfo>, OwnershipError>` (macOS: `ps -A -E -ww -o pid=,pgid=,lstart=,command=` and parse `OCTET_FLEET_ATTEMPT=`; Linux: `/proc/<pid>/{stat,environ,cmdline}`)
  - `pub fn check(processes: &[ProcessInfo], attempt_marker: &str, registered: &[Identity], groups: &[u32], released: &[u32]) -> ProcessCheck` (the spec's "Leftover processes")
  - `pub async fn kill_verified(pids: &[u32])` (SIGTERM, wait 5 s, SIGKILL, re-list)
  - `pub fn admit(record: &WorktreeRecord, driving: &str) -> Result<(), OwnershipError>` (`OtherRunUnresolved { run }`)
- [ ] **Step 1: Write failing tests:** `a_second_driver_is_refused` (Review Focus 5: second `lock` on the same run → `RunBusy`, log file untouched), `a_second_run_on_the_worktree_is_refused`, `admit_refuses_while_another_run_is_listed`, `a_marked_child_is_verified` (spawn `sh -c 'sleep 30'` with the marker env; `list_processes` + `check` → `Verified([pid])`), `a_matching_identity_is_verified`, `an_unmarked_group_member_is_unverified` (spawn in a registered group with `env -u OCTET_FLEET_ATTEMPT`), `released_pids_are_ignored`, `kill_verified_kills_only_those` (two sleeps, one marked: only it dies).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** The worktree hash is `git hash-object --stdin` of the canonical top-level path.
- [ ] **Step 4: Run** → PASS on macOS; run the Linux recipe for `octet-fleet` and `octet-proc` tests.
- [ ] **Step 5: Commit** `Fleet: locks, worktree record, process verification`.

### Task 11: The gate and the evidence runner

**Files:**
- Create: `crates/octet-fleet/src/gate.rs`, `crates/octet-fleet/src/evidence.rs`

**Interfaces:**
- Produces in `gate`:
  - `pub async fn write_gate(attempt_dir: &Path, real_binary: &Path, marker: &str, octet_pid: u32) -> Result<PathBuf, GateError>` — owner-only `sh` script: export the marker, `echo $$ >> processes`, poll for `go` every 50 ms while `kill -0 <octet_pid>`, give up after 120 s with `exit 125`, then `exec '<real_binary>' "$@"` (path single-quoted with `'\''` escaping).
  - `pub async fn read_processes(attempt_dir: &Path) -> Vec<u32>`; `pub async fn release(attempt_dir: &Path) -> Result<(), GateError>` (create `go`, fsync the dir).
- Produces in `evidence`:
  - `pub struct EvidenceRun<'a> { pub repo: &'a Repo, pub log: &'a mut Log, pub attempt: AttemptId, pub candidate: CandidateId, pub tree: String, pub restart: u32, pub commands: &'a [String], pub timeout: Duration, pub marker: String, pub pause_hook: Option<PathBuf> }`
  - `pub async fn run(run: EvidenceRun<'_>) -> Result<EvidenceOutcome, EvidenceError>`; `pub enum EvidenceOutcome { Complete { passed: bool, results: Vec<CommandResult> }, Changed { tree: String, restart: u32 }, Unstable }`
  - `pause_hook`: when set (tests only, from env `OCTET_FLEET_TEST_PAUSE_DIR`), after each held spawn the runner creates `<dir>/held-<restart>-<index>` and waits for `<dir>/continue-<restart>-<index>` before observing `pre_tree`.
- [ ] **Step 1: Write failing tests:**
  - Gate (with a real repo dir and `/bin/sh` scripts): `the_gate_waits_for_go` (a target that writes `ran`; no `ran` until `release`), `the_gate_gives_up_when_octet_is_gone` (pass a dead PID: exits 125 within 1 s, no `ran`), `later_launches_pass_at_once_and_register`.
  - Evidence: `all_pass_on_an_unchanged_tree` (each result has `pre_tree == post_tree == T`), `a_failure_is_tests_failed`, `a_change_after_a_restarts` (A edits a tracked file → `Changed`), `change_use_restore_is_caught` (A edits, B reads, C restores → `Changed` at A's boundary), `an_edit_before_release_is_caught_before_b_runs` (with `pause_hook`: edit while B is held; B's own marker file has no line for restart 0), `three_changes_are_unstable`, `a_hung_command_times_out_with_its_group` (Review Focus 4: `sh -c 'sleep 999 & sleep 999'`, `timeout` 1 s → `timed_out`, both sleeps gone), `nothing_starts_before_its_record` (kill the runner task between spawn and record: the command's marker file is absent).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** the spec's "Test evidence" sequence; held command = `sh -c 'read _ || exit 125; exec sh -c "$1"' sh CMD` via `octet_proc::GroupLeader` with `process_group(0)`; output captured to a file, last 16 KiB kept.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Fleet: vendor gate and evidence runner`.

### Task 12: Scripted fake vendors

**Files:**
- Create: `crates/octet-testkit/src/bin/protocol-child/fleet.rs`
- Modify: `crates/octet-testkit/src/scenario.rs` (add `pub const FLEET_SCRIPT_ENV: &str = "OCTET_FLEET_SCRIPT";`), `protocol-child/main.rs` (`mod fleet;`, startup marker), `codex.rs` and `claude.rs` (call `fleet::turn` first in their turn dispatch)

**Interfaces:**
- Script file (JSON): `{"log": "<path>", "startup_marker": "<path>"?, "turns": {"<step>#<round>": {"write": {"<rel path>": "<content>"}, "delete": ["<rel>"], "reply": "<text>", "hold": "before_write"|"after_write"|null, "spawn_survivor": bool, "exit_after": bool}}}`.
- Behaviour: on start, if `startup_marker` is set, append `pid` to it before reading stdin. For each turn: parse line one of the prompt (`contract::HEADER_PREFIX` format, re-declared in the fake since testkit cannot depend on fleet); append `{"key","cwd","resumed","prompt"}` to `log`; apply `write`/`delete` relative to cwd; hold if asked (until interrupted); `spawn_survivor` starts `sh -c 'sleep 60'` that inherits the environment; reply `reply` as the dialect's text and complete the turn; `exit_after` exits right after. `resumed` = Codex `thread/resume`, Claude `--resume` in argv.
- [ ] **Step 1: Write failing tests** in `crates/octet-core/tests/fleet_fakes.rs` (octet-core already dev-depends on octet-testkit): drive one Codex-dialect and one Claude-dialect session (`octet_core::Session::open`) with a script; assert the file was written, the reply text arrived, `log` has the key and `resumed: false`, then a resumed session logs `resumed: true`; `startup_marker` written before `Ready`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**; untouched when `OCTET_FLEET_SCRIPT` is unset, so existing scenarios are unchanged.
- [ ] **Step 4: Run** `scripts/rust-env.sh cargo test -p octet-testkit -p octet-core` → PASS (existing suites too).
- [ ] **Step 5: Commit** `Testkit: scripted fake vendors for fleet runs`.

### Task 13: One attempt against a session

**Files:**
- Create: `crates/octet-fleet/src/step.rs`, `crates/octet-fleet/src/classify.rs`, `crates/octet-core/src/background.rs`
- Modify: `crates/octet-core/src/lib.rs` (`pub mod background;`), `crates/octet/src/headless.rs` (use `octet_core::background::Background`; delete its local copy)

**Interfaces:**
- Produces in `octet-core`: `pub struct Background` with `pub fn track(&mut self, event: &Event, idle: bool)`, `pub fn pending(&self) -> bool`, `pub async fn grace_over(&mut self)`, and `pub const BACKGROUND_GRACE: Duration = 5 s` (moved as is).
- Produces in `classify`: `pub fn classify(engine: &str, error_text: &str, stopped_mid_turn: bool) -> FailureClass` with `pub enum FailureClass { Transient { reset_at: Option<u64> }, ConfigError, LocalFailure, Unknown }`; `Event::Error` from the core pump is `LocalFailure`.
- Produces in `step`:
  - `pub struct AttemptPlan<'a> { pub step: &'a Step, pub dispatch: &'a Dispatch, pub attempt: AttemptId, pub prompt: String, pub cwd: PathBuf, pub attempt_dir: PathBuf, pub marker: String, pub data_dir: PathBuf }`
  - `pub trait Approver: Send { fn ask<'a>(&'a mut self, summary: &'a octet_core::ApprovalSummary) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>; }` (object-safe; the front end implements it; read-only steps never call it)
  - `pub async fn run_attempt(plan: AttemptPlan<'_>, log: &mut Log, approver: &mut dyn Approver, events: &mpsc::Sender<FleetEvent>) -> Result<TurnEnd, StepError>`
  - `pub enum TurnEnd { Replied { text: String }, Failed(FailureClass, String), Interrupted }`
  - Sequence: write gate → `Session::open(config with binary = gate, …, attempt_dir)` → wait for a PID in `processes` → `AttemptSession` durable → `gate::release` → wait `Ready` → send prompt → forward events as `FleetEvent::Session` → deny every approval if read-only, else ask the approver → on `Finished`, wait for `Background` (+ grace) → `VendorSession` logged on each non-empty `Ready` change → shut down.
- [ ] **Step 1: Write failing tests** (with Task 12's fake): `a_reply_is_collected`, `the_vendor_does_not_run_before_registration` (startup marker absent until `AttemptSession` is in the log), `read_only_approvals_are_denied` (fake `approval` script → answer `allow: false`, approver never called), `write_approvals_go_to_the_approver`, `a_vendor_session_is_logged`, `a_failed_turn_is_classified` (fake `fail` → `Transient` for codex usage-limit text), and `classify` unit tests over the real error texts collected in Task 18 (start with the fixtures' texts).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** `Config::new(engine, gate, cwd)` with `model`, `effort`, `mode`, `approval_timeout`, `resume = dispatch.resume_session`, `data_dir`.
- [ ] **Step 4: Run** `scripts/rust-env.sh cargo test -p octet-fleet -p octet-core -p octet` → PASS (headless suite unchanged).
- [ ] **Step 5: Commit** `Fleet: one attempt against a session; Background moved to core`.

### Task 14: The runner

**Files:**
- Create: `crates/octet-fleet/src/runner.rs`
- Modify: `crates/octet-fleet/src/lib.rs` (`FleetEvent`, re-exports)

**Interfaces:**
- Produces:
  - `pub enum FleetEvent { RunStarted { run: String }, AttemptStarted { attempt: AttemptId, step: String, round: u32, engine: String, model: Option<String> }, Session { attempt: AttemptId, event: octet_core::Event }, Candidate { id: CandidateId, origin: Origin }, EvidenceCommand { index: usize, command: String }, AttemptFinished { attempt: AttemptId, outcome: String }, Retrying { attempt: AttemptId, at: u64 }, Paused { reason: PauseReason, detail: String }, NeedsDecision { reason: PauseReason, options: Vec<&'static str> }, Completed { tree: String, diff_command: String } }`
  - `pub struct Start { pub workflow: Resolved, pub hash: String, pub task: String, pub cwd: PathBuf, pub data_dir: PathBuf }`
  - `pub async fn start(start: Start, io: RunnerIo) -> Result<RunEnd, RunnerError>`; `pub async fn resume(data_dir: &Path, run: &str, decision: Option<Decision>, io: RunnerIo) -> Result<RunEnd, RunnerError>`
  - `pub struct RunnerIo { pub events: mpsc::Sender<FleetEvent>, pub approver: Box<dyn Approver>, pub signals: watch::Receiver<Option<&'static str>> }`
  - `pub enum RunEnd { Completed, Paused(PauseReason), Failed(String), Interrupted(&'static str) }`
  - `pub async fn stop(data_dir: &Path, run: &str) -> Result<RunEnd, RunnerError>` (= `resume` with `Decision::Stop`; refused per the spec's `stop` rule)
  - `pub async fn status(data_dir: &Path, run: Option<&str>) -> Result<Vec<RunSummary>, RunnerError>` with `pub struct RunSummary { pub id: String, pub workflow: String, pub state: String, pub step: Option<String>, pub round: u32, pub open_findings: usize, pub refs: Vec<String> }` (read-only: takes no lock, replays the log)
- Loop: lock → `admit` → replay → observe (snapshot, `list_processes`, `go` file) → `recover` → act → append → repeat. Acting on `Run`: read-only steps in a fresh scratch clone, access check by `tree_of` before/after (`AttemptFailed { AccessViolation }` → pause); document/verdict/change acceptance writes artifacts then `AttemptFinished`; `Invalid` → one `contract::CORRECTIVE` turn then `AttemptFailed { Invalid }`; tests attempts: tester turn → handoff (`Candidate` if changed, `EvidenceStarted`) → `evidence::run`. Drift check before each attempt and before acceptance. `ReadyToComplete` → observe → `Completed { observed_tree }` or `Paused { Drift }`. Transient: `Retry` with `eligible_at`, sleeping until it (also across restarts), up to `transient_retries`, only when the tree is unchanged; otherwise `Reconcile`. Signals: interrupt the turn or command, `AttemptInterrupted`, `Paused { Interrupted }`. The worktree record entry is added before the first write attempt and updated at each attempt start/end.
- [ ] **Step 1: Write failing tests** (in-process, fake vendors, real git): `a_full_run_completes` (shipped workflow with fakes; `completed.observed_tree == tree`), `a_reviewer_write_is_an_access_violation`, `an_edit_after_approval_is_drift`, `a_transient_retry_waits_out_its_backoff_across_a_restart` (use `tokio::time::pause` and an injected clock), `decisions_apply` (Reconcile Keep/Adopt, Drift Adopt, RoundLimit Continue).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** Keep `runner.rs` to orchestration; push any logic that needs no I/O into `machine`/`recover`.
- [ ] **Step 4: Run** → PASS; `make rust-check`.
- [ ] **Step 5: Commit** `Fleet: runner`.

### Task 15: The `octet fleet` front end

**Files:**
- Create: `crates/octet/src/fleet.rs`
- Modify: `crates/octet/src/args.rs` (`Parsed::Fleet(FleetAction)`), `crates/octet/src/main.rs` (dispatch, help line `octet fleet run|resume|stop|status|validate (octet fleet --help)`), `crates/octet/Cargo.toml` (`octet-fleet`)

**Interfaces:**
- `pub(crate) enum FleetAction { Run { workflow: String, task: TaskSource, cwd: Option<PathBuf>, trust: bool, json: bool }, Resume { run: String, decision: Option<String>, rounds: Option<u32>, json: bool }, Stop { run: String }, Status { run: Option<String> }, Validate { workflow: String, cwd: Option<PathBuf> }, Help }` with `journal_dir` carried as for `hub`.
- `pub(crate) async fn fleet(action: FleetAction, data_dir: PathBuf) -> Result<i32, CliError>`
- Trust: show each profile's absolute binary and mode and every command; y/n on a terminal, else require `--trust`; store `<data_dir>/fleet/trusted/<hash>`.
- Text output per the spec's "Front end"; `--output json` writes `FleetEvent`s as JSON lines (child events through `octet_core::event_json`). Terminal approvals: y/n on stderr for write steps; denied with a warning when stdin is not a terminal.
- [ ] **Step 1: Write failing tests** in `args.rs` (parse table in the existing style, including errors: missing task, `--decision bogus`, `--rounds 0`) and a process test in `crates/octet/tests/cli.rs`: `fleet_validate_reports_every_error` (exit 2), `fleet_run_without_trust_off_a_terminal_is_refused` (exit 2, nothing started), `vendor_text_is_plain` (Review Focus 2: a fake reply with `\x1b]0;x\x07` prints without it).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**, reusing `headless::plain` for control-character stripping and the signal setup pattern from `headless::open_headless`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Octet: fleet subcommand`.

### Task 16: Release tests A: the full run and the evidence guarantee

**Files:**
- Create: `crates/octet/tests/fleet.rs` (helpers: a git repo in a TempDir, a workflow file with fake profiles — Claude-dialect researcher/reviewers, Codex-dialect coder/tester, `binary` = `octet_testkit::protocol_child()` — a script file, `octet fleet` as a child process, a log reader that waits for a record type)

- [ ] **Step 1: Write the release tests** from the spec's "Testing" list, numbered as there: **1** (the full run, SIGKILL once `attempt_started` for `test` is durable, then `resume`; assert coder ran once before the test failure, the run cannot complete while tests fail, the final review was fresh and its prompt names the passing evidence's candidate and its patch has the tester's file), **2** (wrong-revision approval), **3** (change, use, restore), **8** (the completion boundary), **10** (an edit between commands, using `OCTET_FLEET_TEST_PAUSE_DIR`).
- [ ] **Step 2: Run** `scripts/rust-env.sh cargo test -p octet --test fleet` → PASS (fix forward in the owning module if not, with a unit test there first).
- [ ] **Step 3: Commit** `Fleet: release tests for the full run and evidence`.

### Task 17: Release tests B: crashes, ownership, interrupts

**Files:**
- Modify: `crates/octet/tests/fleet.rs`

- [ ] **Step 1: Write the release tests** numbered **4, 5, 6, 7, 9, 11, 12, 13** exactly as the spec words them (5 with `startup_marker`, both kill points; 9 with four kills: before/after `evidence_started` × edited/unchanged; 11 with `spawn_survivor` and `exit_after`, plus a survivor started with the marker removed; 12 both variants; 13 Ctrl+C via SIGINT during research, after a coder edit, during evidence), and the "Plus" list: stall pause (exit 3), round-limit pause, access violation, leftover test process on resume, `UnstableTests → retry`.
- [ ] **Step 2: Run** → PASS; then `make rust-check` and the Linux recipe for the whole workspace.
- [ ] **Step 3: Commit** `Fleet: release tests for crashes, ownership and interrupts`. **Milestone 2 is complete here.**

---

## Milestone 3 — the four providers

### Task 18: Real providers, error classes, documentation

**Files:**
- Modify: `crates/octet-fleet/src/classify.rs` (patterns), `crates/octet-fleet/src/step.rs` (any provider fix found)
- Create: `docs/fleet/README.md`

- [ ] **Step 1: Collect real error texts** for each provider (usage limit, model not found, not logged in, network) from existing fixtures and journals the owner has (the music-decoder runs hold real Codex usage-limit errors); add each as a `classify` unit test with its expected class.
- [ ] **Step 2: Run** the `classify` tests → FAIL, implement patterns, → PASS.
- [ ] **Step 3: Real run.** In a throwaway git repo (tiny task: add a function and its test), run `octet fleet run` with a workflow in which **Claude, Codex, opencode and Antigravity each drive at least one step** (Codex via a wrapper pinning a cheap model, as in the sanity recipe). Record per provider: launched through the gate, reply parsed, approvals behaved, resumed session where `continue`. Fix forward with a test for anything found.
- [ ] **Step 4: Write `docs/fleet/README.md`**: how to write a workflow, the trust prompt, every pause and decision, exit codes, and the guarantees in the spec's exact words (the invariant, the evidence guarantee and its blind spot, isolation not sandbox, submodules and sparse checkouts).
- [ ] **Step 5: Run** `make rust-check` and the Linux recipe → PASS.
- [ ] **Step 6: Commit** `Fleet: provider error classes, real-run fixes, documentation`. **Milestone 3 is complete; the branch is ready for review.**
