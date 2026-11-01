# Role-based model workflows in Octet: research and design

**Date:** 2026-11-01 (revision 2, after design review).
**Question:** how can Octet let a user choose one model/provider for coding, another
for testing and another for reviewing, and chain them into workflows?

Revision 2 makes no stronger claim than its mechanisms can support. Revision 1 described
`sees` as "nothing else reaches the model", `policy.writes` as enforced by the
approval path, and a cross-model audit as "independent verification". None of those
claims holds as stated. This revision separates what Octet *supplies* from what an agent
can *reach*. It grades enforcement by what each adapter can actually guarantee, binds all
evidence to an exact captured candidate, and defines recovery before any workflow
syntax. [What changed](#what-changed-from-revision-1) lists every change.

## Summary

- **Positioning.** Cross-vendor CLI delegation is not new: PAL MCP's
  [`clink`](https://github.com/BeehiveInnovations/pal-mcp-server/blob/main/docs/tools/clink.md)
  already runs Claude, Codex and Gemini CLIs with role prompts and separate contexts.
  Nor are context control and deterministic checks unique to CLI-based tools. Octet's
  case is the **combination**: user-configured CLI roles on the user's own logins;
  policies graded by what each adapter can actually enforce; evidence bound to an exact
  workspace snapshot; durable recovery; and one coherent terminal (and phone) workflow.
- **First release: `@review` plus an Octet-run check, both bound to the same captured
  candidate.** It is useful on its own, and it builds the snapshot, evidence and
  execution model that larger workflows need. Everything else waits on Phase 0 (adapter
  capabilities and workspace ownership).
- **Claims.** The result is described as **separate-model review plus independently
  executed checks**. It is not verification of correctness, and it is evaluated against
  single-agent and same-model-review baselines before it is promoted.

## What Octet has today

| Building block | Where | Relevance |
| --- | --- | --- |
| Provider table (`Engine::ALL`: codex, claude, opencode, demo), `Protocol` trait | `octet-engine/src/live/` | A role binding is `(engine, model, effort, mode)` over existing engines |
| Per-vendor permission mapping | `live/claude.rs`, `live/codex.rs`, `live/opencode.rs` | The basis of the capability matrix below |
| Cross-provider handoff (guarded transcript block, 64 KiB) | `octet-tui/src/handoff.rs` | The framing pattern for context inputs |
| `/goal` runner (bounded continuation, completion marker, audit turn) | `octet-core/src/goal.rs` | Migrates onto the new primitive later; it is not the primitive |
| Approvals fail closed (expired, cancelled, oversized, over 8) | `live/driver.rs` | One layer of mediation, not complete mediation |
| Process-group kills | `octet-proc` | Needed so an interrupted attempt is really stopped before reconciliation |
| Append-only JSONL journals | `octet-store` | The evidence and attempt log. Durable step state still has to be designed |
| Hub: concurrent tmux sessions, git status, phone push | `octet-hub` | Later phases: parallel worktrees, phone gates |
| Fake vendors | `octet-testkit` | The deterministic adversarial scenarios listed at the end |

## Survey (brief)

| Tool | Mechanism | Note |
| --- | --- | --- |
| Aider architect mode | Planner model, plus an editor model that writes edits | Two fixed roles |
| Roo / Kilo | A remembered model per mode; an Orchestrator delegates to modes | One agent loop; API keys |
| opencode | Agents with `model: provider/model#variant`, primary agents and subagents | Octet already passes it agents via `OPENCODE_CONFIG_CONTENT` |
| Claude Code subagents | Frontmatter `model:`, scoped hooks, `SubagentStop` | Claude only (secondary sources) |
| Codex agents | Agent TOML with `model` and reasoning effort (third-party guide) | OpenAI only |
| PAL MCP | `consensus`, multi-pass `codereview`, and `clink` (external CLIs as role-prompted subagents) | **Closest prior art for cross-vendor CLI delegation** |
| Archon | YAML DAG workflows, AI steps plus shell steps, a worktree per run | Claude Code only |
| RouteLLM, Not Diamond Code | Learned per-request routing | Coding-specific claims are vendor-reported |
| ACP | Client↔agent protocol; agents *may* request permission | Implementing its permission endpoint is not complete mediation |

### What the evidence supports

- **Self-preference bias in LLM judges is well documented.** That supports giving review
  to a separate model, and a structured review question decorrelates errors further.
- **The cross-vendor study** ([arXiv 2607.21656](https://arxiv.org/html/2607.21656v1))
  reports 71.6% → 89.7% when Claude reviews Codex drafts, and 91.4% → 82.8% in the other
  direction. According to the design review, the setting is algorithmic tasks, with a
  reviewer that *produces a revised final program* and cannot run tests, and the authors
  limit generalisation to repository-scale work. (This method detail was not checked
  independently: arxiv.org is not reachable from the session that wrote this.) **It does
  not validate a read-only reviewer on a repository.**
  It is a reason to make the reviewer binding configurable and to measure it, nothing
  more.
- **Isolating the test writer** (spec only) is a practitioner consensus, not a
  controlled result.

## Design

### 1. Adapter capability matrix (Phase 0)

A role policy can be promised only to the extent an adapter can mediate or contain the
action. The matrix below is what the current code establishes. Rows marked *verify* need
gate scenarios against each pinned CLI version (Claude Code 2.1.289, Codex 0.154,
opencode as pinned), recorded by `octet-gate`.

**Claude Code** (approvals over `--permission-prompt-tool stdio`; `--setting-sources=`
and `--strict-mcp-config`):

| Action | `default` (ask) | `acceptEdits` | `auto` / `bypassPermissions` |
| --- | --- | --- | --- |
| File edit tools | Reach Octet | **Do not reach Octet** | Do not reach Octet |
| Shell | Reach Octet, except what Claude itself treats as pre-approved (*verify*) | Same as ask (*verify*) | Do not reach Octet |
| Reads (Read/Grep/Glob) | **Never reach Octet** | Same | Same |
| MCP tools | No servers unless an enabled plugin brings one (*verify*) | Same | Same |
| Subagents (Task) | Whether their tool calls use the same permission tool: *verify* | Same | Same |
| Plugin hooks | **Can answer an approval** (Octet shows it); `--no-plugin-hooks` turns this off | Same | Same |
| OS sandbox | Octet configures none today. Claude's own sandbox settings could be injected with `--settings`: *verify* | Same | Same |

**Codex** (`codex app-server`; `codex_thread_params`):

| Action | `untrusted` (ask) | `on-request` | `auto_review` | `danger-full-access` |
| --- | --- | --- | --- | --- |
| OS sandbox | `workspace-write` | `workspace-write` | `workspace-write` | None |
| Writes outside the workspace | Contained by the OS; escalation asks | Same | Codex's reviewer decides | Uncontained |
| Writes inside the workspace | Commands: untrusted ones ask. Patch edits: *verify* | Not mediated | Not mediated | Not mediated |
| `readOnly` sandbox | **Reported by Codex and parsed by Octet (`codex_reported`), but never requested.** A candidate for OS-enforced repository read-only (*verify*) | | | |

**opencode** (ACP; Octet's agents `octet-ask`, `octet-accept-edits`, `octet-full-access`):

| Action | Behaviour |
| --- | --- |
| Approvals | Per opencode's permission rules in Octet's agents; `auto` uses opencode's own rules |
| Subagents | **Their approval requests are never passed on to Octet.** Octet's ask and accept-edits agents disable subagents for this reason |
| OS sandbox | None |

**Enforcement levels.** Each role declares the level it needs. Octet **refuses** a role
configuration the adapter and mode cannot provide, and never quietly downgrades it.

| Level | Meaning | Example |
| --- | --- | --- |
| `advisory` | The prompt asks for the behaviour; nothing enforces it | Any adapter |
| `mediated` | Every action of the listed kinds is known to pass through Octet's approval path for this adapter, mode and CLI version, with hooks off; Octet denies by policy | Claude `default` with `--no-plugin-hooks`, for edits and shell, once verified |
| `contained` | An OS boundary or a disposable copy limits the effect regardless of approvals | Codex `readOnly` sandbox (once verified); any adapter run in a disposable candidate copy (§3) |

**Two policies that are not the same:**

- **Repository read-only:** the real candidate cannot change. This is what a reviewer
  needs.
- **No filesystem writes:** stricter, and usually impossible, because every vendor CLI
  writes its own session storage and caches (`~/.claude`, `~/.codex`, opencode's data
  directory).

`policy.repo = "read-only"` means the former.

### 2. Two separate controls: context inputs and accessible resources

| Control | What it governs | Mechanism |
| --- | --- | --- |
| **Context inputs** (`inputs`) | What Octet explicitly puts in the prompt | A context pack built by Octet, bounded and shown in full with `Ctrl+O` |
| **Accessible resources** (`access`) | What the agent can read or fetch on its own: the working tree, `.git` history, instruction files the CLI loads itself (`CLAUDE.md`, `AGENTS.md`), earlier run artifacts, MCP tools, network | Which directory it runs in, what is in that directory, the tool and plugin configuration, and the sandbox |

Rules:

- `inputs` never claims isolation. A role that needs isolation states it in `access`,
  and Octet refuses the role if the adapter cannot provide it.
- **Each step gets a fresh vendor session by default.** A warm session is allowed only
  for an adapter and mode where conversation isolation has been verified (Phase 0). For
  now, that means none.
- Access levels, from weakest to strongest:
  1. `workspace`: the live working tree.
  2. `candidate-copy`: a disposable checkout of the captured candidate (§3), with
     `.git` history present.
  3. `candidate-export`: the candidate tree only, with no `.git` directory, no earlier
     run artifacts, and instruction files listed explicitly or stripped.
  4. Level 3 plus tools restricted: plugins and MCP off, and network off where the
     sandbox can do it.
- **The test writer's inputs:** the task, acceptance criteria, public interfaces and
  test conventions. The implementation plan and implementation reasoning are excluded by
  default. (Revision 1's example contradicted its own recommendation here.)

### 3. Candidate snapshots and evidence binding (Phase 0–1)

Every review and gate is bound to an exact candidate, and any later change invalidates
it.

**Capture** (the user's index and working tree are left untouched):

```text
base      = git rev-parse HEAD
tmp index = GIT_INDEX_FILE=<run>/index git read-tree HEAD
            GIT_INDEX_FILE=<run>/index git add -A        # tracked + untracked, respecting .gitignore
candidate = GIT_INDEX_FILE=<run>/index git write-tree    # a tree OID: the candidate ID
commit    = git commit-tree <candidate> -p <base> -m "octet candidate"
            git update-ref refs/octet/candidates/<run>/<n> <commit>   # keeps it from gc
```

- Ignored files are not part of the candidate. Anything a gate depends on there is
  recorded under the environment identity, or the gate is refused.
- Gates and reviewers run in a **disposable checkout** of the candidate commit
  (`git worktree add --detach`, or an export for `candidate-export`). The user can go on
  editing the live tree, and the evidence still refers to the exact tree that was
  checked.
- A worktree is **not a security boundary**: a process can write through `../`. It is a
  correctness boundary. Tampering is caught by re-hashing (below), not prevented.

**Evidence record** (journaled; schema-versioned):

```json
{
  "schema": "octet.evidence/1",
  "run": "r_…", "step": "review", "attempt": 1,
  "base": "<commit>", "candidate": "<tree oid>",
  "inputs": {"pack_sha256": "…", "artifacts": {"diff": "sha256:…"}},
  "binding": {"engine": "claude", "model": "…", "cli_version": "2.1.289", "mode": "default", "level": "mediated"},
  "gate": {"command": ["cargo", "test", "--locked"], "env_sha256": "…", "toolchain": "rustc 1.98.1", "lockfile_sha256": "…"},
  "execution": "completed",
  "decision": "request_changes",
  "after": {"checkout_tree": "<tree oid>", "workspace_tree": "<tree oid>"}
}
```

**Invalidation:**

- If the disposable checkout's tree after the step differs from the candidate, the step
  changed what it was judging, and the evidence is void.
- If the live workspace's tree differs from the candidate when the result is shown, the
  result is **stale**. It is shown as stale and does not count for advancement.
- The final human gate shows the candidate ID, the base, the diff, and every evidence
  record bound to *that* candidate.

### 4. Execution status and review decision are separate

| Dimension | Values | Routing |
| --- | --- | --- |
| Execution | `completed`, `timed_out`, `cancelled`, `provider_error`, `auth_error`, `invalid_output` | Anything but `completed` **blocks advancement and never reaches the coder**. `invalid_output` gets at most one formatting retry in the same session; provider and auth errors follow §6 |
| Decision (only when execution is `completed`) | `approve`, `request_changes`, `inconclusive` | Only `request_changes` with findings that pass validation (file in the candidate, line in range, non-empty claim) goes back to the coder. `inconclusive` goes to the user |

**Verdict schema `octet.verdict/1`.** It is bound to run, step, attempt and candidate,
which Octet writes into the request and checks in the reply:

```text
[[OCTET_VERDICT v1 candidate=<tree oid> attempt=<n>]]
{"decision":"request_changes","findings":[{"severity":"high","file":"src/x.rs","line":42,"claim":"…","evidence":"…"}]}
```

- It is parsed **only from the designated final assistant message of that turn**, as the
  last block. It is never taken from tool output, logs or quoted text.
- A mismatched candidate or attempt makes the output `invalid_output`.

### 5. Gates: red and green as an explicit contract

A gate is a typed check run by Octet in the candidate checkout. Its success is judged on
**named test identities**, not on exit status alone.

| Gate type | Passes when |
| --- | --- |
| `baseline` | Runs on `base` before any work; records the pre-existing failures, which are excluded from later judgements and reported |
| `red-behavioural` | It compiles, every **new** acceptance test is *run* and *fails by assertion or panic*, and the baseline is otherwise unchanged. A compile error, a timeout, a missing dependency or an unrelated failure is a **gate error**, not red |
| `red-compile` | Explicit opt-in: the new tests fail to compile **only** because of the declared missing interface (matched against the declared symbols) |
| `green` | Every frozen acceptance test is reported as **run and passed** (not filtered, not `#[ignore]`d, not compiled out), and the regression suite has no new failures compared with the baseline |

**Freezing (after red):**

- Record the acceptance tests' identities (for example `crate::module::tests::name`),
  the content hashes of their source spans, and the hashes of discovery and
  configuration inputs: `Cargo.toml` `[[test]]`/`harness`/features, `.config/nextest.toml`,
  `build.rs`, `cfg` attributes on test modules, and the CI test invocation.
- Any change to these makes `green` fail as **tampered**, whatever the exit status.

**Rust specifics.** Unit tests live inside production `.rs` files, so path globs
(`tests/**`) cannot separate tests from code. Freezing is therefore by test identity and
source span, plus the run-and-passed check. That also catches `#[ignore]`, `cfg`
tricks and filtered runs. Test identities and results come from cargo-nextest's JUnit
output, or from libtest's pretty output parsed per test. Stable libtest has no JSON
format.

### 6. Durable attempts, recovery and fallback (before workflow syntax)

**Attempt lifecycle** (each transition fsynced to the run journal *before* its side
effect):

```text
planned → starting(attempt id, pre-snapshot tree) → running → finishing → completed(post-snapshot, execution, decision)
                                         └──────────→ interrupted (found on restart without a terminal record)
```

- **On restart,** any attempt without a terminal record is marked `interrupted`. The run
  **pauses** (as goals do today), and Octet first makes sure the vendor process group is
  dead.
- **Reconciliation:** diff the pre-snapshot against the current workspace, then show it.
  The user chooses to keep it (it becomes a new candidate), restore the pre-snapshot, or
  abandon the run. Octet never replays a side-effecting step on its own.
- **Cancellation** goes through the same path: interrupt, kill the group, record
  `cancelled` with a post-snapshot.

**Fallback taxonomy:**

| Failure | Effects possible? | Action |
| --- | --- | --- |
| Launch failure (binary missing, handshake refused) | No | Move to the next binding, **only if it meets the same enforcement level and access** |
| Authentication failure | No | Stop and tell the user. Never fall back silently |
| Transient throttling | Maybe | Retry the same binding with backoff, within the budget |
| Quota exhausted before any effect | No | Next binding (same conditions as launch failure) |
| Quota exhausted or crash after effects | Yes | `interrupted` → reconciliation. No automatic fallback |

**Budgets are run-wide and persistent:** total attempts, wall time, and step transitions.
Back-edge counters (`max`) count per run and **never reset**, so two interacting loops
cannot multiply. The first exhausted budget pauses the run.

### 7. Configuration trust model

- **User policy** (`~/.config/octet/policy.toml`) is the authority. It sets the allowed
  engines and models, the highest permission mode and enforcement level a role may use,
  whether workspace gates may run, and under what containment.
- **Workspace files** (`.octet/roles.toml`, `.octet/workflows/*.toml`) only **propose**.
  On first use and after any change, Octet shows the effective configuration and its
  diff from the last trusted version. The user trusts it explicitly, pinned by content
  hash (as `direnv allow` works). A workspace file can never raise a limit set by user
  policy.
- **The effective configuration is resolved and frozen into the run record** at run
  start. Edits made during a run do not affect that run.
- **Gate commands run repository-controlled code** (`build.rs`, tests, scripts). Running
  them from Octet makes them no more trustworthy than running them yourself. They run
  under the containment the user policy sets for gates, and never with full access
  unless the user opts in per workspace.

### 8. Roles (revised)

```toml
# .octet/roles.toml: proposals, capped by user policy
[roles.review]
engine = "claude"
model  = "opus"
mode   = "default"
inputs = ["task", "diff", "gate:check"]     # the plan is optional for review, off by default (anchoring)
access = "candidate-copy"
policy = { repo = "read-only", level = "contained" }
withhold_identity = true                     # the prompt does not name the author model; this is not blindness

[roles.code]
engine   = "codex"
model    = "gpt-5.5"
mode     = "untrusted"
fallback = ["claude:sonnet"]                 # used only under §6 conditions, at the same level
```

`withhold_identity` only means the prompt does not name the author's vendor or model.
Style, tool traces and instruction files may still reveal it.

### 9. First release: `@review` plus a check, on one candidate

`@review [note]`, or `/review [--check NAME]`:

1. **Capture** the candidate (§3) and refuse if it is empty. Show `base..candidate`
   (files, size).
2. **Check:** if one is configured and trusted, run it in a disposable checkout and
   record an evidence record.
3. **Review:** start a fresh reviewer session in a *second* disposable checkout. The
   inputs are the user's note (or last prompt), the bounded diff and the check result.
   Use the strongest level the binding supports: Codex `readOnly` if verified, otherwise
   `contained` by copy.
4. **Parse** the verdict (§4), re-hash the reviewer's checkout, and compare the live tree
   with the candidate.
5. **Show** the result in the main transcript: execution, decision, findings, the check
   result, the candidate ID, and **stale** if the live tree has moved on.
6. **Stated limitations,** shown in `/role` and `F1`: the reviewer can read the whole
   candidate and its history, and use the network. The result is a separate model's
   opinion plus a check that ran, not proof of correctness.

It leaves the main session as it was: no handoff, no switch.

### 10. Workflows (Phase 3, revised example)

```toml
name = "feature"

[[step]]
id = "baseline"; gate = "baseline"; run = ["cargo", "nextest", "run"]

[[step]]
id = "plan"; role = "plan"; inputs = ["task"]; emits = "plan.md"

[[step]]
id = "tests"; role = "test"
inputs = ["task", "acceptance", "interfaces", "test-conventions"]   # no plan
access = "candidate-copy"
emits  = "acceptance-tests"          # names the new test identities

[[step]]
id = "red"; gate = "red-behavioural"; tests = "acceptance-tests"
then = "freeze"

[[step]]
id = "code"; role = "code"
inputs = ["task", "acceptance", "interfaces", "plan", "gate:red"]

[[step]]
id = "green"; gate = "green"; tests = "acceptance-tests"
on_fail = { goto = "code", escalate = "claude:opus" }

[[step]]
id = "review"; role = "review"; inputs = ["task", "diff", "gate:green"]
on_request_changes = { goto = "code", with = "findings" }

[[step]]
id = "ship"; human = true            # shows the candidate and its evidence

[budget]
attempts = 12; wall = "90m"; transitions = 20
```

Two reviewers agreeing never stands in for `green`. Agreement is recorded, and the gates
still decide.

### 11. Evaluation and the scoreboard

- **Evaluate before promoting:** compare single agent, same-model review, and
  separate-model review on a fixed set of representative Octet tasks. Measure accepted
  findings, false positives, regressions found later, completion time and human
  interventions, not only the test pass rate.
- **The scoreboard is descriptive.** Each row records task category, model and CLI
  version, effort, context budget, attempt number, starting state (fresh or inherited
  failure), gate definition hash and reviewer binding. It shows sample sizes and
  intervals. Rejection rates are labelled as a mix of reviewer strictness and code
  quality.
- **Suggestions only, under user control.** No automatic routing until comparable data
  shows it improves outcomes.
- **Quota and money are shown as "unknown"** where the CLI does not expose a reliable
  measurement.

### 12. Native subagents and ACP

- Compiling roles into vendor subagents (Claude `model:`, Codex agent TOML, opencode
  agents) stays out of scope. It works within one vendor only, and opencode already does
  not pass subagent approvals on.
- **A generic ACP provider is its own adapter project, not a table row.** It needs
  authentication handling, capability discovery, cancellation, session recovery, and
  its own row in the capability matrix with gate evidence.

## Delivery order

| Phase | Deliverable | Exit criterion |
| --- | --- | --- |
| 0 | Capability matrix, verified by gate scenarios per pinned CLI version. Codex `readOnly` probe. Candidate capture and disposable checkouts. Trust model for config | Every *verify* cell resolved; role configurations the adapter cannot enforce are refused |
| 1 | Role bindings; fresh-session `@review` over a captured candidate; stated limitations | Evidence records and stale detection work end to end |
| 2 | Snapshot-bound gates (baseline, red, green, freeze); verdict schema v1; `/goal` audit by a separate role, described as separate review | Adversarial scenarios below pass |
| 3 | Linear workflows on the durable attempt lifecycle: recovery, budgets, cancellation | Crash-between-effect-and-journal scenario reconciles correctly |
| 4 | Parallel worktrees, routing experiments (descriptive), phone approvals, ACP adapter | Evaluation shows the benefit over baselines |

`/goal` migrates onto the step primitive only after Phase 3's semantics are stable.

## Deterministic scenarios (testkit), required before going past the MVP

1. The reviewer writes through the shell when edits are denied (Claude `default`), and
   the change is detected by re-hash.
2. A plugin hook answers an approval for a `mediated` role, and the role is refused, or
   hooks are forced off.
3. A reused session carries context from an earlier step (warm reuse refused).
4. A compile error is mistaken for red, and `red-behavioural` reports a gate error.
5. The implementer adds `#[ignore]`, a `cfg` change or a filtered run, and `green` reports
   tampering.
6. A stale approval: the live tree changes after review, and the result is shown stale
   and blocks advancement.
7. A malformed, truncated or mismatched verdict gives `invalid_output`, then one retry,
   with no coder loop.
8. Partial edits, then quota exhausted: interrupted, with no fallback.
9. A crash between a side effect and the journal write: on restart, paused,
   reconciled, never replayed.
10. A workspace config that raises the permission mode is refused without user trust and
    capped by user policy.

## What changed from revision 1

| Review point | Change |
| --- | --- |
| 1. Approval interception is not enforcement | Capability matrix (§1) grounded in the current adapters; enforcement levels; configurations that cannot be enforced are refused; repository read-only kept separate from no writes |
| 2. `sees` is not isolation | Split into `inputs` and `access` (§2); fresh sessions; test writer gets no plan |
| 3. A weak red gate | Typed gates with named-test contracts, freezing, and Rust-specific handling (§5) |
| 4. Snapshot binding | Candidate capture, disposable checkouts, evidence records, invalidation (§3) |
| 5. Status vs. decision | Separate dimensions; versioned, bound verdict; final message only (§4) |
| 6. Recovery and fallback | Attempt lifecycle, reconciliation, fallback taxonomy, run-wide budgets (§6) |
| 7. Config trust | User policy as the ceiling, hash-pinned trust, frozen effective config, gate execution policy (§7) |
| 8. Overclaiming verification | "Separate-model review plus executed checks"; the paper's scope restated; an evaluation plan (§11) |
| 9. Novelty | `clink` credited; positioning narrowed; ACP is an adapter project (§12) |
| 10. Scoreboard | Descriptive only, confounders recorded, unknowns shown as unknown (§11) |
| 11. Example and scope | Revised example (§10); `withhold_identity`; agreement never replaces gates; `/goal` migrates last; phases reordered |

## Sources

- [PAL MCP `clink`](https://github.com/BeehiveInnovations/pal-mcp-server/blob/main/docs/tools/clink.md), [PAL MCP](https://github.com/beehiveinnovations/zen-mcp-server)
- [Agent Client Protocol: tool calls and permissions](https://agentclientprotocol.com/protocol/tool-calls), [Zed external agents](https://zed.dev/docs/ai/external-agents)
- [Anthropic: Claude Code sandboxing](https://www.anthropic.com/research/claude-code-sandboxing)
- [Cross-Model LLM Code Review (arXiv 2607.21656)](https://arxiv.org/html/2607.21656v1), [Label-induced bias (arXiv 2508.21164)](https://arxiv.org/pdf/2508.21164), [Paraphrasing attack on LLM reviewers (arXiv 2601.06884)](https://arxiv.org/pdf/2601.06884)
- [opencode agents](https://opencode.ai/docs/es/agents), [Roo Code modes](https://docs.roocode.com/basic-usage/using-modes), [Kilo migration guide](https://kilo.ai/articles/roo-to-kilo-migration-guide), [Aider architect mode](https://www.promptlayer.com/glossary/aider-architect-mode)
- Claude Code subagents (secondary): [morphllm](https://www.morphllm.com/claude-subagents), [PubNub](https://www.pubnub.com/blog/best-practices-claude-code-sub-agents/)
- Codex agents: [custom agent TOML guide](https://codex.danielvaughan.com/2026/04/27/codex-cli-custom-agent-definitions-toml-specialised-subagents/), [openai/codex#19482](https://github.com/openai/codex/issues/19482)
- [Archon](https://betterstack.com/community/guides/ai/archon-ai/), [RouteLLM](https://lmsys.org/blog/2024-07-01-routellm/), [Not Diamond Code](https://www.notdiamond.ai/blog/not-diamond-code-intelligent-model-routing-for-coding-agents)
- TDD isolation: [agentpatterns.ai](https://agentpatterns.ai/verification/red-green-refactor-agents/), [markaicode](https://www.markaicode.com/multi-agent-coding-test-writer/)
- [SD Times: decorrelating reviewers](https://sdtimes.com/sdt_dev/frontier-models-write-the-code-local-models-should-review-it/)
