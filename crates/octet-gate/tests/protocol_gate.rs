//! Adversarial wire transcripts exercise the actual gate executable offline.
#![cfg(unix)]
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]
use serde_json::{Value, json};
use std::process::Command;

/// A probe expected to fail waits out its whole budget, so that budget is short.
fn probe_engine(engine: &str, scenario: &str, frames: &[Value]) -> Value {
    probe_engine_within(engine, scenario, frames, 2)
}
/// A probe expected to pass returns as soon as the terminal event arrives, so it
/// can afford a budget that survives a slow process launch on a busy machine.
fn passing_probe_engine(engine: &str, scenario: &str, frames: &[Value]) -> Value {
    probe_engine_within(engine, scenario, frames, 20)
}
fn probe_engine_within(engine: &str, scenario: &str, frames: &[Value], seconds: u64) -> Value {
    let root = octet_testkit::TempDir::new("octet-gate-test");
    let wire = frames
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let executable = octet_testkit::write_script(
        root.path(),
        "vendor",
        &format!(
            "if [ \"$1\" = --version ]; then echo fixture; exit; fi\ncat <<'WIRE'\n{wire}\nWIRE\ncat >/dev/null"
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_protocol-gate"))
        .args(["--engine", engine, "--scenario", scenario, "--binary"])
        .arg(&executable)
        .args(["--timeout", &seconds.to_string()])
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("gate output: {output:?}"))
}
fn probe(scenario: &str, events: Vec<Value>) -> Value {
    probe_engine("codex", scenario, &codex_frames(events))
}
fn passing_probe(scenario: &str, events: Vec<Value>) -> Value {
    passing_probe_engine("codex", scenario, &codex_frames(events))
}
fn codex_frames(mut events: Vec<Value>) -> Vec<Value> {
    let mut frames = vec![
        json!({"id":1,"result":{}}),
        json!({"id":2,"result":{"thread":{"id":"t"},"model":"fixture"}}),
        json!({"id":3,"result":{"turn":{"id":"u"}}}),
        json!({"method":"turn/started","params":{"threadId":"t","turn":{"id":"u"}}}),
    ];
    frames.append(&mut events);
    frames
}
fn terminal(thread: &str, turn: &Value, status: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":status}}})
}
#[test]
fn failure_probe_rejects_success_and_uncorrelated_terminals() {
    for frame in [
        terminal("t", &json!("u"), "completed"),
        terminal("other", &json!("u"), "failed"),
        terminal("t", &Value::Null, "failed"),
    ] {
        let evidence = probe("failure", vec![frame]);
        assert!(
            evidence["status"].as_str().unwrap().starts_with("failed:"),
            "{evidence}"
        );
        assert_eq!(evidence["child_cleanup"], true);
    }
    assert_eq!(
        passing_probe("failure", vec![terminal("t", &json!("u"), "failed")])["status"],
        "passed"
    );
}
#[test]
fn interrupt_terminal_can_precede_acknowledgement() {
    let evidence = passing_probe(
        "interrupt",
        vec![
            terminal("t", &json!("u"), "interrupted"),
            json!({"id":4,"result":{}}),
        ],
    );
    assert_eq!(evidence["status"], "passed", "{evidence}");
    assert_eq!(evidence["child_cleanup"], true);
}

#[test]
fn simple_probe_requires_expected_reply() {
    let done = terminal("t", &json!("u"), "completed");
    assert!(
        probe("simple", vec![done.clone()])["status"]
            .as_str()
            .unwrap()
            .starts_with("failed:")
    );
    let answer = json!({"method":"item/completed","params":{"threadId":"t","turnId":"u","item":{"type":"agentMessage","text":"READY"}}});
    assert_eq!(
        passing_probe("simple", vec![answer, done])["status"],
        "passed"
    );
}

#[test]
fn claude_checks_result_and_accepts_terminal_before_interrupt_ack() {
    let init =
        json!({"type":"control_response","response":{"request_id":"init-1","subtype":"success"}});
    let done = json!({"type":"result","is_error":false,"result":"READY"});
    let ack = json!({"type":"control_response","response":{"request_id":"interrupt-2","subtype":"success"}});
    assert_eq!(
        passing_probe_engine("claude", "interrupt", &[init.clone(), done.clone(), ack])["status"],
        "passed"
    );
    assert_eq!(
        passing_probe_engine("claude", "simple", &[init.clone(), done])["status"],
        "passed"
    );
    assert!(
        probe_engine(
            "claude",
            "simple",
            &[init, json!({"type":"result","result":"READY"})]
        )["status"]
            .as_str()
            .unwrap()
            .starts_with("failed:")
    );
}

/// How long a `probe_in` run may take. Its scripts end the scenario
/// themselves (a pass, or a failure the gate detects on the wire), so the
/// budget is only a backstop: generous, so a slow launch on a loaded machine
/// is never reported as "deadline exceeded" instead of the real outcome.
const PROBE_IN_SECONDS: &str = "60";

/// Runs the gate against `frames` with its temporary workspace under `tmp`;
/// the gate's exit code and its parsed evidence, if it printed any.
fn probe_in(
    tmp: &std::path::Path,
    engine: &str,
    scenario: &str,
    body: &str,
) -> (Option<i32>, Option<Value>) {
    let executable = octet_testkit::write_script(tmp, "vendor", body);
    let output = Command::new(env!("CARGO_BIN_EXE_protocol-gate"))
        .args(["--engine", engine, "--scenario", scenario, "--binary"])
        .arg(&executable)
        .args(["--timeout", PROBE_IN_SECONDS])
        .env("TMPDIR", tmp)
        .output()
        .unwrap();
    (
        output.status.code(),
        serde_json::from_slice(&output.stdout).ok(),
    )
}

fn wire_script(frames: &[Value]) -> String {
    let wire = frames
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "if [ \"$1\" = --version ]; then echo fixture; exit; fi\n\
         for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > \"$(dirname \"$0\")/argv\"\n\
         cat <<'WIRE'\n{wire}\nWIRE\ncat >/dev/null"
    )
}

#[test]
fn the_gate_launches_claude_as_octet_does() {
    let root = octet_testkit::TempDir::new("octet-gate-argv");
    std::fs::create_dir_all(root.path()).unwrap();
    let init =
        json!({"type":"control_response","response":{"request_id":"init-1","subtype":"success"}});
    let done = json!({"type":"result","is_error":false,"result":"READY"});
    let (_, evidence) = probe_in(root.path(), "claude", "simple", &wire_script(&[init, done]));
    assert_eq!(evidence.unwrap()["status"], "passed");
    let argv = std::fs::read_to_string(root.path().join("argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    let config =
        octet_engine::live::Config::new(octet_engine::live::Engine::CLAUDE, "claude", root.path());
    let octet: Vec<String> = octet_engine::live::launch_args(&config)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(
        argv.starts_with(&octet.iter().map(String::as_str).collect::<Vec<_>>()),
        "gate {argv:?}\noctet {octet:?}"
    );
}

#[test]
fn an_approval_without_an_id_is_a_protocol_failure() {
    let root = octet_testkit::TempDir::new("octet-gate-no-id");
    std::fs::create_dir_all(root.path()).unwrap();
    let request = json!({"method":"item/commandExecution/requestApproval","params":{"command":"other","cwd":"/elsewhere"}});
    let (code, evidence) = probe_in(
        root.path(),
        "codex",
        "approval-interrupt",
        &wire_script(&codex_frames(vec![request])),
    );
    assert_eq!(code, Some(1), "a failed scenario, not a panic");
    let evidence = evidence.expect("evidence is printed");
    assert!(
        evidence["status"]
            .as_str()
            .unwrap()
            .starts_with("failed: protocol"),
        "{evidence}"
    );
}

#[test]
fn the_workspace_is_removed_on_failure() {
    let root = octet_testkit::TempDir::new("octet-gate-cleanup");
    std::fs::create_dir_all(root.path()).unwrap();
    let request = json!({"method":"item/commandExecution/requestApproval","params":{"command":"other","cwd":"/elsewhere"}});
    let _ = probe_in(
        root.path(),
        "codex",
        "approval-interrupt",
        &wire_script(&codex_frames(vec![request])),
    );
    let left: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("octet-protocol-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A scripted `opencode`: `acp` writes `frames`, then keeps what the gate
/// sent in `stdin`; `session delete ID` appends ID to `deleted`, and
/// `session list` prints `listed`; `debug config` shows the gate command as
/// lockdown's user rule defines it, `debug skill` no skills.
fn opencode_script(frames: &[Value], listed: &str) -> String {
    opencode_script_with(frames, listed, &gate_command(Some("build")))
}

/// As `opencode_script`, with `debug config` printing `config`.
fn opencode_script_with(frames: &[Value], listed: &str, config: &Value) -> String {
    let wire = frames
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "dir=\"$(dirname \"$0\")\"\n\
         case \"$1\" in\n\
         --version) echo 1.18.25; exit ;;\n\
         session) if [ \"$2\" = delete ]; then echo \"$3\" >> \"$dir/deleted\"; else echo '{listed}'; fi; exit ;;\n\
         debug) if [ \"$2\" = config ]; then echo '{config}'; else echo '[]'; fi; exit ;;\n\
         esac\n\
         cat <<'WIRE'\n{wire}\nWIRE\ncat > \"$dir/stdin\""
    )
}

/// opencode's reply `id` confirming option `option` at `value`.
fn opencode_confirm(id: u64, option: &str, value: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":{"configOptions":[{"id":option,"currentValue":value}]}})
}

/// opencode's handshake for session `ses_a` (the free model and Octet's ask
/// agent confirmed, then its command list naming the gate command), then
/// `events` and the prompt's `end_turn` reply.
fn opencode_frames(events: Vec<Value>) -> Vec<Value> {
    opencode_frames_listing("Octet gate command", events)
}

/// As `opencode_frames`, the gate command listed with `description`.
fn opencode_frames_listing(description: &str, mut events: Vec<Value>) -> Vec<Value> {
    let mut frames = vec![
        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}),
        json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"ses_a"}}),
        opencode_confirm(3, "model", "opencode/deepseek-v4.1-flash-free"),
        opencode_confirm(4, "mode", "octet-ask"),
        opencode_update(
            &json!({"sessionUpdate":"available_commands_update","availableCommands":[
            {"name":"octet-gate-command","description":description}]}),
        ),
    ];
    frames.append(&mut events);
    frames.push(json!({"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn"}}));
    frames
}

/// A `session/update` notification for session `ses_a` carrying `update`.
fn opencode_update(update: &Value) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_a","update":update}})
}

#[test]
fn an_opencode_run_deletes_its_session_afterwards() {
    let root = octet_testkit::TempDir::new("octet-gate-opencode");
    std::fs::create_dir_all(root.path()).unwrap();
    let reply = opencode_update(
        &json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"READY"}}),
    );
    let script = opencode_script(&opencode_frames(vec![reply]), "[]");
    let (code, evidence) = probe_in(root.path(), "opencode", "simple", &script);
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(0), "{evidence}");
    assert_eq!(evidence["status"], "passed");
    assert_eq!(evidence["sessions"], json!(["ses_a"]));
    assert_eq!(evidence["sessions_left"], json!([]));
    assert_eq!(evidence["stop_reasons"], json!(["end_turn"]));
    let deleted = std::fs::read_to_string(root.path().join("deleted")).unwrap();
    assert_eq!(deleted, "ses_a\n");
    // Every frame the gate sent is JSON-RPC 2.0, with the free model and
    // Octet's ask agent set before the prompt.
    let sent = std::fs::read_to_string(root.path().join("stdin")).unwrap();
    let sent: Vec<Value> = sent
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        sent.iter().all(|frame| frame["jsonrpc"] == "2.0"),
        "{sent:?}"
    );
    let methods: Vec<&str> = sent.iter().filter_map(|f| f["method"].as_str()).collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    assert_eq!(
        sent[2]["params"]["value"],
        "opencode/deepseek-v4.1-flash-free"
    );
    assert_eq!(sent[3]["params"]["value"], "octet-ask");
}

#[test]
fn an_opencode_option_set_without_its_confirmation_fails_the_run() {
    let root = octet_testkit::TempDir::new("octet-gate-opencode-unconfirmed");
    std::fs::create_dir_all(root.path()).unwrap();
    let reply = opencode_update(
        &json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"READY"}}),
    );
    // opencode answers the agent switch without saying which agent is set.
    let mut frames = opencode_frames(vec![reply]);
    frames[3] = json!({"jsonrpc":"2.0","id":4,"result":{}});
    let script = opencode_script(&frames, "[]");
    let (code, evidence) = probe_in(root.path(), "opencode", "simple", &script);
    let evidence = evidence.unwrap();
    assert_ne!(code, Some(0), "{evidence}");
    assert_eq!(
        evidence["status"], "failed: protocol: set_config_option not confirmed",
        "{evidence}"
    );
}

#[test]
fn an_opencode_session_still_listed_fails_the_run() {
    let root = octet_testkit::TempDir::new("octet-gate-opencode-left");
    std::fs::create_dir_all(root.path()).unwrap();
    let reply = opencode_update(
        &json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"READY"}}),
    );
    let script = opencode_script(&opencode_frames(vec![reply]), r#"[{"id":"ses_a"}]"#);
    let (code, evidence) = probe_in(root.path(), "opencode", "simple", &script);
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(1));
    assert_eq!(
        evidence["status"],
        "failed: sessions left in opencode history"
    );
    assert_eq!(evidence["sessions_left"], json!(["ses_a"]));
}

#[test]
fn lockdown_passes_only_when_opencode_asks() {
    let call = json!({"toolCallId":"c1","title":"echo fixture","kind":"execute","rawInput":{"command":"echo fixture"}});
    let mut ran = call.clone();
    ran["sessionUpdate"] = json!("tool_call");
    let mut done =
        json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed"});
    // The command ran without a permission request: a bypass.
    let root = octet_testkit::TempDir::new("octet-gate-lockdown-bypass");
    std::fs::create_dir_all(root.path()).unwrap();
    let frames = opencode_frames(vec![opencode_update(&ran), opencode_update(&done)]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "lockdown",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(1));
    assert_eq!(
        evidence["status"],
        "failed: protocol: a tool changed something without a permission request"
    );
    // opencode asked: the gate rejects, as Octet's stray reply does.
    let root = octet_testkit::TempDir::new("octet-gate-lockdown-asks");
    std::fs::create_dir_all(root.path()).unwrap();
    let options = json!([
        {"optionId":"once","kind":"allow_once"},
        {"optionId":"always","kind":"allow_always"},
        {"optionId":"reject","kind":"reject_once"},
    ]);
    let ask = json!({"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"ses_a","toolCall":call,"options":options}});
    done["status"] = json!("failed");
    let frames = opencode_frames(vec![opencode_update(&ran), ask, opencode_update(&done)]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "lockdown",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(0), "{evidence}");
    assert_eq!(evidence["approval_requests"], 1);
    let sent = std::fs::read_to_string(root.path().join("stdin")).unwrap();
    let answer: Value = sent
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|frame| frame["id"] == 0 && frame.get("method").is_none())
        .unwrap();
    assert_eq!(
        answer,
        json!({"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"reject"}}})
    );
}

/// opencode's `reject_once`-style options, as 1.18.25 sends them.
fn opencode_options() -> Value {
    json!([
        {"optionId":"once","kind":"allow_once"},
        {"optionId":"always","kind":"allow_always"},
        {"optionId":"reject","kind":"reject_once"},
    ])
}

/// A permission request, numbered `id`, for `call` in session `ses_a`.
fn opencode_ask(id: u64, call: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":"ses_a","toolCall":call,"options":opencode_options()}})
}

/// The gate's answer to the request numbered `id`, from what it sent.
fn sent_answer(root: &std::path::Path, id: u64) -> Value {
    let sent = std::fs::read_to_string(root.join("stdin")).unwrap();
    sent.lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|frame| frame["id"] == id && frame.get("method").is_none())
        .unwrap()
}

#[test]
fn lockdown_counts_only_the_fixture_command() {
    // A request for anything but `echo fixture` (another command, or an edit
    // titled like it) is rejected and proves nothing.
    for (n, call) in [
        json!({"toolCallId":"c1","title":"rm -rf build","kind":"execute","rawInput":{"command":"rm -rf build"}}),
        json!({"toolCallId":"c1","title":"echo fixture","kind":"edit","rawInput":{}}),
        json!({"toolCallId":"c1","title":"echo fixture; rm -rf ~","kind":"execute","rawInput":{"command":"echo fixture; rm -rf ~"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let root = octet_testkit::TempDir::new(&format!("octet-gate-lockdown-other-{n}"));
        std::fs::create_dir_all(root.path()).unwrap();
        let failed =
            json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed"});
        let frames = opencode_frames(vec![opencode_ask(0, &call), opencode_update(&failed)]);
        let (code, evidence) = probe_in(
            root.path(),
            "opencode",
            "lockdown",
            &opencode_script(&frames, "[]"),
        );
        let evidence = evidence.unwrap();
        assert_eq!(code, Some(1), "{call}: {evidence}");
        assert_eq!(evidence["approval_requests"], 0, "{call}");
        assert_eq!(
            evidence["status"],
            "failed: protocol: the model ran no command, so lockdown is unproven",
            "{call}"
        );
        assert_eq!(
            sent_answer(root.path(), 0)["result"]["outcome"]["optionId"],
            "reject",
            "{call}"
        );
    }
    // The title alone names the command when opencode sends no input yet.
    let root = octet_testkit::TempDir::new("octet-gate-lockdown-title");
    std::fs::create_dir_all(root.path()).unwrap();
    let call = json!({"toolCallId":"c1","title":"echo fixture","kind":"execute","rawInput":{}});
    let frames = opencode_frames(vec![opencode_ask(0, &call)]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "lockdown",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(0), "{evidence}");
    assert_eq!(evidence["approval_requests"], 1);
}

#[test]
fn subagent_fails_when_a_subagent_runs() {
    // opencode's task tool: kind other, titled `task`, with a subagent type.
    let task = json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"task","kind":"other","status":"pending","rawInput":{"description":"run it","prompt":"run echo fixture","subagent_type":"general"}});
    let done = json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"run it","kind":"other","status":"completed"});
    let root = octet_testkit::TempDir::new("octet-gate-subagent-ran");
    std::fs::create_dir_all(root.path()).unwrap();
    let frames = opencode_frames(vec![opencode_update(&task), opencode_update(&done)]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "subagent",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(1), "{evidence}");
    assert_eq!(
        evidence["status"],
        "failed: protocol: a subagent ran outside Octet's permission requests"
    );
    // Asked to start one, the gate allows it once, as a user might; the
    // subagent that then runs still fails the run.
    let root = octet_testkit::TempDir::new("octet-gate-subagent-asked");
    std::fs::create_dir_all(root.path()).unwrap();
    let asked = opencode_ask(
        0,
        &json!({"toolCallId":"t1","title":"task","kind":"think","rawInput":{"subagent_type":"general"}}),
    );
    let frames = opencode_frames(vec![opencode_update(&task), asked, opencode_update(&done)]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "subagent",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(1), "{evidence}");
    assert_eq!(evidence["approval_requests"], 0);
    assert_eq!(
        sent_answer(root.path(), 0),
        json!({"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"once"}}})
    );
    assert_eq!(
        evidence["status"],
        "failed: protocol: a subagent ran outside Octet's permission requests"
    );
    // A denied task tool, and a command the model then asked for itself and
    // was refused: nothing ran unasked.
    let root = octet_testkit::TempDir::new("octet-gate-subagent-denied");
    std::fs::create_dir_all(root.path()).unwrap();
    let mut refused = done.clone();
    refused["status"] = json!("failed");
    let call = json!({"toolCallId":"c1","title":"echo fixture","kind":"execute","rawInput":{"command":"echo fixture"}});
    let mut ran = call.clone();
    ran["sessionUpdate"] = json!("tool_call");
    let failed = json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed"});
    let frames = opencode_frames(vec![
        opencode_update(&task),
        opencode_update(&refused),
        opencode_update(&ran),
        opencode_ask(0, &call),
        opencode_update(&failed),
    ]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "subagent",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(0), "{evidence}");
    assert_eq!(
        sent_answer(root.path(), 0)["result"]["outcome"]["optionId"],
        "reject"
    );
    // A command that ran unasked fails it, as in lockdown.
    let root = octet_testkit::TempDir::new("octet-gate-subagent-bypass");
    std::fs::create_dir_all(root.path()).unwrap();
    let completed =
        json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed"});
    let frames = opencode_frames(vec![opencode_update(&ran), opencode_update(&completed)]);
    let (code, evidence) = probe_in(
        root.path(),
        "opencode",
        "subagent",
        &opencode_script(&frames, "[]"),
    );
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(1), "{evidence}");
    assert_eq!(
        evidence["status"],
        "failed: protocol: a tool changed something without a permission request"
    );
    // Its prompt asks for a subagent, in octet-ask, with lockdown's rules.
    let sent: Vec<Value> = std::fs::read_to_string(root.path().join("stdin"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let prompt = sent
        .iter()
        .find(|frame| frame["method"] == "session/prompt")
        .unwrap();
    let text = prompt["params"]["prompt"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("subagent") && text.contains("echo fixture"),
        "{text}"
    );
}

#[test]
fn each_engine_runs_only_its_own_scenarios() {
    let refused = |engine: &str, scenario: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_protocol-gate"))
            .args([
                "--engine",
                engine,
                "--scenario",
                scenario,
                "--binary",
                "/nonexistent",
            ])
            .output()
            .unwrap();
        output.status.code() == Some(2)
            && String::from_utf8_lossy(&output.stderr).contains("unsupported by selected engine")
    };
    for scenario in [
        "mcp",
        "hook",
        "compact",
        "approval-interrupt",
        "user-input",
        "failure",
    ] {
        assert!(refused("opencode", scenario), "{scenario}");
    }
    for scenario in ["lockdown", "subagent", "command-expansion"] {
        assert!(refused("codex", scenario), "{scenario}");
        assert!(refused("claude", scenario), "{scenario}");
    }
}

/// A scripted `opencode` for `lockdown`: the gate command listed with
/// `description`, a permission request for `echo fixture`, and `debug
/// config` printing `config`.
fn opencode_lockdown_script(description: &str, config: &Value) -> String {
    let permission = json!({"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"ses_a",
        "toolCall":{"toolCallId":"c1","title":"echo fixture","kind":"execute","rawInput":{"command":"echo fixture"}},
        "options":[{"optionId":"once","kind":"allow_once","name":"Allow once"}]}});
    opencode_script_with(
        &opencode_frames_listing(description, vec![permission]),
        "[]",
        config,
    )
}

/// The `command` entry `opencode debug config` shows for the gate command.
fn gate_command(agent: Option<&str>) -> Value {
    let mut command =
        json!({"description":"Octet gate command","subtask":true,"template":"Gate !`echo gate`"});
    if let Some(agent) = agent {
        command["agent"] = json!(agent);
    }
    json!({"command":{"octet-gate-command":command}})
}

#[test]
fn opencode_lockdown_checks_what_octet_reads_of_a_command() {
    // Octet checks a command against `opencode debug config` and the list's
    // description before it sends it while the mode asks.
    let root = octet_testkit::TempDir::new("octet-gate-lockdown-command");
    std::fs::create_dir_all(root.path()).unwrap();
    let script = opencode_lockdown_script("Octet gate command", &gate_command(Some("build")));
    let (code, evidence) = probe_in(root.path(), "opencode", "lockdown", &script);
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(0), "{evidence}");
    assert_eq!(evidence["status"], "passed");
    for (description, config, why) in [
        (
            "Octet gate command",
            gate_command(None),
            "opencode debug config does not show a command's agent",
        ),
        (
            "Another",
            gate_command(Some("build")),
            "does not list the gate command",
        ),
    ] {
        let script = opencode_lockdown_script(description, &config);
        let (code, evidence) = probe_in(root.path(), "opencode", "lockdown", &script);
        let evidence = evidence.unwrap();
        assert_eq!(code, Some(1), "{evidence}");
        assert!(
            evidence["status"].as_str().unwrap().contains(why),
            "{evidence}"
        );
    }
}

/// The markers `command-expansion`'s commands leave when opencode expands
/// them as Octet's emulation predicts.
const EXPANSION_MARKERS: [&str; 5] = ["append", "bang", "quote", "tail", "template"];

/// A scripted `opencode` for `command-expansion`: it leaves `marks` in the
/// workspace's `marks` folder (as the real one's shell would), lists its
/// built-ins with `init` described as `init`, and the gate's commands, then
/// ends each of the six prompts.
fn opencode_expansion_script(marks: &[&str], init: &str) -> String {
    let mut commands = vec![
        json!({"name":"init","description":init}),
        json!({"name":"review","description":"review changes [commit|branch|pr], defaults to uncommitted"}),
    ];
    for name in ["append", "bang", "quote", "slice", "tail", "template"] {
        commands.push(json!({"name":format!("octet-gate-{name}"),"description":"Octet gate"}));
    }
    let mut frames = vec![
        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"OpenCode","version":"1.18.25"}}}),
        json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"ses_a"}}),
        opencode_update(
            &json!({"sessionUpdate":"available_commands_update","availableCommands":commands}),
        ),
        opencode_confirm(3, "model", "opencode/deepseek-v4.1-flash-free"),
        opencode_confirm(4, "mode", "build"),
    ];
    for id in 5..11 {
        frames.push(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
    }
    let wire = frames
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let touch: String = marks
        .iter()
        .map(|mark| [" marks/", mark].concat())
        .collect();
    format!(
        "dir=\"$(dirname \"$0\")\"\n\
         case \"$1\" in\n\
         --version) echo 1.18.25; exit ;;\n\
         session) if [ \"$2\" = delete ]; then echo \"$3\" >> \"$dir/deleted\"; else echo '[]'; fi; exit ;;\n\
         esac\n\
         mkdir -p marks; [ -n \"{touch}\" ] && touch{touch}\n\
         cat <<'WIRE'\n{wire}\nWIRE\ncat > \"$dir/stdin\""
    )
}

#[test]
fn command_expansion_passes_when_opencode_runs_the_shell_octet_predicts() {
    let root = octet_testkit::TempDir::new("octet-gate-expansion");
    std::fs::create_dir_all(root.path()).unwrap();
    let script = opencode_expansion_script(&EXPANSION_MARKERS, "guided AGENTS.md setup");
    let (code, evidence) = probe_in(root.path(), "opencode", "command-expansion", &script);
    let evidence = evidence.unwrap();
    assert_eq!(code, Some(0), "{evidence}");
    assert_eq!(evidence["status"], "passed");
    assert_eq!(evidence["expansion"]["version"], "1.18.25");
    assert_eq!(evidence["expansion"]["verified"], true);
    assert_eq!(evidence["expansion"]["markers"], json!(EXPANSION_MARKERS));
    // In auto (opencode's build agent), with Octet's launch environment,
    // each command sent as Octet sends one.
    let sent = std::fs::read_to_string(root.path().join("stdin")).unwrap();
    let sent: Vec<Value> = sent
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        sent.iter().all(|frame| frame["jsonrpc"] == "2.0"),
        "{sent:?}"
    );
    assert_eq!(sent[3]["params"]["value"], "build");
    let prompts: Vec<&str> = sent
        .iter()
        .filter(|frame| frame["method"] == "session/prompt")
        .filter_map(|frame| {
            frame
                .pointer("/params/prompt/0/text")
                .and_then(Value::as_str)
        })
        .collect();
    assert_eq!(prompts.len(), 6, "{prompts:?}");
    assert!(
        prompts.iter().all(|text| text.starts_with("/octet-gate-")),
        "{prompts:?}"
    );
}

#[test]
fn command_expansion_fails_when_opencode_runs_other_shell_or_its_built_ins_drift() {
    let root = octet_testkit::TempDir::new("octet-gate-expansion-drift");
    std::fs::create_dir_all(root.path()).unwrap();
    for (marks, init, why) in [
        (
            &["append", "bang", "quote", "slice", "tail", "template"][..],
            "guided AGENTS.md setup",
            "not as Octet's emulation predicts",
        ),
        (
            &["append", "bang", "quote", "template"][..],
            "guided AGENTS.md setup",
            "not as Octet's emulation predicts",
        ),
        (&EXPANSION_MARKERS[..], "init", "built-in"),
    ] {
        let script = opencode_expansion_script(marks, init);
        let (code, evidence) = probe_in(root.path(), "opencode", "command-expansion", &script);
        let evidence = evidence.unwrap();
        assert_eq!(code, Some(1), "{evidence}");
        assert!(
            evidence["status"].as_str().unwrap().contains(why),
            "{evidence}"
        );
    }
}
