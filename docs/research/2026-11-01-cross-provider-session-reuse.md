# Reusing session data across provider switches: design

**Date:** 2026-11-01.
**Goal:** spend less context (subscription quota, or tokens) when the user switches
provider, by reusing what each vendor session already holds instead of rebuilding it.
**Related:** [provider handoff design](../superpowers/specs/2026-10-08-provider-handoff-design.md),
[role-based workflows](2026-11-01-role-based-model-workflows.md).

## What happens today

- **A cross-provider `/model` always opens a fresh vendor session.**
  `Selection::configure` (`crates/octet-core/src/model.rs`) sets `resume: None` whenever
  the provider changes.
- **The next prompt carries a rendered transcript** of the whole conversation, across
  every provider, up to 64 KiB (`crates/octet-tui/src/handoff.rs`). It keeps the first
  prompt and the newest turns, and shortens each tool call to one line of at most 200
  characters.
- **So switching A → B → A wastes A's session.** A's own vendor session already holds
  everything from before the switch, with full tool outputs. Octet discards it, opens
  a new A session, and sends A the whole transcript again, now including the part A
  wrote itself.
- **Usage is not measurable yet.** `Event::Usage` is a display string per vendor
  (Claude: session cost; Codex: context in use; opencode: total tokens). Octet cannot
  see cached against uncached input, so it cannot show a saving today.

## Constraints that shape the design

1. **Prompt caches belong to each vendor and expire.** They last minutes to an hour,
   depending on vendor, model and setting. Nothing cached by Claude helps Codex.
   Cross-provider reuse therefore means **not resending and not re-deriving**. It does
   not mean sharing a cache.
2. **Resuming does not make history free.** A resumed vendor session re-reads its whole
   history on the next turn. If its cache has gone cold, and its history (full tool
   outputs) is bigger than a 64 KiB handoff, **resuming can cost more than a fresh
   session**. The choice has to be made per switch, and measured.
3. **The workspace may have changed while a provider was away.** A resumed session's
   earlier file reads may be stale. Resuming has to say what changed.
4. **Reuse conflicts with isolation.** The role design requires fresh sessions for
   isolation roles (reviewer, test writer). Reuse applies only to the **conversation
   lane**: the main session that `/model` switches, and continuity steps that opt in
   (`resume = true`, for example a coder on a rework loop). An isolation role never
   resumes, and its cost is kept down by small context inputs instead.

## Design

### 1. Measure first: structured usage

Add a structured usage event next to the display string, so the journal and RPC
consumers that read `usage` keep working:

```json
{"type":"usage_detail","engine":"claude","model":"…","input":1234,"cached_input":56000,
 "cache_write":800,"output":900,"context":61000,"cost_usd":0.0123}
```

- Every field is optional. A field the CLI does not report is **absent**, never 0.
- Claude's stream-json `result` reports its usage fields and cost. Codex's `tokenUsage`
  reports token counts; whether it separates cached input is to be verified. opencode
  reports totals.
- The journal records, per turn, the handoff it carried (`kind: full | delta | none`,
  bytes, turns). That makes it possible to compare "resume plus delta" with "fresh plus
  full handoff" on real sessions.

Without this phase, any claim of savings is a guess.

### 2. A session ledger: switching back resumes

For each Octet conversation, keep the latest vendor session per provider:

```json
{"type":"provider_session","engine":"claude","session":"…","model":"…",
 "upto":182, "left_at":"2026-11-01T21:40:00Z", "tree":"<git tree oid>", "context":61000}
```

| Field | Meaning |
| --- | --- |
| `upto` | The sequence number of the last transcript entry this vendor session has seen |
| `tree` | The workspace snapshot when Octet left this provider, using the same capture as role design §3 |
| `context` | The last known context size, where the vendor reports one |

The ledger is journaled, so it survives a restart and `/resume N`.

**When switching to provider P:**

1. If the ledger has an entry for P, and the policy (§4) chooses to resume:
   `configure` sets `resume = Some(ledger[P].session)`. `configure` stays pure: it takes
   the prior entry as an argument.
2. The next prompt carries a **delta handoff**: only the transcript entries after
   `ledger[P].upto`, plus the workspace changes since `ledger[P].tree`.
3. If the resume fails (the session expired or was deleted, or Claude's fresh session
   had no ID yet), fall back to today's behaviour: a fresh session and the full handoff,
   with a notice saying so.

**Delta handoff format.** It uses the same guarded block and random tag as today, with a
different preamble:

```text
[Octet handoff] You are resuming your own earlier session. Since your last turn the user
continued with another assistant; what happened since then is reproduced below as
context. Do not redo its actions; tool calls already ran. Files changed since you last
saw the workspace (re-read them before relying on earlier reads):
  M crates/core/src/model.rs
  A crates/core/src/ledger.rs
<earlier-conversation-…>
…only entries after `upto`…
</earlier-conversation-…>
```

The changed-files list comes from `git diff --name-status <ledger tree> <current tree>`.
It costs a few hundred bytes and prevents the stale-read error that resuming would
otherwise invite.

**Other ledger rules:**

- `/new` clears the ledger.
- `/fork` keeps the other providers' entries, because their history is a shared prefix
  of the fork.
- A same-provider model switch already resumes, so it is unchanged.
- `/goal` still pauses on a switch.

### 3. A leaner full handoff

When Octet has to start a fresh session, send less:

- **A deterministic digest of older turns.** For turns beyond the newest few, send:
  the files read and edited (paths only), the commands run with their exit status, and
  each assistant reply's first paragraph. Recent turns stay verbatim. No model call,
  so the digest itself costs nothing.
- **A working set.** List the files the earlier providers read and edited, marked
  changed or unchanged. The new model can then read the right files directly instead of
  exploring. This is the main hidden cost of a switch, and it should be measured
  (exploration tool calls on the first turn after a switch).
- **A configurable budget,** keeping 64 KiB as the default. The data from Phase 1
  decides whether a smaller default is better.
- **An optional summarizer role** (Phase 4). A cheap binding turns a long transcript
  into a short brief before a switch. It is worth it only when the brief stays in the
  new session's history over many turns. That is an empirical question, so it is off by
  default.

### 4. Choosing between resume and fresh

| Signal | Prefer resume | Prefer fresh plus handoff |
| --- | --- | --- |
| Time since leaving P | Recent; the cache plausibly still warm | Long ago |
| P's last known context size compared with the handoff size | Similar or smaller | Much larger (bloated with tool output) |
| Size of the delta since `upto` | Small | Most of the conversation happened elsewhere |
| P's earlier session ended with an error or a reported compaction failure | | Yes |

- **Start with a simple rule:** resume when the ledger has an entry and the resume
  succeeds.
- **Then use the Phase 1 data** to set a threshold. Show the decision in the switch
  notice, for example "Resumed your claude session (left 3 min ago); carried 4 turns,
  2 KiB" or "New codex session; carried 14 turns, 22 KiB".
- **`/model --fresh`** forces a new session.
- The policy decides **cost only**. It never overrides isolation (constraint 4).

### 5. Routing switches through opencode (optional, user's choice)

opencode owns its message history and converts it between providers itself. When both
the source and target models are reachable through opencode, a switch can stay inside
one opencode session, with full history and no Octet handoff. The trade-off belongs to
the user: opencode uses its own provider credentials (often pay-per-token), not the
Claude or ChatGPT subscription. Octet should **offer** this route, never pick it on its
own.

## Where it goes in the code

| Change | Location |
| --- | --- |
| `usage_detail` event; parse Claude `result` usage, Codex `tokenUsage`, opencode totals | `octet-engine/src/live/{claude,codex,opencode}.rs`, `octet-core` event record |
| `ProviderSession` ledger type, journal record, reload on resume | `octet-core` (new `ledger.rs`), `octet-store` |
| `Selection::configure(current, session, binary, prior: Option<&ProviderSession>)` | `octet-core/src/model.rs` |
| Delta render: entries after `upto`, changed-files preamble; digest and working set for the full render | `octet-tui/src/handoff.rs`, `app.rs` (where `render` is called today) |
| Resume-failure fallback to the full handoff | `octet-tui/src/reconnect.rs` |
| Switch notices; `/model --fresh` | `octet-tui/src/commands.rs`, `models.rs` |

## Tests (fake vendors)

1. A → B → A resumes A's vendor session, and the wire carries only B's turns plus the
   changed files.
2. A → B → A where A's resume fails falls back to a fresh session and the full handoff,
   with a notice.
3. Files changed by B appear in the delta's changed-files list.
4. `/new` clears the ledger, so the next switch is fresh.
5. `/resume N` restores the ledger from the journal.
6. The ledger survives a restart.
7. A three-provider chain A → B → C → A gives A the delta covering B's and C's turns.
8. An isolation role never resumes, even when a ledger entry exists.
9. A usage field the vendor did not report is absent from `usage_detail`, never 0.

## Phases

| Phase | Deliverable | Exit criterion |
| --- | --- | --- |
| 1 | Structured usage, and handoff kind and size in the journal | Cached and uncached input visible per turn for Claude and Codex |
| 2 | Ledger, resume on return, delta handoff with changed files, fallback | Tests 1–9 pass; live A → B → A check on both CLIs |
| 3 | Leaner full handoff (digest, working set, configurable budget) | Phase 1 data shows a lower first-turn cost, and no rise in redo or exploration |
| 4 | Resume-or-fresh policy thresholds from data; optional summarizer; opencode route | Policy beats "always resume" on recorded sessions |

## Open questions

- Does Codex's `tokenUsage` separate cached input? This needs a gate scenario.
- Does Claude Code keep an expired session resumable, and for how long, at the pinned
  CLI versions?
- Should the ledger be kept per conversation (proposed) or per workspace? Per workspace
  would allow reusing a provider session across Octet conversations, but it would mix
  histories, so it is not proposed.

## Sources

- Caching TTLs and pricing (secondary sources; the vendors' official docs are
  authoritative): [DEV: prompt caching in 2026](https://dev.to/alexcloudstar/prompt-caching-in-2026-anthropic-vs-openai-vs-gemini-for-production-apps-433i),
  [Future AGI](https://futureagi.com/blog/understanding-prompt-caching-for-faster-ai-responses/),
  [OpenAI retention test (gist)](https://gist.github.com/monperrus/9784f52d5144975fd11be7f642c4a8fc),
  [Claude TTL default change claim (unverified)](https://dev.to/whoffagents/anthropic-silently-dropped-prompt-cache-ttl-from-1-hour-to-5-minutes-5gm8)
- Octet: [provider handoff design](../superpowers/specs/2026-10-08-provider-handoff-design.md) (pi's and opencode's history conversion)
