//! `protocol-gate`: runs one scenario against a real vendor CLI and prints
//! the contract evidence as JSON. It launches the vendor exactly as Octet
//! does (`octet_engine::live::launch_args`), adding only what a scenario
//! needs, so it checks the contract Octet ships.
#![forbid(unsafe_code)]
use octet_engine::live::{
    Config, Engine, claude_stray_reply, codex_stray_reply, launch_args, launch_env,
    opencode_stray_reply,
};
use octet_gate::{
    Evidence, GateError, GateProcess, claude_fixture_allow, claude_fixture_hook_response,
    claude_fixture_mcp_response, claude_fixture_mcp_tool_allow, codex_fixture_allow,
    codex_fixture_user_input, opencode_fixture_allow, opencode_is_fixture_request,
    opencode_is_lockdown_request, opencode_task_allow,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

/// Every scenario the gate runs.
const SCENARIOS: [&str; 16] = [
    "initialize",
    "simple",
    "interrupt",
    "approval-deny",
    "approval-interrupt",
    "approval-allow",
    "mcp",
    "resume",
    "fork",
    "compact",
    "hook",
    "user-input",
    "tool-only",
    "failure",
    "lockdown",
    "subagent",
];
/// The scenarios opencode runs; `lockdown` and `subagent` are its alone.
const OPENCODE_SCENARIOS: [&str; 9] = [
    "initialize",
    "simple",
    "interrupt",
    "approval-allow",
    "approval-deny",
    "resume",
    "fork",
    "lockdown",
    "subagent",
];

/// Whether `scenario` runs in `lockdown`'s workspace: a project config and
/// a user rule that would allow the fixture command.
fn locked_down(scenario: &str) -> bool {
    matches!(scenario, "lockdown" | "subagent")
}

struct Opt {
    engine: Engine,
    scenario: String,
    binary: PathBuf,
    workdir: Option<PathBuf>,
    output: Option<PathBuf>,
    timeout: Duration,
}
fn options() -> Result<Opt, &'static str> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut engine = None;
    let mut scenario = None;
    let mut binary = None;
    let mut workdir = None;
    let mut output = None;
    let mut seconds = 90;
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).ok_or("flag requires value")?.clone();
        match args[i].as_str() {
            "--engine" => engine = Some(val),
            "--scenario" => scenario = Some(val),
            "--binary" => binary = Some(PathBuf::from(val)),
            "--workdir" => workdir = Some(PathBuf::from(val)),
            "--output" => output = Some(PathBuf::from(val)),
            "--timeout" => seconds = val.parse().map_err(|_| "invalid timeout")?,
            _ => return Err("unknown flag"),
        }
        i += 2;
    }
    let name = engine.ok_or("missing --engine")?;
    let engine = Engine::parse(&name)
        .filter(|engine| engine.is_vendor())
        .ok_or("invalid engine")?;
    let scenario = scenario.ok_or("missing --scenario")?;
    if !SCENARIOS.contains(&scenario.as_str()) {
        return Err("unsupported scenario");
    }
    if !(1..=300).contains(&seconds) {
        return Err("timeout outside 1..300 seconds");
    }
    let codex_only = ["user-input", "tool-only", "failure"];
    let claude_only = ["mcp", "hook"];
    let supported = if engine == Engine::OPENCODE {
        OPENCODE_SCENARIOS.contains(&scenario.as_str())
    } else if engine == Engine::CODEX {
        !claude_only.contains(&scenario.as_str()) && !locked_down(&scenario)
    } else {
        !codex_only.contains(&scenario.as_str()) && !locked_down(&scenario)
    };
    if !supported {
        return Err("scenario is unsupported by selected engine");
    }
    let binary = binary.unwrap_or_else(|| PathBuf::from(&name));
    Ok(Opt {
        engine,
        scenario,
        binary,
        workdir,
        output,
        timeout: Duration::from_secs(seconds),
    })
}

/// The scenario's workspace. A temporary one is removed when this drops, on
/// every exit path; a `--workdir` is kept for inspection.
struct Workspace {
    path: PathBuf,
    temporary: bool,
}
impl Workspace {
    fn create(workdir: Option<PathBuf>) -> Result<Self, &'static str> {
        let temporary = workdir.is_none();
        let path = workdir.unwrap_or_else(|| {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            env::temp_dir().join(format!("octet-protocol-{}-{nonce}", std::process::id()))
        });
        if !temporary && path.exists() {
            return Err("--workdir must be a new fixture directory");
        }
        fs::create_dir_all(&path).map_err(|_| "cannot create fixture workspace")?;
        let workspace = Self { path, temporary };
        fs::write(
            workspace.path.join("fixture.txt"),
            b"Octet protocol fixture\n",
        )
        .map_err(|_| "cannot write fixture")?;
        Ok(workspace)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

async fn version(binary: &Path) -> String {
    let mut command = tokio::process::Command::new(binary);
    command.arg("--version").kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|s| s.lines().next().map(|line| line.chars().take(80).collect()))
        .unwrap_or_else(|| "unknown".into())
}

/// Octet's own arguments and environment for this engine, plus what the
/// scenario needs: Claude's tool list and, for `mcp`, the fixture server;
/// for opencode's `lockdown` and `subagent`, the user configuration file
/// that allows the fixture command.
fn vendor_launch(
    opt: &Opt,
    cwd: &Path,
    resume: Option<&str>,
) -> (Vec<OsString>, Vec<(OsString, OsString)>) {
    let mut config = Config::new(opt.engine, &opt.binary, cwd);
    if let Some(session) = resume {
        config.resume = Some(session.to_owned());
        config.fork = opt.scenario == "fork";
    }
    let mut args = launch_args(&config);
    let mut env = launch_env(&config);
    if opt.engine == Engine::OPENCODE && locked_down(&opt.scenario) {
        env.push((
            "OPENCODE_CONFIG".into(),
            cwd.join(LOCKDOWN_USER_CONFIG).into_os_string(),
        ));
    }
    if opt.engine == Engine::CLAUDE {
        let tools = match opt.scenario.as_str() {
            s if s.starts_with("approval-") => "Bash",
            "hook" => "Bash,Read",
            "mcp" => "mcp__fixture__fixture_echo",
            _ => "",
        };
        args.extend(["--tools".into(), tools.into()]);
        if opt.scenario == "mcp" {
            args.extend([
                OsString::from("--mcp-config"),
                OsString::from(r#"{"mcpServers":{"fixture":{"type":"sdk","name":"fixture"}}}"#),
            ]);
        }
    }
    (args, env)
}

/// Answers a request the scenario does not handle, as Octet would.
async fn codex_default(gate: &GateProcess, v: &Value) -> Result<(), GateError> {
    if let Some(reply) = codex_stray_reply(v) {
        gate.send(&reply).await?;
    }
    Ok(())
}
fn is_reply_to(v: &Value, id: u64) -> bool {
    v.get("method").is_none() && v.get("id") == Some(&json!(id))
}
fn method(v: &Value) -> Option<&str> {
    v.get("method").and_then(Value::as_str)
}
fn is_approval_request(v: &Value) -> bool {
    method(v).is_some_and(|m| m.ends_with("/requestApproval"))
}

async fn codex(gate: &mut GateProcess, scenario: &str, cwd: &Path) -> Result<(), GateError> {
    codex_handshake(gate).await?;
    if scenario == "initialize" {
        return Ok(());
    }
    let (thread_id, model) = codex_open_thread(gate, scenario, cwd).await?;
    let ended = codex_run_turn(gate, scenario, cwd, &thread_id, &model).await?;
    if ended && matches!(scenario, "resume" | "fork" | "compact") {
        codex_session_op(gate, scenario, &thread_id).await?;
    }
    Ok(())
}

async fn codex_handshake(gate: &mut GateProcess) -> Result<(), GateError> {
    gate.send(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"octet_protocol_gate","title":"Octet Protocol Gate","version":"0.1.0"},"capabilities":{"experimentalApi":true}}})).await?;
    loop {
        let v = gate.receive().await?;
        if is_reply_to(&v, 1) {
            if v.get("error").is_some() || !v.get("result").is_some_and(Value::is_object) {
                return Err(GateError::Protocol("initialize error"));
            }
            break;
        }
        codex_default(gate, &v).await?;
    }
    gate.send(&json!({"method":"initialized","params":{}}))
        .await
}

/// Starts the thread; its ID and the model Codex chose.
async fn codex_open_thread(
    gate: &mut GateProcess,
    scenario: &str,
    cwd: &Path,
) -> Result<(String, String), GateError> {
    let approval = scenario.starts_with("approval-");
    let sandbox = if approval {
        "read-only"
    } else {
        "workspace-write"
    };
    let policy = if approval { "untrusted" } else { "on-request" };
    let instructions = if approval {
        "Protocol fixture: run exactly `printf READY > probe.out` in the current directory, then stop if approval is denied. Never inspect any other path."
    } else if scenario == "user-input" {
        "Protocol fixture: before answering, use request_user_input to ask whether to choose ALPHA or BETA."
    } else if scenario == "tool-only" {
        "Protocol fixture: use the terminal to read fixture.txt. Produce no assistant text before or after the tool call."
    } else {
        "Protocol fixture: do not use tools. Answer in plain text only."
    };
    gate.send(&json!({"id":2,"method":"thread/start","params":{"cwd":cwd,"sandbox":sandbox,"approvalPolicy":policy,"approvalsReviewer":"user","developerInstructions":instructions}})).await?;
    loop {
        let v = gate.receive().await?;
        if is_reply_to(&v, 2) {
            if v.get("error").is_some() {
                return Err(GateError::Protocol("thread/start error"));
            }
            let thread_id = v
                .pointer("/result/thread/id")
                .and_then(Value::as_str)
                .ok_or(GateError::Protocol("missing thread id"))?
                .to_owned();
            let model = v
                .pointer("/result/model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            return Ok((thread_id, model));
        }
        codex_default(gate, &v).await?;
    }
}

fn codex_prompt(scenario: &str) -> &'static str {
    match scenario {
        s if s.starts_with("approval-") => {
            "Use the terminal to run `printf READY > probe.out` in this temporary workspace. If permission is denied, stop and say denied."
        }
        "user-input" => {
            "Call request_user_input to ask me to choose ALPHA or BETA, then answer with the selected word."
        }
        "tool-only" => {
            "Run `cat fixture.txt` using a terminal tool. Do not send any assistant messages."
        }
        "interrupt" => "Count from 1 to 1000, one number per line. Do not call tools.",
        _ => "Reply with READY. Do not call tools.",
    }
}

/// The state of the one turn a Codex scenario runs.
#[derive(Default)]
struct CodexTurn {
    id: Option<String>,
    interrupt_sent: bool,
    interrupt_acked: bool,
    /// A terminal event that arrived before the interrupt's acknowledgement.
    pending_terminal: Option<Value>,
    ready_reply: bool,
}

impl CodexTurn {
    async fn interrupt(&mut self, gate: &GateProcess, thread_id: &str) -> Result<(), GateError> {
        let id = self
            .id
            .as_ref()
            .ok_or(GateError::Protocol("missing turn id"))?;
        self.interrupt_sent = true;
        gate.send(
            &json!({"id":4,"method":"turn/interrupt","params":{"threadId":thread_id,"turnId":id}}),
        )
        .await
    }
}

/// Runs the scenario's turn. True when it ended as expected and a session
/// operation may follow; false when a `failure` scenario ended early.
async fn codex_run_turn(
    gate: &mut GateProcess,
    scenario: &str,
    cwd: &Path,
    thread_id: &str,
    model: &str,
) -> Result<bool, GateError> {
    let mut params =
        json!({"threadId":thread_id,"input":[{"type":"text","text":codex_prompt(scenario)}]});
    if scenario == "user-input" {
        if model.is_empty() {
            return Err(GateError::Protocol("model missing for plan mode"));
        }
        params["collaborationMode"] = json!({"mode":"plan","settings":{"model":model}});
    }
    if scenario == "failure" {
        params["model"] = json!("octet-protocol-no-such-model");
    }
    gate.send(&json!({"id":3,"method":"turn/start","params":params}))
        .await?;
    let mut turn = CodexTurn::default();
    loop {
        let mut v = gate.receive().await?;
        if is_reply_to(&v, 3) {
            if v.get("error").is_some() {
                if scenario == "failure" {
                    gate.record_failure("turn_start_error");
                    return Ok(false);
                }
                return Err(GateError::Protocol("turn/start error"));
            }
            turn.id = v
                .pointer("/result/turn/id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if scenario == "interrupt" && method(&v) == Some("turn/started") {
            let id = turn
                .id
                .as_deref()
                .ok_or(GateError::Protocol("missing turn id"))?;
            if v.pointer("/params/turn/id").and_then(Value::as_str) != Some(id) {
                return Err(GateError::Protocol("turn/started id mismatch"));
            }
            turn.interrupt(gate, thread_id).await?;
        }
        let this_turn = v.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
            && turn.id.is_some()
            && v.pointer("/params/turnId").and_then(Value::as_str) == turn.id.as_deref();
        let item = v.pointer("/params/item/type").and_then(Value::as_str);
        if method(&v) == Some("item/completed") && this_turn && item == Some("agentMessage") {
            turn.ready_reply = v
                .pointer("/params/item/text")
                .and_then(Value::as_str)
                .map(str::trim)
                == Some("READY");
        }
        if scenario == "tool-only"
            && !turn.interrupt_sent
            && method(&v) == Some("item/completed")
            && item == Some("commandExecution")
            && gate.evidence().agent_messages == 0
        {
            turn.interrupt(gate, thread_id).await?;
        }
        if is_reply_to(&v, 4) {
            if v.get("error").is_some() {
                return Err(GateError::Protocol("interrupt error"));
            }
            turn.interrupt_acked = true;
            if let Some(terminal) = turn.pending_terminal.take() {
                v = terminal;
            }
        }
        if method(&v) == Some("turn/completed") {
            if turn.interrupt_sent && !turn.interrupt_acked {
                turn.pending_terminal = Some(v);
                continue;
            }
            return codex_turn_ended(gate, scenario, thread_id, &turn, &v);
        }
        codex_turn_request(gate, scenario, cwd, thread_id, &mut turn, &v).await?;
    }
}

/// Checks the turn's terminal event against what the scenario expects.
fn codex_turn_ended(
    gate: &mut GateProcess,
    scenario: &str,
    thread_id: &str,
    turn: &CodexTurn,
    v: &Value,
) -> Result<bool, GateError> {
    let actual = v.pointer("/params/turn/id").and_then(Value::as_str);
    let status = v.pointer("/params/turn/status").and_then(Value::as_str);
    if turn.id.is_none()
        || actual != turn.id.as_deref()
        || v.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id)
    {
        return Err(GateError::Protocol("terminal thread or turn id mismatch"));
    }
    if scenario == "failure" {
        if status != Some("failed") {
            return Err(GateError::Protocol("expected failed turn"));
        }
        gate.record_failure("turn_failed");
        return Ok(false);
    }
    let interrupting = matches!(scenario, "interrupt" | "approval-interrupt" | "tool-only");
    let expected = if interrupting {
        "interrupted"
    } else {
        "completed"
    };
    if status != Some(expected) || interrupting && !turn.interrupt_acked {
        return Err(GateError::Protocol(
            "turn completed with wrong id or status",
        ));
    }
    let evidence = gate.evidence();
    if scenario == "simple" && !turn.ready_reply {
        return Err(GateError::Protocol("unexpected simple reply"));
    }
    if scenario.starts_with("approval-") && evidence.approval_requests == 0 {
        return Err(GateError::Protocol("no approval request observed"));
    }
    if scenario == "user-input" && evidence.user_questions == 0 {
        return Err(GateError::Protocol("no user question observed"));
    }
    if scenario == "tool-only" && evidence.tool_only_turns == 0 {
        return Err(GateError::Protocol("no tool-only turn observed"));
    }
    Ok(true)
}

/// Answers a server request during the turn as the scenario says.
async fn codex_turn_request(
    gate: &mut GateProcess,
    scenario: &str,
    cwd: &Path,
    thread_id: &str,
    turn: &mut CodexTurn,
    v: &Value,
) -> Result<(), GateError> {
    let stray = || codex_stray_reply(v).ok_or(GateError::Protocol("approval request without id"));
    if scenario == "approval-interrupt" && is_approval_request(v) {
        if codex_fixture_allow(v, cwd).is_none() {
            return gate.send(&stray()?).await;
        }
        gate.record_approval();
        return turn.interrupt(gate, thread_id).await;
    }
    if scenario == "user-input"
        && let Some(reply) = codex_fixture_user_input(v)
    {
        gate.record_user_question();
        return gate.send(&reply).await;
    }
    if scenario == "approval-allow" && is_approval_request(v) {
        let allowed = codex_fixture_allow(v, cwd);
        if allowed.is_some() {
            gate.record_approval();
        }
        let reply = match allowed {
            Some(reply) => reply,
            None => stray()?,
        };
        return gate.send(&reply).await;
    }
    if let Some(reply) = codex_stray_reply(v) {
        if scenario == "approval-deny"
            && codex_fixture_allow(v, cwd).is_some()
            && is_approval_request(v)
        {
            gate.record_approval();
        }
        gate.send(&reply).await?;
    }
    Ok(())
}

/// After the turn: resume, fork or compact the thread and check the result.
async fn codex_session_op(
    gate: &mut GateProcess,
    scenario: &str,
    thread_id: &str,
) -> Result<(), GateError> {
    let op = match scenario {
        "resume" => "thread/resume",
        "fork" => "thread/fork",
        _ => "thread/compact/start",
    };
    gate.send(&json!({"id":5,"method":op,"params":{"threadId":thread_id}}))
        .await?;
    let mut ack = false;
    let mut compacted = false;
    let mut compact_turn_id: Option<String> = None;
    let mut compact_terminal = false;
    loop {
        let next = gate.receive().await?;
        if is_reply_to(&next, 5) {
            if next.get("error").is_some() {
                return Err(GateError::Protocol("session operation error"));
            }
            let id = next.pointer("/result/thread/id").and_then(Value::as_str);
            if scenario == "resume" && id != Some(thread_id) {
                return Err(GateError::Protocol("resume id mismatch"));
            }
            if scenario == "fork" && id.is_none_or(|id| id == thread_id) {
                return Err(GateError::Protocol("fork id missing or unchanged"));
            }
            ack = true;
        }
        if next.get("id").is_none()
            && next.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id)
        {
            continue;
        }
        let item = next.pointer("/params/item/type").and_then(Value::as_str);
        match method(&next) {
            Some("thread/compacted") => compacted = true,
            Some("item/completed") if item == Some("contextCompaction") => compacted = true,
            Some("turn/started") => {
                compact_turn_id = next
                    .pointer("/params/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("turn/completed")
                if compact_turn_id.is_some()
                    && next.pointer("/params/turn/id").and_then(Value::as_str)
                        == compact_turn_id.as_deref()
                    && next.pointer("/params/turn/status").and_then(Value::as_str)
                        == Some("completed") =>
            {
                compact_terminal = true;
            }
            _ => {}
        }
        if ack && (scenario != "compact" || compacted && compact_terminal) {
            return Ok(());
        }
        codex_default(gate, &next).await?;
    }
}

async fn claude(gate: &mut GateProcess, scenario: &str, cwd: &Path) -> Result<(), GateError> {
    claude_handshake(gate, scenario).await?;
    if scenario == "initialize" {
        return Ok(());
    }
    gate.send(&json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":claude_prompt(scenario)}]},"parent_tool_use_id":null})).await?;
    if scenario == "interrupt" {
        gate.send(&json!({"type":"control_request","request_id":"interrupt-2","request":{"subtype":"interrupt"}})).await?;
    }
    claude_turn(gate, scenario, cwd).await
}

async fn claude_handshake(gate: &mut GateProcess, scenario: &str) -> Result<(), GateError> {
    let hooks = if scenario == "hook" {
        json!({"PreToolUse":[{"matcher":null,"hookCallbackIds":["hook_0"]}]})
    } else {
        Value::Null
    };
    gate.send(&json!({"type":"control_request","request_id":"init-1","request":{"subtype":"initialize","hooks":hooks}})).await?;
    loop {
        let v = gate.receive().await?;
        if v.get("type").and_then(Value::as_str) == Some("control_response")
            && v.pointer("/response/request_id").and_then(Value::as_str) == Some("init-1")
        {
            if v.pointer("/response/subtype").and_then(Value::as_str) != Some("success") {
                return Err(GateError::Protocol("initialize error"));
            }
            return Ok(());
        }
        if scenario == "mcp"
            && let Some((reply, _)) = claude_fixture_mcp_response(&v)
        {
            gate.send(&reply).await?;
            continue;
        }
        if let Some(reply) = claude_stray_reply(&v) {
            gate.send(&reply).await?;
        }
    }
}

fn claude_prompt(scenario: &str) -> &'static str {
    match scenario {
        "hook" => {
            "Use Bash to run `cat fixture.txt` in this temporary workspace, then reply READY."
        }
        "mcp" => "Call the fixture_echo MCP tool exactly once, then reply with its returned word.",
        s if s.starts_with("approval-") => {
            "Invoke Bash exactly once with command `printf READY > probe.out`. Use that relative path and no other shell commands or checks. If permission is denied, stop and say denied."
        }
        "interrupt" => "Count from 1 to 1000, one number per line. Do not call tools.",
        _ => "Reply with READY. Do not call tools.",
    }
}

async fn claude_turn(gate: &mut GateProcess, scenario: &str, cwd: &Path) -> Result<(), GateError> {
    let interrupting = scenario == "interrupt" || scenario == "approval-interrupt";
    let mut interrupt_acked = false;
    let mut compact_requested = false;
    let mut pending_result = None;
    loop {
        let mut v = gate.receive().await?;
        if interrupting
            && v.get("type").and_then(Value::as_str) == Some("control_response")
            && v.pointer("/response/request_id").and_then(Value::as_str) == Some("interrupt-2")
        {
            if v.pointer("/response/subtype").and_then(Value::as_str) != Some("success") {
                return Err(GateError::Protocol("interrupt error"));
            }
            interrupt_acked = true;
            if let Some(result) = pending_result.take() {
                v = result;
            }
        }
        if v.get("type").and_then(Value::as_str) == Some("result") {
            if interrupting {
                if !interrupt_acked {
                    pending_result = Some(v);
                    continue;
                }
                return if scenario == "interrupt" || gate.evidence().approval_requests > 0 {
                    Ok(())
                } else {
                    Err(GateError::Protocol("result before interrupt ack"))
                };
            }
            if v.get("is_error").and_then(Value::as_bool) != Some(false) {
                return Err(GateError::Protocol("result error"));
            }
            if scenario == "simple"
                && v.get("result").and_then(Value::as_str).map(str::trim) != Some("READY")
            {
                return Err(GateError::Protocol("unexpected simple reply"));
            }
            if scenario == "compact" && !compact_requested {
                gate.send(&json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"/compact"}]},"parent_tool_use_id":null})).await?;
                compact_requested = true;
                continue;
            }
            return claude_result_checks(gate.evidence(), scenario);
        }
        if scenario == "hook"
            && let Some(reply) = claude_fixture_hook_response(&v)
        {
            gate.record_hook_call();
            gate.send(&reply).await?;
            continue;
        }
        if scenario == "mcp"
            && let Some((reply, called)) = claude_fixture_mcp_response(&v)
        {
            if called {
                gate.record_mcp_call();
            }
            gate.send(&reply).await?;
            continue;
        }
        if let Some(reply) = claude_stray_reply(&v) {
            claude_request(gate, scenario, cwd, &v, reply).await?;
        }
    }
}

/// What a successful result must have been preceded by.
fn claude_result_checks(evidence: &Evidence, scenario: &str) -> Result<(), GateError> {
    if scenario == "compact" && evidence.compact_boundaries == 0 {
        return Err(GateError::Protocol("no compact boundary observed"));
    }
    if scenario.starts_with("approval-") && evidence.approval_requests == 0 {
        return Err(GateError::Protocol("no approval request observed"));
    }
    if scenario == "mcp" && evidence.mcp_calls == 0 {
        return Err(GateError::Protocol("no fixture MCP call observed"));
    }
    if scenario == "hook" && evidence.hook_calls == 0 {
        return Err(GateError::Protocol("no hook callback observed"));
    }
    Ok(())
}

/// Answers a control request: the scenario's fixture permission, or
/// Octet's own `reply`.
async fn claude_request(
    gate: &mut GateProcess,
    scenario: &str,
    cwd: &Path,
    v: &Value,
    reply: Value,
) -> Result<(), GateError> {
    if v.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool") {
        let fixture_write = claude_fixture_allow(v, cwd);
        if scenario.starts_with("approval-") && fixture_write.is_some() {
            gate.record_approval();
        }
        match scenario {
            "approval-interrupt" if fixture_write.is_some() => {
                return gate.send(&json!({"type":"control_request","request_id":"interrupt-2","request":{"subtype":"interrupt"}})).await;
            }
            "approval-allow" => return gate.send(&fixture_write.unwrap_or(reply)).await,
            "mcp" => {
                let allowed = claude_fixture_mcp_tool_allow(v);
                if allowed.is_some() {
                    gate.record_approval();
                }
                return gate.send(&allowed.unwrap_or(reply)).await;
            }
            _ => {}
        }
    }
    gate.send(&reply).await
}

/// The free model every opencode scenario runs on: a turn costs nothing.
const OPENCODE_MODEL: &str = "opencode/deepseek-v4.1-flash-free";
/// The agent Octet runs opencode's `ask` mode in, its default.
const OPENCODE_ASK: &str = "octet-ask";
/// The most reply text an opencode scenario keeps.
const OPENCODE_TEXT_LIMIT: usize = 64 * 1024;
/// The most tool calls an opencode turn tracks.
const OPENCODE_TOOL_LIMIT: usize = 256;
/// `lockdown`'s project configuration: it would allow everything, in
/// Octet's own `ask` agent too, if opencode read it.
const LOCKDOWN_PROJECT_CONFIG: &str =
    r#"{"permission":"allow","agent":{"octet-ask":{"permission":{"*":"allow"}}}}"#;
/// `lockdown`'s user rule, read through `OPENCODE_CONFIG`: it allows the
/// command the scenario asks for, and defines [`GATE_COMMAND`], a slash
/// command Octet must refuse in `ask` (its own agent, a subagent, shell in
/// its template), which nothing sends.
const LOCKDOWN_USER_RULE: &str = r#"{"permission":{"bash":{"echo *":"allow"}},
"command":{"octet-gate-command":{"description":"Octet gate command","agent":"build","subtask":true,
"template":"Gate !`echo gate`"}}}"#;
/// The slash command `lockdown`'s user rule defines.
const GATE_COMMAND: &str = "octet-gate-command";
/// The description `lockdown`'s user rule gives [`GATE_COMMAND`].
const GATE_COMMAND_DESCRIPTION: &str = "Octet gate command";
/// Where in the workspace `lockdown` keeps its user rule.
const LOCKDOWN_USER_CONFIG: &str = "user-config.json";
/// How long one `opencode session` command may run.
const OPENCODE_SESSION_LIMIT: Duration = Duration::from_secs(15);

/// What one opencode prompt did, as its session updates showed it.
#[derive(Default)]
struct OpencodeTurn {
    /// The reply text, bounded.
    text: String,
    /// `session/cancel` was sent.
    cancel_sent: bool,
    /// The tool calls opencode asked permission for.
    asked: HashSet<String>,
    /// Each tool call's kind, from its first frame.
    kinds: Vec<(String, String)>,
    /// Calls that changed something without a permission request.
    unasked: u64,
    /// The `task` calls (subagents) seen, by ID.
    tasks: HashSet<String>,
    /// `task` calls that completed: a subagent ran, under an agent of its
    /// own whose tools the parent session does not see.
    subagents: u64,
}

impl OpencodeTurn {
    /// Records a `tool_call` or `tool_call_update`: a call that edits or
    /// runs something and completes without having asked is counted.
    fn tool(&mut self, update: &Value) {
        let id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
        if let Some(kind) = update["kind"].as_str()
            && !self.kinds.iter().any(|(known, _)| *known == id)
            && self.kinds.len() < OPENCODE_TOOL_LIMIT
        {
            self.kinds.push((id.clone(), kind.to_owned()));
        }
        let kind = self
            .kinds
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, kind)| kind.as_str());
        let acts = matches!(kind, Some("execute" | "edit" | "delete" | "move"));
        if acts && update["status"] == "completed" && !self.asked.contains(&id) {
            self.unasked += 1;
        }
        // opencode names a call after its tool in its first frame; a task's
        // input names the subagent.
        let task = update["title"] == "task" || update.pointer("/rawInput/subagent_type").is_some();
        if task && self.tasks.len() < OPENCODE_TOOL_LIMIT {
            self.tasks.insert(id.clone());
        }
        if update["status"] == "completed" && self.tasks.contains(&id) {
            self.subagents += 1;
        }
    }
}

/// One opencode connection under a scenario: Octet's last request ID, the
/// open session and the running prompt.
struct Opencode<'a> {
    gate: &'a mut GateProcess,
    scenario: &'a str,
    next_id: u64,
    session: String,
    turn: Option<OpencodeTurn>,
    /// Updates for the session before its first prompt: a resume's replay.
    replayed: u64,
    /// The description opencode lists [`GATE_COMMAND`] with, once it lists
    /// its commands.
    gate_command: Option<String>,
}

impl<'a> Opencode<'a> {
    /// A connection on `gate` with no session yet.
    fn new(gate: &'a mut GateProcess, scenario: &'a str) -> Self {
        Self {
            gate,
            scenario,
            next_id: 0,
            session: String::new(),
            turn: None,
            replayed: 0,
            gate_command: None,
        }
    }

    /// Sends `method` and waits for its reply, handling what comes before.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, GateError> {
        self.next_id += 1;
        let id = self.next_id;
        self.gate
            .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let v = self.gate.receive().await?;
            if is_reply_to(&v, id) {
                return Ok(v);
            }
            self.other(&v).await?;
        }
    }

    /// A frame that is not the awaited reply: a request is answered, an
    /// update for this session counted.
    async fn other(&mut self, v: &Value) -> Result<(), GateError> {
        let this_session = !self.session.is_empty()
            && v.pointer("/params/sessionId").and_then(Value::as_str)
                == Some(self.session.as_str());
        if v.get("id").is_some() && method(v).is_some() {
            if method(v) == Some("session/request_permission") && this_session {
                return self.permission(v).await;
            }
            if let Some(reply) = opencode_stray_reply(v) {
                self.gate.send(&reply).await?;
            }
            return Ok(());
        }
        if method(v) != Some("session/update") || !this_session {
            return Ok(());
        }
        let update = &v["params"]["update"];
        if update["sessionUpdate"] == "available_commands_update" {
            self.gate_command = update["availableCommands"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|command| command["name"] == GATE_COMMAND)
                .map(|command| {
                    command["description"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                });
        }
        let Some(turn) = self.turn.as_mut() else {
            if update["sessionUpdate"] != "available_commands_update" {
                self.replayed += 1;
            }
            return Ok(());
        };
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(text) = update.pointer("/content/text").and_then(Value::as_str)
                    && turn.text.len() + text.len() <= OPENCODE_TEXT_LIMIT
                {
                    turn.text.push_str(text);
                }
                if self.scenario == "interrupt" && !turn.cancel_sent {
                    turn.cancel_sent = true;
                    let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session}});
                    self.gate.send(&cancel).await?;
                }
            }
            Some("tool_call" | "tool_call_update") => {
                if env::var_os("OCTET_GATE_TRACE").is_some() {
                    let shown: String = update.to_string().chars().take(2000).collect();
                    eprintln!("tool {shown}");
                }
                turn.tool(update);
            }
            _ => {}
        }
        Ok(())
    }

    /// A permission request for this session, answered as the scenario
    /// says: the fixture write allowed in `approval-allow`, a subagent in
    /// `subagent` (as a user might), everything else rejected with Octet's
    /// own reply.
    async fn permission(&mut self, v: &Value) -> Result<(), GateError> {
        if env::var_os("OCTET_GATE_TRACE").is_some() {
            let shown: String = v.to_string().chars().take(2000).collect();
            eprintln!("permission {shown}");
        }
        let fixture = opencode_is_fixture_request(v);
        let counted = match self.scenario {
            "approval-allow" | "approval-deny" => fixture,
            "lockdown" | "subagent" => opencode_is_lockdown_request(v),
            _ => false,
        };
        if counted {
            self.gate.record_approval();
        }
        if let (Some(turn), Some(id)) = (
            self.turn.as_mut(),
            v.pointer("/params/toolCall/toolCallId")
                .and_then(Value::as_str),
        ) && turn.asked.len() < OPENCODE_TOOL_LIMIT
        {
            turn.asked.insert(id.to_owned());
        }
        let allow = match self.scenario {
            "approval-allow" => opencode_fixture_allow(v),
            "subagent" => opencode_task_allow(v),
            _ => None,
        };
        let reply = match allow {
            Some(allow) => allow,
            None => opencode_stray_reply(v)
                .ok_or(GateError::Protocol("permission request without id"))?,
        };
        self.gate.send(&reply).await
    }

    /// The handshake: `initialize`, which must report a protocol version.
    async fn initialize(&mut self) -> Result<(), GateError> {
        let reply = self
            .request(
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":{}}),
            )
            .await?;
        if reply.get("error").is_some()
            || !reply
                .pointer("/result/protocolVersion")
                .is_some_and(Value::is_u64)
        {
            return Err(GateError::Protocol("initialize error"));
        }
        Ok(())
    }

    /// Opens a new session, or resumes or forks `from`, as Octet does.
    async fn open(&mut self, cwd: &Path, from: Option<&str>) -> Result<(), GateError> {
        let mut params = json!({"cwd":cwd,"mcpServers":[]});
        let method = match from {
            Some(id) => {
                params["sessionId"] = json!(id);
                if self.scenario == "fork" {
                    "session/fork"
                } else {
                    "session/resume"
                }
            }
            None => "session/new",
        };
        let reply = self.request(method, params).await?;
        if reply.get("error").is_some() {
            return Err(GateError::Protocol("session open error"));
        }
        let reported = reply.pointer("/result/sessionId").and_then(Value::as_str);
        let session = match (method, reported) {
            ("session/resume", Some(id)) if Some(id) != from => {
                return Err(GateError::Protocol("resume id mismatch"));
            }
            ("session/resume", _) => from.unwrap_or_default(),
            ("session/fork", Some(id)) if Some(id) != from => id,
            ("session/fork", _) => return Err(GateError::Protocol("fork id missing or unchanged")),
            (_, Some(id)) => id,
            (_, None) => return Err(GateError::Protocol("missing session id")),
        }
        .to_owned();
        self.gate.record_session(&session);
        self.session = session;
        Ok(())
    }

    /// Sets the session's option `id` to `value`; the reply must confirm it
    /// by reporting the option at `value`, as Octet requires of an agent.
    async fn set(&mut self, id: &str, value: &str) -> Result<(), GateError> {
        let params = json!({"sessionId":self.session,"configId":id,"value":value});
        let reply = self.request("session/set_config_option", params).await?;
        if reply.get("error").is_some() {
            return Err(GateError::Protocol("set_config_option error"));
        }
        let reported = reply
            .pointer("/result/configOptions")
            .and_then(Value::as_array)
            .and_then(|options| options.iter().find(|option| option["id"] == id))
            .map(|option| &option["currentValue"]);
        if reported.is_none_or(|current| current != value) {
            return Err(GateError::Protocol("set_config_option not confirmed"));
        }
        Ok(())
    }

    /// Runs one prompt to its reply; the reply's stop reason and the turn.
    async fn prompt(&mut self, text: &str) -> Result<(String, OpencodeTurn), GateError> {
        self.turn = Some(OpencodeTurn::default());
        let params = json!({"sessionId":self.session,"prompt":[{"type":"text","text":text}]});
        let reply = self.request("session/prompt", params).await?;
        let turn = self.turn.take().unwrap_or_default();
        if reply.get("error").is_some() {
            return Err(GateError::Protocol("prompt error"));
        }
        let stop = reply
            .pointer("/result/stopReason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.gate.record_stop_reason(&stop);
        Ok((stop, turn))
    }
}

/// The prompt a scenario's first connection sends opencode: a plain reply,
/// a counting reply to cancel, or a request for the one command (directly,
/// or through a subagent) whose permission request the scenario checks.
fn opencode_prompt(scenario: &str) -> &'static str {
    match scenario {
        s if s.starts_with("approval-") => {
            "Use the bash tool to run exactly `printf READY > probe.out` in the current directory, once, with no other commands. If permission is denied, stop and say denied."
        }
        "lockdown" => {
            "Use the bash tool to run exactly `echo fixture`, once, with no other commands. If permission is denied, stop and say denied."
        }
        "subagent" => {
            "Use the task tool to start a general subagent whose only job is to run exactly `echo fixture` with its bash tool, once. Do not run any command yourself. If the task tool is unavailable or denied, stop and say denied."
        }
        "interrupt" => "Count from 1 to 1000, one number per line. Do not call tools.",
        _ => "Reply with READY. Do not call tools.",
    }
}

/// Runs the scenario's first connection: a session in Octet's `ask` agent
/// on the free model, and its one prompt, checked.
async fn opencode(gate: &mut GateProcess, scenario: &str, cwd: &Path) -> Result<(), GateError> {
    let mut acp = Opencode::new(gate, scenario);
    acp.initialize().await?;
    if scenario == "initialize" {
        return Ok(());
    }
    acp.open(cwd, None).await?;
    acp.set("model", OPENCODE_MODEL).await?;
    acp.set("mode", OPENCODE_ASK).await?;
    let (stop, turn) = acp.prompt(opencode_prompt(scenario)).await?;
    // A finished prompt ends with `end_turn`. opencode 1.18.25 ends a
    // cancelled one the same way; ACP's own `cancelled` is accepted too, for
    // the interrupt scenario only.
    let stopped = stop == "end_turn" || scenario == "interrupt" && stop == "cancelled";
    if !stopped {
        return Err(GateError::Protocol(
            "prompt ended with an unexpected stop reason",
        ));
    }
    if turn.unasked > 0 {
        return Err(GateError::Protocol(
            "a tool changed something without a permission request",
        ));
    }
    if turn.subagents > 0 {
        return Err(GateError::Protocol(
            "a subagent ran outside Octet's permission requests",
        ));
    }
    let asked = acp.gate.evidence().approval_requests > 0;
    match scenario {
        "simple" | "resume" | "fork" if turn.text.trim() != "READY" => {
            Err(GateError::Protocol("unexpected simple reply"))
        }
        "interrupt" if !turn.cancel_sent => Err(GateError::Protocol("no reply text to cancel")),
        "interrupt" if turn.text.contains("1000") => {
            Err(GateError::Protocol("the cancelled turn ran to its end"))
        }
        s if s.starts_with("approval-") && !asked => {
            Err(GateError::Protocol("no approval request observed"))
        }
        // A command that ran unasked failed the run above.
        "lockdown" if !asked => Err(GateError::Protocol(
            "the model ran no command, so lockdown is unproven",
        )),
        // Octet checks a command against its definition using the
        // description opencode lists it with.
        "lockdown" if acp.gate_command.as_deref() != Some(GATE_COMMAND_DESCRIPTION) => Err(
            GateError::Protocol("opencode does not list the gate command with its description"),
        ),
        _ => Ok(()),
    }
}

/// Reopens the first connection's session `first` in a new process, as
/// `--resume` (and a fork) does: the session, or its fork, must remember
/// the first reply.
async fn opencode_reopen(
    gate: &mut GateProcess,
    scenario: &str,
    cwd: &Path,
    first: &str,
) -> Result<u64, GateError> {
    let mut acp = Opencode::new(gate, scenario);
    acp.initialize().await?;
    acp.open(cwd, Some(first)).await?;
    acp.set("model", OPENCODE_MODEL).await?;
    acp.set("mode", OPENCODE_ASK).await?;
    let replayed = acp.replayed;
    let (stop, turn) = acp
        .prompt("Which word did you reply with earlier in this conversation? Reply with that word only. Do not call tools.")
        .await?;
    if stop != "end_turn" {
        return Err(GateError::Protocol(
            "prompt ended with an unexpected stop reason",
        ));
    }
    if !turn.text.contains("READY") {
        return Err(GateError::Protocol(
            "the reopened session lost the conversation",
        ));
    }
    Ok(replayed)
}

/// Runs an opencode scenario: one connection, and for `resume` and `fork` a
/// second, in a new process. The status, the evidence, the updates a resume
/// replayed, and whether every child stopped.
async fn run_opencode(opt: &Opt, cwd: &Path, deadline: Instant) -> (String, Evidence, u64, bool) {
    let spawn = |resume: Option<&str>| {
        let (args, env) = vendor_launch(opt, cwd, resume);
        GateProcess::spawn(
            opt.engine,
            opt.binary.clone(),
            args,
            env,
            cwd.to_owned(),
            deadline,
        )
    };
    let mut gate = match spawn(None) {
        Ok(gate) => gate,
        Err(error) => return (format!("failed: {error}"), Evidence::default(), 0, false),
    };
    let mut result = opencode(&mut gate, &opt.scenario, cwd).await;
    if result.is_ok() && opt.scenario == "lockdown" {
        result = opencode_command_contract(opt, cwd).await;
    }
    let mut evidence = gate.evidence().clone();
    let report = gate.shutdown().await;
    let mut cleaned = report.reaped && report.descendants_stopped;
    let outcome = match result {
        Ok(()) if cleaned => "passed".to_owned(),
        Ok(()) => "failed: child cleanup".to_owned(),
        Err(error) => format!("failed: {error}"),
    };
    if outcome != "passed" || !matches!(opt.scenario.as_str(), "resume" | "fork") {
        return (outcome, evidence, 0, cleaned);
    }
    let Some(first) = evidence.session_id.clone() else {
        return (
            "failed: first session id missing".into(),
            evidence,
            0,
            cleaned,
        );
    };
    let mut second = match spawn(Some(&first)) {
        Ok(second) => second,
        Err(error) => return (format!("failed: {error}"), evidence, 0, cleaned),
    };
    let result = opencode_reopen(&mut second, &opt.scenario, cwd, &first).await;
    evidence.add_resumed(second.evidence());
    let report = second.shutdown().await;
    cleaned &= report.reaped && report.descendants_stopped;
    match result {
        Ok(_) if !cleaned => ("failed: resumed child cleanup".into(), evidence, 0, cleaned),
        Ok(replayed) => ("passed".into(), evidence, replayed, cleaned),
        Err(error) => (format!("failed: {error}"), evidence, 0, cleaned),
    }
}

/// What Octet reads before it sends a command while the mode asks:
/// `opencode debug config`, run with the session's launch environment in
/// its workspace, must show [`GATE_COMMAND`]'s agent, subtask setting and
/// template, and `opencode debug skill` must print a list of skills.
async fn opencode_command_contract(opt: &Opt, cwd: &Path) -> Result<(), GateError> {
    let config = opencode_debug(opt, cwd, "config")
        .await
        .ok_or(GateError::Protocol("opencode debug config printed no JSON"))?;
    let command = &config["command"][GATE_COMMAND];
    let shown = command["agent"] == "build"
        && command["subtask"] == true
        && command["template"]
            .as_str()
            .is_some_and(|template| template.contains("!`"))
        && command["description"] == GATE_COMMAND_DESCRIPTION;
    if !shown {
        return Err(GateError::Protocol(
            "opencode debug config does not show a command's agent, subtask and template",
        ));
    }
    let skills = opencode_debug(opt, cwd, "skill")
        .await
        .ok_or(GateError::Protocol("opencode debug skill printed no JSON"))?;
    if !skills.is_array() {
        return Err(GateError::Protocol("opencode debug skill printed no list"));
    }
    Ok(())
}

/// `opencode debug what`'s output as JSON, run as Octet runs it (the
/// scenario's launch environment, in `cwd`, its output to a file: opencode
/// 1.18.25 exits before a pipe takes more than 64 KiB); `None` when it
/// fails, overruns or prints something else.
async fn opencode_debug(opt: &Opt, cwd: &Path, what: &str) -> Option<Value> {
    let path = cwd.join(format!("debug-{what}.json"));
    let file = fs::File::create(&path).ok()?;
    let (_, env) = vendor_launch(opt, cwd, None);
    let mut command = tokio::process::Command::new(&opt.binary);
    command
        .args(["debug", what])
        .envs(env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(OPENCODE_SESSION_LIMIT, command.status())
        .await
        .ok()?
        .ok()?;
    let text = fs::read_to_string(&path).ok();
    let _ = fs::remove_file(&path);
    status.success().then_some(())?;
    serde_json::from_str(&text?).ok()
}

/// Runs `opencode session ARGS` as Octet's launch would, in `cwd`; its
/// output, or `None` when it fails or overruns.
async fn opencode_session(opt: &Opt, cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let config = Config::new(opt.engine, &opt.binary, cwd);
    let mut command = tokio::process::Command::new(&opt.binary);
    command
        .arg("session")
        .args(args)
        .envs(launch_env(&config))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(OPENCODE_SESSION_LIMIT, command.output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Deletes the sessions an opencode run opened, once its processes are
/// gone, so the user's history keeps none of them. The ones still listed
/// afterwards, or `None` when the list cannot be read.
async fn opencode_cleanup(opt: &Opt, cwd: &Path, sessions: &[String]) -> Option<Vec<String>> {
    if sessions.is_empty() {
        return Some(Vec::new());
    }
    for id in sessions {
        if valid_session_id(id) {
            let _ = opencode_session(opt, cwd, &["delete", id]).await;
        }
    }
    let listed = opencode_session(opt, cwd, &["list", "--format", "json"]).await?;
    // An empty history lists nothing at all.
    let listed: Value = if listed.trim_ascii().is_empty() {
        json!([])
    } else {
        serde_json::from_slice(&listed).ok()?
    };
    let listed: HashSet<&str> = listed
        .as_array()?
        .iter()
        .filter_map(|session| session["id"].as_str())
        .collect();
    Some(
        sessions
            .iter()
            .filter(|id| listed.contains(id.as_str()))
            .cloned()
            .collect(),
    )
}

/// Whether `id` may be passed to `opencode session delete`: no option, no
/// control characters, bounded.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Runs the scenario once, and for Claude's `resume` and `fork` again in a
/// second process. The status, the evidence and whether every child stopped.
async fn run_scenario(opt: &Opt, cwd: &Path, deadline: Instant) -> (String, Evidence, bool) {
    let (args, env) = vendor_launch(opt, cwd, None);
    let mut gate = match GateProcess::spawn(
        opt.engine,
        opt.binary.clone(),
        args,
        env,
        cwd.to_owned(),
        deadline,
    ) {
        Ok(gate) => gate,
        Err(error) => return (format!("failed: {error}"), Evidence::default(), false),
    };
    let result = if opt.engine == Engine::CLAUDE {
        claude(&mut gate, &opt.scenario, cwd).await
    } else {
        codex(&mut gate, &opt.scenario, cwd).await
    };
    let mut evidence = gate.evidence().clone();
    let report = gate.shutdown().await;
    let mut cleaned = report.reaped && report.descendants_stopped;
    let outcome = match result {
        Ok(()) if cleaned => "passed".to_owned(),
        Ok(()) => "failed: child cleanup".to_owned(),
        Err(error) => format!("failed: {error}"),
    };
    let second_run = outcome == "passed"
        && opt.engine == Engine::CLAUDE
        && matches!(opt.scenario.as_str(), "resume" | "fork");
    if !second_run {
        return (outcome, evidence, cleaned);
    }
    let Some(first) = evidence.session_id.clone() else {
        return ("failed: first session id missing".into(), evidence, cleaned);
    };
    let (args, env) = vendor_launch(opt, cwd, Some(&first));
    let mut second = match GateProcess::spawn(
        opt.engine,
        opt.binary.clone(),
        args,
        env,
        cwd.to_owned(),
        deadline,
    ) {
        Ok(second) => second,
        Err(error) => return (format!("failed: {error}"), evidence, cleaned),
    };
    let second_result = claude(&mut second, "simple", cwd).await;
    evidence.add_resumed(second.evidence());
    let resumed = second.evidence().session_id.clone();
    let second_report = second.shutdown().await;
    cleaned &= second_report.reaped && second_report.descendants_stopped;
    let outcome = match second_result {
        Ok(()) if !cleaned => "failed: resumed child cleanup".into(),
        Ok(()) if opt.scenario == "resume" && resumed.as_deref() == Some(first.as_str()) => {
            "passed".into()
        }
        Ok(()) if opt.scenario == "fork" && resumed.as_deref().is_some_and(|id| id != first) => {
            "passed".into()
        }
        Ok(()) => "failed: session id mismatch".into(),
        Err(error) => format!("failed: {error}"),
    };
    (outcome, evidence, cleaned)
}

/// Whether the fixture write landed: exactly `READY` in `probe.out`.
fn fixture_written(cwd: &Path) -> bool {
    fs::File::open(cwd.join("probe.out"))
        .and_then(|file| {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(6).read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .is_ok_and(|bytes| bytes == b"READY")
}

/// Runs the gate; the process's exit code. Everything it owns, the
/// workspace above all, is dropped before the process exits.
async fn run() -> i32 {
    let opt = match options() {
        Ok(opt) => opt,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let workspace = match Workspace::create(opt.workdir.clone()) {
        Ok(workspace) => workspace,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let cwd = workspace.path.clone();
    let opencode = opt.engine == Engine::OPENCODE;
    if opencode && locked_down(&opt.scenario) {
        let written = fs::write(cwd.join("opencode.json"), LOCKDOWN_PROJECT_CONFIG)
            .and_then(|()| fs::write(cwd.join(LOCKDOWN_USER_CONFIG), LOCKDOWN_USER_RULE));
        if written.is_err() {
            eprintln!("cannot write the lockdown configuration");
            return 2;
        }
    }
    let started = Instant::now();
    let deadline = started + opt.timeout;
    let (mut status, evidence, replayed, cleaned) = if opencode {
        run_opencode(&opt, &cwd, deadline).await
    } else {
        let (status, evidence, cleaned) = run_scenario(&opt, &cwd, deadline).await;
        (status, evidence, 0, cleaned)
    };
    let sessions_left = if opencode {
        opencode_cleanup(&opt, &cwd, &evidence.sessions).await
    } else {
        Some(Vec::new())
    };
    match &sessions_left {
        None if status == "passed" => status = "failed: cannot list opencode sessions".into(),
        Some(left) if status == "passed" && !left.is_empty() => {
            status = "failed: sessions left in opencode history".into();
        }
        _ => {}
    }
    let fixture_write = fixture_written(&cwd);
    if status == "passed" && opt.scenario == "approval-allow" && !fixture_write {
        status = "failed: approved fixture write not observed".into();
    }
    if status == "passed"
        && (opt.scenario == "approval-deny" || opt.scenario == "approval-interrupt")
        && cwd.join("probe.out").exists()
    {
        status = "failed: denied or interrupted fixture write occurred".into();
    }
    let mut record = json!({"engine":opt.engine.as_str(),"scenario":opt.scenario,"version":version(&opt.binary).await,"host":env::consts::OS,"arch":env::consts::ARCH,"status":status,"events":evidence.events,"approval_requests":evidence.approval_requests,"mcp_calls":evidence.mcp_calls,"hook_calls":evidence.hook_calls,"compact_boundaries":evidence.compact_boundaries,"agent_messages":evidence.agent_messages,"late_usage_turns":evidence.late_usage_turns,"tool_only_turns":evidence.tool_only_turns,"user_questions":evidence.user_questions,"failure_kind":evidence.failure_kind,"fixture_write":fixture_write,"child_cleanup":cleaned,"elapsed_ms":started.elapsed().as_millis()});
    if opencode {
        record["sessions"] = json!(evidence.sessions);
        record["sessions_left"] = json!(sessions_left);
        record["stop_reasons"] = json!(evidence.stop_reasons);
        record["replayed_updates"] = json!(replayed);
    }
    let serialized = record.to_string();
    let serialized = serde_json::to_string_pretty(&record).unwrap_or(serialized);
    if let Some(path) = &opt.output
        && fs::write(path, &serialized).is_err()
    {
        eprintln!("cannot write evidence");
        return 2;
    }
    println!("{serialized}");
    i32::from(!status.starts_with("passed"))
}

#[tokio::main]
async fn main() {
    let code = run().await;
    std::process::exit(code);
}
