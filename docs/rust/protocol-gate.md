# The protocol gate

`protocol-gate` runs one scenario against a real vendor CLI (`claude`,
`codex`, `opencode` or Antigravity's `agy`) and prints what happened as JSON evidence. It is how
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

Antigravity scenarios run on `gemini-3.8-flash` at thinking level `low`, in
Antigravity's mode `default` (Octet's `ask`). Each run makes a second
temporary folder, owner-only, beside the workspace: `octet-data/` (Octet's
data directory) and `gemini-home/`. The gate prepares the launch as Octet
does (`antigravity_prepare`, reading "the user's" settings from
`gemini-home`, never from `~/.gemini`), then starts `agy` with Octet's
arguments (`launch_args`: `acp --cli_dir_name ../…/<octet-data>/antigravity/review`,
32 `../` then the folder's absolute path without its leading `/`) plus
`--gemini-dir <gemini-home>`, so no run adds a session to your Antigravity
history. `agy` takes `--cli_dir_name` relative to its app data folder
(`<gemini-home>/antigravity-cli`): an absolute name would land inside
`gemini-home`. Before launching, the gate resolves the name as `agy` does
(joined under `<gemini-home>/antigravity-cli` with any leading `/` removed,
`.` and `..` folded by name) and refuses to launch unless it lands in a
settings folder under `octet-data` (never the full-access one), with
exactly that `--gemini-dir`. The folder is removed
when the run ends, whatever its outcome. The gate does not run
`agy --version`: the evidence's `version` is the `agentInfo.version` that
`initialize` reports.

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

- **`--binary`:** the CLI to run; by default the provider's own command on
  `PATH` (`claude`, `codex`, `opencode`, `agy`; opencode installs to
  `~/.opencode/bin`).
- **`--workdir`:** a fixture workspace to create, and keep; by default a
  temporary one, removed when the run ends, whatever its outcome. It must
  not exist yet.
- **`--output`:** also write the evidence to this file.
- **`--timeout`:** 1–300 seconds for the scenario; 90 by default, except
  Antigravity's `lockdown`, `resume` and `compact` (180) and `subagent`
  (240).

With `OCTET_GATE_TRACE` set, an opencode or Antigravity run also prints each
permission request and tool call update it sees to stderr (each cut to 2,000
characters).

## Scenarios

| Scenario | Checks | Claude | Codex | opencode | Antigravity |
| --- | --- | --- | --- | --- | --- |
| `initialize` | The handshake | yes | yes | yes | yes |
| `simple` | One turn to completion | yes | yes | yes | yes |
| `interrupt` | Cancelling a running turn | yes | yes | yes | yes |
| `approval-allow` | An approval allowed; the fixture file is written | yes | yes | yes | yes |
| `approval-deny` | An approval denied | yes | yes | yes | yes |
| `approval-interrupt` | Cancelling while an approval waits | yes | yes | no | yes |
| `resume` | Reopening a session | yes | yes | yes | yes |
| `fork` | Forking a session | yes | yes | yes | no |
| `compact` | Compacting a session's context | yes | yes | no | yes |
| `mcp` | An MCP tool call | yes | no | no | no |
| `hook` | A hook callback | yes | no | no | no |
| `user-input` | A request for user input | no | yes | no | no |
| `tool-only` | A turn that only runs a tool | no | yes | no | no |
| `failure` | A turn the vendor fails | no | yes | no | no |
| `lockdown` | Configuration that would allow actions cannot bypass `ask` | no | no | yes | yes |
| `subagent` | A subagent cannot bypass `ask` | no | no | yes | yes |
| `command-expansion` | Octet's emulation of how opencode expands a command | no | no | yes | no |

opencode's `resume` and `fork` reopen the session in a second process, as
`--resume` does, and ask which word it replied with: the reopened session,
or its fork, must remember. opencode's `lockdown` runs in a workspace whose
`opencode.json` would allow everything, Octet's `octet-ask` agent included
(`{"permission":"allow","agent":{"octet-ask":{"permission":{"*":"allow"}}}}`),
with `OPENCODE_CONFIG` naming a user file that allows the command
(`{"permission":{"bash":{"echo *":"allow"}}}`). It asks for `echo fixture`
and passes only if opencode asks permission first for exactly that: an
`execute` call whose command (or title) is `echo fixture`. Any other
permission request is rejected and proves nothing; a command that runs
unasked fails it. The gate rejects every request, as Octet does when a
request has no one to answer it.

opencode's `subagent` runs in the same workspace and asks the model, in `octet-ask`, to
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

### Antigravity

An Antigravity scenario ends `passed`, `failed: <why>` or `unproven: <why>`.
`unproven` means the run did not exercise what the scenario claims to prove
(the model did not attempt the action, say); it is not a pass, and the gate
exits 1 for it. In every Antigravity scenario a tool call that edits or runs
something (kind `execute`, `edit`, `delete` or `move`) and completes without
a permission request fails the run, and any request the scenario does not
handle gets Octet's own reply (`antigravity_stray_reply`: a permission
request is rejected). Every scenario sets the model and the thinking level
right after opening the session, and the mode to `default` unless the
session already reports it; a refusal, or a reply that does not confirm the
value, fails the run with what Antigravity said.

- **`simple`** asks for `READY` and passes if the reply is that word and the
  prompt ends `end_turn`.
- **`interrupt`** sends `session/cancel` at the first reply text of a long
  count; the prompt must end `cancelled` before reaching its end.
- **`approval-*`** ask for `printf READY > probe.out`. Only a request for
  that command (an `execute` call whose `rawInput.CommandLine`, or title, is
  the command, run in the workspace) counts; without one the run is
  `unproven`. `approval-allow` allows it with the request's `allow_once`
  option and the file must then hold `READY`; `approval-deny` rejects it and
  the file must not exist; `approval-interrupt` sends `session/cancel` while
  the request waits, then rejects it, as Octet does, and the prompt must end
  `cancelled` with no file written.
- **`resume`** has the model reply with a word, stops the process, and
  resumes the session (`session/resume`) in a second one, in the same
  scratch home. The evidence records the model, mode and thinking level the
  resume's reply reports, before the gate sets them again; the next turn
  must recall the word.
- **`compact`** has the model reply with a word, then sends `/compact` as a
  prompt only if the session lists a `compact` command (otherwise
  `unproven`); it must end `end_turn`, and a further turn must recall the
  word.
- **`lockdown`** writes `gemini-home/antigravity-cli/settings.json` (where
  Octet reads the user's settings) with every way of skipping a request:
  `{"toolPermission":"always-proceed","agentMode":"accept-edits",
  "permissions":{"allow":["command(*)","write_file(*)"]},"octetGateMarker":true}`.
  Before launching, it reads the settings Octet wrote
  (`octet-data/antigravity/review/settings.json`): the run is `unproven`
  unless they hold the marker (the fixture went through Octet's merge),
  `toolPermission` `request-review`, no `agentMode` and no
  `permissions.allow`. Once the session is open it is `unproven` too unless
  `agy` made its `log` folder in Octet's settings folder: otherwise that
  was not the folder `agy` used. In mode `default` it then asks for
  `echo fixture` and, in a second turn, for a file `gate.txt`. It passes only if each brought a
  permission request (which the gate rejects) and `gate.txt` does not exist;
  a missing request leaves it `unproven`.
- **`subagent`** asks the model to delegate `touch gate-sub.txt` with
  `invoke_subagent`. It is `unproven` unless an `invoke_subagent` tool call
  completed. After 15 s, a second turn asks for the subagent's state with
  `manage_subagents`: unless that call completed and its output or the reply
  reports the subagent waiting for input, the run is `unproven`. Then it
  watches for 30 s and passes only if `gate-sub.txt` does not exist and no
  `execute` tool call completed on the connection. The evidence records what
  `manage_subagents` reported.

The evidence's `antigravity` object holds the version and whether Octet
lists it as verified (`ANTIGRAVITY_VERIFIED_VERSIONS`), the launch (the
scratch folder shown as `<scratch>`), the key names of the settings Octet
wrote and their `toolPermission`, whether `agy` made its `log` folder in
Octet's settings folder by the end of the run
(`agy_log_in_settings_folder`), the options confirmed and those a resume
reported, each permission request (its tool, kind, title and the answer)
and tool call (its kind, statuses, whether it asked, and its `rawInput`'s
key names), each reply text, the session's commands, and the scratch
folder's layout when the run ended (16 levels, 80 entries). Nothing of the
account is recorded: not `authMethods`, not the tier, and any word with an
`@` in vendor text is replaced.

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
replayed before its first prompt (`replayed_updates`). For Antigravity a
`status` may also start with `unproven`, and the evidence adds `sessions`,
`stop_reasons` and the `antigravity` object above.

| Exit | Meaning |
| --- | --- |
| 0 | The scenario passed |
| 1 | The vendor did something else, or (Antigravity) the run was `unproven`; the evidence says what |
| 2 | A usage error, or the fixture workspace could not be set up |

## Offline tests

The gate's own logic is tested without a vendor: `crates/octet-gate/tests`
runs the gate binary against scripted wire transcripts (`protocol_gate.rs`)
and checks the contract fixtures (`contracts.rs`). These run in
`make rust-check`; the live scenarios above do not.
