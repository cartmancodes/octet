# Role-based model workflows in Octet: research

**Date:** 2026-11-01.
**Question:** how can Octet let a user pick one model/provider for coding, another for
testing, another for reviewing, and chain them into workflows? Which approaches are new
rather than copies of what other tools already do?

## Summary

Most tools that offer "a model per role" own their agent loop and call model APIs with
API keys: Aider (architect/editor), Roo/Kilo (sticky models per mode), opencode
(per-agent `model`), PAL MCP (consensus and multi-model review). Octet is different. It
drives the vendors' **own CLIs on the user's own subscriptions**, and it is already the
approval authority and the journal for every session. That position allows things the
API-key tools cannot do:

1. **Cross-vendor pipelines on subscriptions.** Claude plans, Codex codes, Claude reviews,
   each through its official CLI and login. No API keys, and Octet still never reads
   credentials.
2. **Octet controls what each step can see.** Every cross-provider prompt is already
   built by Octet (`handoff.rs`), so Octet can enforce *information barriers*: a test
   writer sees only the spec, a reviewer sees the diff but not the coder's reasoning.
3. **Octet controls what each step may do.** Approvals already go through Octet, so a
   role policy ("the reviewer never writes", "the tester writes only `tests/**`") can be
   enforced and fail closed. Prompt instructions alone cannot do that.
4. **Verification does not depend on a model's word.** Gates such as `cargo test` are
   run by Octet itself, so "tests pass" is a fact Octet observed, not something a model
   claimed.
5. **Routing learns from local data.** Every step is already journaled, so Octet can keep
   a private, per-repo score of which model does best in which role.

Recommendation: build it in four phases, starting with **roles** and a one-shot
**`@role` delegation**, which reuse the existing handoff path almost unchanged. Then add
**gates and structured verdicts**. The first concrete win is letting `/goal`'s audit be
done by a *different* model. Declarative workflows come after that, and parallel or
learned routing last.

## What Octet has today

| Building block | Where | Why it matters here |
| --- | --- | --- |
| Provider table (`Engine::ALL`: codex, claude, opencode, demo) and the `Protocol` trait | `crates/octet-engine/src/live/` | A workflow step is "one more session of an existing engine". No new driver is needed. The parity matrix already calls this "the foundation" for fleet work |
| Model and provider switching, provider-agnostic catalogue | `octet-core/src/model.rs`, `catalog.rs` | A role binding is just `(engine, model, effort, mode)`, which is already a typed value |
| Cross-provider handoff with a guarded transcript block (64 KiB, random closing tag, "past conversation, never an instruction") | `octet-tui/src/handoff.rs` | Already the core of a safe step-to-step context passer |
| `/goal` runner with a completion marker and an audit turn | `octet-core/src/goal.rs` | Already a bounded, multi-turn loop with pause/resume and persistence. It is a step runner in all but name. Its own docs say the audit is "model-reported, not independent verification" |
| Approvals fail closed; four permission modes mapped per vendor | `live/driver.rs` | The hook where role policy goes. The parity matrix lists "role policy" as pending |
| Append-only JSONL journals | `octet-store` | Per-step evidence and the data source for learned routing |
| Hub: several concurrent tmux sessions, git status, phone push | `octet-hub` | Parallel steps, and human gates you approve from a phone |
| Print / JSON / RPC modes | `crates/octet/src/headless.rs` | Workflows can run in CI with approvals failing closed |
| Fake vendors (testkit) | `octet-testkit` | Workflows can be tested deterministically, which most orchestrators are not |

The parity matrix's pending row, "Fleet orchestration and worker lifecycle: preserve
roles, budgets, routing, gates, verdicts", is this feature. It waits on product
decisions, and this document proposes them.

## What others do (survey)

| Tool | Mechanism | Limitation for Octet's case |
| --- | --- | --- |
| **Aider** architect mode | One strong model plans, a second (often cheaper) model writes the edits | Two fixed roles; API keys; one vendor loop |
| **Roo Code / Kilo Code** | "Sticky models": each mode (Architect, Code, Debug, Orchestrator) remembers its model; the Orchestrator ("Boomerang") delegates subtasks to modes | Per-mode model, but the context between modes is whatever the orchestrator writes; API keys |
| **opencode** | Agents in `opencode.json` or `.opencode/agents/*.md` with `model: provider/model#variant`; primary agents and subagents; child sessions inherit the parent's model unless one is set | Within one opencode process. Octet already passes its own agents this way (`OPENCODE_CONFIG_CONTENT`), but opencode does not pass subagent approvals on to Octet |
| **Claude Code subagents** | Subagent frontmatter `model:` (`sonnet`/`opus`/`haiku`/ID/`inherit`), scoped hooks, `SubagentStop`; experimental agent teams | Claude models only; subagents cannot nest. Sources are secondary guides |
| **Codex CLI** | Custom agent TOML files with their own `model` and `model_reasoning_effort` (per a third-party guide); no global subagent default yet (open feature request) | OpenAI models only |
| **PAL MCP** (formerly Zen) | An MCP server that lets Claude Code consult Gemini/OpenAI/etc. for `consensus` and multi-pass `codereview`; `clink` bridges other CLIs | API keys; the host model decides when to call, so there is no enforced pipeline |
| **Archon** | YAML DAG workflows (`depends_on`, loops), AI steps mixed with deterministic shell steps, a worktree per run, drives Claude Code | One vendor. Per-step model choice is not clearly documented |
| **Model routers** (RouteLLM, Not Diamond Code) | Choose a model per request or per turn from learned preference data | Cloud or API level. Coding-specific claims are vendor-reported |
| **ACP** (Agent Client Protocol) | A standard client↔agent protocol (Zed, JetBrains registry; adapters for Claude, Codex, Gemini) | Octet already speaks it to opencode. It is a route to "any ACP agent as a role" |

### What the evidence says

- **Self-preference bias in LLM judges is well documented.** Models rate their own
  output higher. This is the main argument for a reviewer from a different model.
- **Cross-vendor review helps, but depends on direction.** A 2026 controlled study
  (116 LiveCodeBench tasks) found Claude Opus 4.7 reviewing Codex GPT-5.5 drafts raised
  pass rate from 71.6% to 89.7%. The reverse direction *lowered* it, from 91.4% to
  82.8%. So the reviewer binding should be configurable and *measured per repo*, not
  hard-coded to "always the other vendor".
- **Decorrelating the reviewer's task matters as much as changing its model.** Giving the
  reviewer a different, structured question (named criteria and a typed verdict) reduces
  correlated misses and self-preference. This is cheap, and Octet can enforce it.
- **Information barriers for TDD.** Practitioner reports agree that when one context
  writes both tests and code, the implementation plan leaks into the tests. The common fix
  is: the test writer sees only the spec, and the implementer sees only the failing tests,
  with a "do not edit tests" rule. This evidence comes from practitioners, not controlled
  studies.
- **Multi-agent costs tokens.** Review is the cheapest place to add agents because
  reviewers do not write. Cheap-model-first with escalation is the common cost pattern.

## Proposed design

### 1. Roles: a named binding, not a mode

```toml
# .octet/roles.toml  (workspace), merged over ~/.config/octet/roles.toml (user)
[roles.plan]
engine = "claude"
model  = "opus"
effort = "high"
mode   = "ask"

[roles.code]
engine   = "codex"
model    = "gpt-5.5"
mode     = "accept-edits"
fallback = ["opencode:anthropic/claude-sonnet-4-5", "claude:sonnet"]   # quota or launch failure

[roles.test]
engine = "claude"
model  = "sonnet"
mode   = "accept-edits"
policy = { writes = ["tests/**", "**/*_test.rs"] }    # enforced by Octet's approval gate

[roles.review]
engine = "claude"
model  = "opus"
mode   = "ask"
policy = { writes = [] }      # every edit request is denied, with a reason the model sees
blind  = true                 # the reviewer is not told which vendor wrote the code
```

- The binding uses the same names as `/model` and `--engine`, so the catalogue,
  validation and pickers are reused.
- `policy.writes` is the "role policy" row of the parity matrix. It is enforced in the
  driver's approval path, so it fails closed and does not rely on the prompt.
- `fallback` is an ordered chain. It is the first, simple form of quota routing.

**UI.** `/role` lists roles and their bindings. `/role code` makes a role the main
session (a switch, through today's handoff). The sidebar shows which role is active.

### 2. One-shot delegation: `@review`, `@test`

The cheapest and most useful first step. In the composer, `@review look at my last
change` does the following:

1. Opens (or reuses, kept warm) the reviewer's session.
2. Sends it a **context pack** built by Octet, not the full transcript: the task, the
   `git diff`, and optionally the plan.
3. Streams its reply into the main transcript, tagged with the role.
4. Leaves the main session where it was.

This is Aider's architect/editor split and PAL's "second opinion", but across the user's
own vendor subscriptions, without API keys, and with Octet choosing the context. It
reuses `handoff.rs` (the block framing) and the session machinery the hub already uses
for concurrent sessions.

### 3. Gates and typed verdicts

Two kinds of check between AI steps:

- **Deterministic gates**, run by Octet: `run = "cargo test"`, `expect = "pass" | "fail"`.
  The model never reports the result; Octet observes the exit status and output.
- **Verdicts**, from a reviewer or auditor role, as a typed final block, extending the
  existing `[[OCTET_GOAL_COMPLETE]]` marker:

  ```
  [[OCTET_VERDICT]]
  {"decision":"reject","findings":[{"severity":"high","file":"src/x.rs","line":42,"claim":"…"}]}
  ```

  Octet parses it, journals it, and routes on `decision`. A verdict that is missing or
  malformed counts as `reject`, so it fails closed.

**First concrete win: an independent `/goal` audit.** Today `/goal complete` asks the
same model to audit its own work. Add `/goal audit-by review` (or a default
`goal.auditor = "review"`), so the audit runs in the reviewer role with only the goal
text and the diff, plus an optional `goal.check = "cargo test"`. That turns the
documented "model-reported audit, not independent verification" into independent
verification, with very little new machinery.

### 4. Workflows: a short declarative pipeline

```toml
# .octet/workflows/feature.toml
name = "feature"
description = "Plan → tests first → implement → independent review"

[[step]]
id    = "plan"
role  = "plan"
sees  = ["task"]
emits = "plan.md"              # stored as a run artifact

[[step]]
id    = "tests"
role  = "test"
sees  = ["task", "plan"]       # the information barrier: no implementation in sight

[[step]]
id     = "red"
run    = "cargo test"
expect = "fail"                # new tests must fail first

[[step]]
id   = "code"
role = "code"
sees = ["plan", "gate:red"]    # the failing output, not the test writer's reasoning

[[step]]
id      = "green"
run     = "cargo test"
expect  = "pass"
on_fail = { goto = "code", max = 3, escalate = "claude:opus" }   # a cascade: cheap first, then stronger

[[step]]
id        = "review"
role      = "review"
sees      = ["task", "plan", "diff"]
verdict   = true
on_reject = { goto = "code", max = 2, with = "findings" }

[[step]]
id     = "ship"
human  = true                  # waits for you; the hub can push it to your phone
```

Rules that keep it small and testable:

- **Linear steps with bounded back-edges** (`goto` plus `max`), not a general DAG
  language. This matches what `/goal` already does (bounded continuation with a guard)
  and avoids Archon-style YAML sprawl.
- **`sees` is explicit and whitelisted.** The inputs are `task`, `plan` (or any emitted
  artifact), `diff`, `gate:<id>`, `findings`, and `transcript:<step>`. Nothing else
  reaches the model. Each pack has a size budget (as handoff has its 64 KiB).
- **Each step gets a fresh vendor session by default.** `resume = true` opts back into
  continuity, for example the `code` step on a rework loop.
- **Runs are journaled as a tree:** one run ID, one journal per step, plus artifacts.
  `/flow status` shows the pipeline in the sidebar, and `/flow resume` continues after
  a restart, coming back paused like goals do.
- **The same file runs headless:** `octet --flow feature -p "Add rate limiting"` for CI,
  with approvals failing closed as `--print` does today.

### 5. Ideas beyond the baseline

| Idea | What it does | Why Octet is well placed |
| --- | --- | --- |
| **Blind review** | The reviewer is told the code was written "by a developer", never which vendor or model wrote it | Reduces self- and provider-preference bias. Octet writes the prompt, so this is enforced, not requested |
| **Disagreement as a signal** | `review` may list several roles (`["review", "review-alt"]`). When they agree, continue automatically. When they disagree, stop for a human with both views side by side | Turns the cost of two reviewers into a calibrated escalation, not a vote |
| **Cross-vendor best-of-N** | A `fanout` step runs the `code` role on two or three bindings in separate git worktrees; gates prune the failures; a judge role picks one, or the user does | The hub already supervises several sessions and does git work. Worktree isolation is the missing piece |
| **Quota-aware routing** | On a rate-limit or usage-cap error, move to the next binding in `fallback`. Later, prefer bindings with more quota left | Subscription limits are the user's real constraint, and only a tool that runs the CLIs sees them |
| **A local scoreboard** | From the journals, record per repo and per role: gate pass rate, review rounds, wall time, usage. `/role stats` suggests rebinding ("in this repo, codex:gpt-5.5 passes `green` on the first try 78% of the time against 61% for sonnet") | A private, per-repo RouteLLM. The data already exists and never leaves the machine |
| **Escalation cascade** | Start each role on a cheaper binding and escalate after N failed gates (`escalate =`) | The common cost pattern, driven by observed gates rather than a guess |
| **Phone gates** | A `human` step or a rejected verdict sends a push; approve or reject from the phone | Unique to Octet's hub and mobile app |
| **Replayable runs** | Re-run a workflow against the fake vendors (testkit) or the demo engine | Workflows can be tested in CI, which almost no orchestrator offers |
| **ACP roles** | Any ACP-speaking agent (Gemini CLI, Copilot CLI and others in the ACP registry) becomes a binding through a generic ACP provider | opencode is already driven over ACP, so a generic ACP provider is one table row away |

### 6. Native subagents: delegate or orchestrate?

Two ways to get "a model per role":

- **(A) Octet orchestrates vendor sessions** (proposed above). It works across vendors,
  and Octet keeps approvals, context and journals.
- **(B) Compile roles into vendor-native subagents**: Claude subagent files with
  `model:`, Codex agent TOML, opencode agents. These are cheaper (one process) and keep
  vendor-native delegation, but they work within one vendor only, and Octet loses
  approval visibility. opencode already does not pass subagent approvals on.

**Recommendation: build A.** Offer B later as an optional "export roles to vendor
config", for users who stay inside one vendor.

## Mapping to crates

| Crate | Change |
| --- | --- |
| `octet-core` | `Role`, `RoleBinding`, `RolePolicy`, `Workflow`, `Step`, `Gate`, `Verdict`, `ContextPack` types; config loading and merging; generalise `goal.rs`'s runner into a step runner (goals become a one-step workflow) |
| `octet-engine` | Driver: apply `RolePolicy` in the approval path (deny with a reason, fail closed). Classify rate-limit and quota errors for `fallback`. No new provider is needed |
| `octet-tui` | `@role` in the composer; `/role`, `/flow`; role-tagged transcript entries; pipeline in the Task sidebar; a context-pack builder next to `handoff.rs` |
| `octet-store` | A run ID linking step journals; an artifacts directory per run (plan, diffs, gate logs, verdicts) |
| `octet-hub` | Worktrees for `fanout`; push for `human` steps and rejected verdicts; flows started from the phone |
| `octet` (bin) | `--flow NAME`, `--role NAME`; RPC commands for flows |
| `octet-testkit` | Scripted multi-session scenarios: a reviewer rejects, a gate fails, a fallback fires |

## Phased plan

1. **Roles and `@role` delegation.** `roles.toml`, `/role`, `@review`/`@test` one-shots
   with a diff-based context pack, and role policy (no writes for the reviewer). Small,
   and useful on day one.
2. **Gates, verdicts and an independent goal audit.** The `[[OCTET_VERDICT]]` block,
   Octet-run checks, and `/goal` audited by another role.
3. **Workflow files.** Linear steps with bounded loops, `sees` barriers, artifacts,
   resume, and headless `--flow`.
4. **Scale-out.** Worktree fan-out, quota-aware fallback, the local scoreboard and its
   suggestions, and phone gates.

## Risks and open decisions

- **Cost and latency.** Every extra role adds a session and tokens. Keep sessions warm
  and lazy, show usage per step, and make review the default extra role (it is
  read-only and has the best return).
- **Context loss between steps** is the main failure mode of multi-agent setups. Context
  packs must be explicit, bounded and visible (`Ctrl+O` shows exactly what a step saw).
- **Approval fatigue.** Several agents can each ask for approval. Role policies, and
  `accept-edits` scoped to a role's allowed paths, reduce the prompts. The cap of eight
  waiting approvals should probably apply per run.
- **Vendor terms.** Check that running several concurrent, automated sessions on a
  consumer subscription is allowed by each vendor's terms before shipping fan-out.
- **Decisions needed** (the parity matrix's "product decisions"): config format (TOML is
  proposed, which matches Codex and Cargo); whether run artifacts live in Octet's store,
  which ties to the pending history-store decision; and whether native subagents (B) are
  in scope at all.

## Sources

- [opencode agents docs](https://opencode.ai/docs/es/agents), [v2](https://v2.opencode.ai/agents)
- [Roo Code: using modes / sticky models](https://docs.roocode.com/basic-usage/using-modes), [Kilo: Roo migration guide](https://kilo.ai/articles/roo-to-kilo-migration-guide)
- [Aider architect mode (glossary)](https://www.promptlayer.com/glossary/aider-architect-mode), [agentwiki: aider](https://agentwiki.org/aider)
- Claude Code subagents (secondary guides): [morphllm](https://www.morphllm.com/claude-subagents), [PubNub](https://www.pubnub.com/blog/best-practices-claude-code-sub-agents/), [Mintlify best practices](https://www.mintlify.com/shanraisshan/claude-code-best-practice/best-practices/subagents)
- Codex agents: [custom agent TOML guide](https://codex.danielvaughan.com/2026/04/27/codex-cli-custom-agent-definitions-toml-specialised-subagents/), [openai/codex#19482](https://github.com/openai/codex/issues/19482)
- [PAL MCP (formerly Zen)](https://github.com/beehiveinnovations/zen-mcp-server)
- [Archon (Better Stack guide)](https://betterstack.com/community/guides/ai/archon-ai/)
- [Cross-Model LLM Code Review (arXiv 2607.21656)](https://arxiv.org/html/2607.21656v1), [Chain-of-Models (arXiv 2607.28636)](https://arxiv.org/pdf/2607.28636), [Label-induced bias (arXiv 2508.21164)](https://arxiv.org/pdf/2508.21164), [Paraphrasing attack on LLM reviewers (arXiv 2601.06884)](https://arxiv.org/pdf/2601.06884)
- [SD Times: local models should review](https://sdtimes.com/sdt_dev/frontier-models-write-the-code-local-models-should-review-it/), [Augment: adversarial code review](https://www.augmentcode.com/guides/adversarial-code-review), [MindStudio: cross-vendor review](https://www.mindstudio.ai/blog/cross-vendor-ai-agent-review-claude-codex)
- TDD isolation: [agentpatterns.ai red-green-refactor](https://agentpatterns.ai/verification/red-green-refactor-agents/), [markaicode](https://www.markaicode.com/multi-agent-coding-test-writer/)
- Multi-agent practice: [Tembo](https://www.tembo.io/blog/multi-agent-ai-coding-workflows), [Wavect](https://wavect.io/blog/multi-model-ai-coding-agent-stack-2026.md), [smcleod plan-act-review](https://smcleod.net/2025/04/my-plan-document-act-review-flow-for-agentic-software-development/)
- Routing: [RouteLLM (LMSYS)](https://lmsys.org/blog/2024-07-01-routellm/), [Not Diamond Code](https://www.notdiamond.ai/blog/not-diamond-code-intelligent-model-routing-for-coding-agents)
- ACP: [Zed external agents](https://zed.dev/docs/ai/external-agents), [Zed ACP progress](https://zed.dev/blog/acp-progress-report), [ACP registry (gihyo)](https://gihyo.jp/article/2026/01/acp-registry)
