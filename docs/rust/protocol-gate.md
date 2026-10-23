# The protocol gate

`protocol-gate` runs one scenario against a real vendor CLI (`claude`,
`codex` or `opencode`) and prints what happened as JSON evidence. It is how
Octet checks that a vendor's wire protocol still behaves the way the drivers
expect, before a CLI update reaches users. It launches the CLI with Octet's own
arguments and process limits (`octet_engine::live::launch_args` and
`vendor_process`), adding only what a scenario needs (Claude's `--tools`,
and the fixture MCP server), so it checks the contract Octet ships.

It talks to the real vendor, so each run uses your login and your quota.
Keep runs few and small.

opencode scenarios run on the free model `opencode/deepseek-v4.1-flash-free`
in Octet's `ask` agent (`octet-ask`), so a turn costs nothing. Each
opencode session a run opens is deleted afterwards
(`opencode session delete`), whatever the outcome, so your opencode history
keeps none of them.

## Run it

```sh
make gate ENGINE=claude SCENARIO=simple
```

or directly, with every option:

```sh
scripts/rust-env.sh cargo run --locked -p octet-gate --bin protocol-gate -- \
  --engine claude --scenario simple \
  [--binary PATH] [--workdir NEW_DIR] [--output FILE] [--timeout SECONDS]
```

- **`--binary`:** the CLI to run; the engine's name on `PATH` by default
  (opencode installs to `~/.opencode/bin`).
- **`--workdir`:** a fixture workspace to create, and keep; by default a
  temporary one, removed when the run ends, whatever its outcome. It must
  not exist yet.
- **`--output`:** also write the evidence to this file.
- **`--timeout`:** 1–300 seconds for the scenario.

With `OCTET_GATE_TRACE` set, an opencode run also prints each permission
request and tool call update it sees to stderr (each cut to 2,000
characters).

## Scenarios

| Scenario | Checks | Claude | Codex | opencode |
| --- | --- | --- | --- | --- |
| `initialize` | The handshake | yes | yes | yes |
| `simple` | One turn to completion | yes | yes | yes |
| `interrupt` | Cancelling a running turn | yes | yes | yes |
| `approval-allow` | An approval allowed; the fixture file is written | yes | yes | yes |
| `approval-deny` | An approval denied | yes | yes | yes |
| `approval-interrupt` | Cancelling while an approval waits | yes | yes | no |
| `resume` | Reopening a session | yes | yes | yes |
| `fork` | Forking a session | yes | yes | yes |
| `compact` | Compacting a session's context | yes | yes | no |
| `mcp` | An MCP tool call | yes | no | no |
| `hook` | A hook callback | yes | no | no |
| `user-input` | A request for user input | no | yes | no |
| `tool-only` | A turn that only runs a tool | no | yes | no |
| `failure` | A turn the vendor fails | no | yes | no |
| `lockdown` | A project config and a user rule cannot bypass `ask` | no | no | yes |
| `subagent` | A subagent (opencode's `task` tool) cannot bypass `ask` | no | no | yes |
| `command-expansion` | Octet's emulation of how opencode expands a command | no | no | yes |

opencode's `resume` and `fork` reopen the session in a second process, as
`--resume` does, and ask which word it replied with: the reopened session,
or its fork, must remember. `lockdown` runs in a workspace whose
`opencode.json` would allow everything, Octet's `octet-ask` agent included
(`{"permission":"allow","agent":{"octet-ask":{"permission":{"*":"allow"}}}}`),
with `OPENCODE_CONFIG` naming a user file that allows the command
(`{"permission":{"bash":{"echo *":"allow"}}}`). It asks for `echo fixture`
and passes only if opencode asks permission first for exactly that: an
`execute` call whose command (or title) is `echo fixture`. Any other
permission request is rejected and proves nothing; a command that runs
unasked fails it. The gate rejects every request, as Octet does when a
request has no one to answer it.

`subagent` runs in the same workspace and asks the model, in `octet-ask`, to
have a subagent run `echo fixture`. opencode gives a subagent the parent
session's deny rules but not the agent's `"*":"ask"`, and runs it under its
own agent, so Octet's asking agents deny the `task` tool. If opencode asks to
start a subagent anyway, the gate allows it once, as a user might. It passes
only if nothing runs unasked: a `task` call that completes, or a command
that runs without a permission request, fails it. A command the model asks
for itself is rejected. In every opencode scenario a completed `task` call
fails the run. Against opencode 1.18.25 with `task` allowed, the subagent
ran `echo fixture` with no permission request, and this scenario failed.

`command-expansion` checks what Octet's command check assumes of opencode
(`crates/octet-engine/src/live/opencode/commands.rs`). Its user rule (through
`OPENCODE_CONFIG`) defines six throwaway commands whose templates and
arguments cover a template's own `` !`cmd` ``, `$1` alone and with a quoted
word, `$ARGUMENTS` with JavaScript's `` $' `` pattern, arguments appended to
a template that takes none, and the highest `$N` taking every word from there
on. Each shell command touches a marker in the workspace's `marks` folder.
The gate sends each command as Octet does, in opencode's `build` agent with
Octet's launch environment, so opencode runs the shell first, and cancels
each turn as soon as it shows life, before the model does anything. It
passes only if the markers left are exactly those Octet's emulation
(`octet_engine::live::opencode_command_expansion`) predicts, and opencode
lists its built-in commands with the descriptions Octet's check knows (a
drift signal). The evidence's `expansion` holds the version opencode
reported, whether Octet lists it as verified, and the predicted and actual
markers. No model output is involved, so the result is deterministic.

### Verifying a new opencode version

Octet's command check holds only for the opencode versions in
`VERIFIED_VERSIONS` (`opencode/commands.rs`); on any other, Octet runs no
command or skill in `ask` and `accept-edits`. When opencode updates:

1. Read the new version's `Command.state`, `SessionPrompt.command` and
   `ACP.prompt` (and how often it lists commands) against `commands.rs` and
   `slash_command` in `opencode.rs`; change the emulation where they differ.
2. Run `command-expansion`, `lockdown` and `subagent` against it, and the
   offline tests (`make rust-check`). All must pass.
3. Add the version to `VERIFIED_VERSIONS`, with the gate evidence in the
   commit message.

## Result

The evidence goes to stdout as one JSON object; its `status` starts with
`passed` when the vendor behaved as the drivers expect. For opencode it
also lists the sessions the run opened (`sessions`), any still in
opencode's history after the cleanup (`sessions_left`; a run that leaves one
fails), each prompt's `stopReason` (`stop_reasons`) and the updates a resume
replayed before its first prompt (`replayed_updates`).

| Exit | Meaning |
| --- | --- |
| 0 | The scenario passed |
| 1 | The vendor did something else; the evidence says what |
| 2 | A usage error, or the fixture workspace could not be set up |

## Offline tests

The gate's own logic is tested without a vendor: `crates/octet-gate/tests`
runs the gate binary against scripted wire transcripts (`protocol_gate.rs`)
and checks the contract fixtures (`contracts.rs`). These run in
`make rust-check`; the live scenarios above do not.
