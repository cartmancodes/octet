//! The fake opencode agent (`opencode acp`): the Agent Client Protocol's
//! `initialize`, `session/*` requests, with the scripts in `scenario` chosen by
//! the prompt's text or the model the session was given.
use crate::wire::{acp_error, acp_result, acp_text, acp_update, emit};
use octet_testkit::scenario;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, BufRead},
    process, thread,
    time::Duration,
};

/// The model a session starts with.
const MODEL_FREE: &str = "opencode/fixture-free";
/// The agent (Octet calls it the mode) a session starts with.
const MODE_BUILD: &str = "build";
/// The effort levels `MODEL_FREE` offers, as opencode lists a model's
/// variants; the first is its default. Other models offer none.
const FREE_EFFORTS: [&str; 3] = ["low", "high", "max"];

/// One opencode session's state; the fake serves one session per process.
struct OpencodeFixture {
    /// The session ID every notification names.
    session: String,
    /// Which request opened the session (`session/new`, `/resume` or `/fork`), for `PARAMS`.
    opened: String,
    /// The current model.
    model: String,
    /// The session's mode (opencode's `modeId`): the agent the next prompt
    /// runs under. As in opencode 1.18.25, a switch does not reach a prompt
    /// already running.
    mode: String,
    /// Every mode accepted so far, for `PARAMS`.
    modes: Vec<String>,
    /// The agent each prompt ran under, in order, for `PARAMS`.
    agents: Vec<String>,
    /// The current effort level; `None` for a model without levels.
    effort: Option<String>,
    /// Every effort level asked for so far, accepted or not, for `PARAMS`.
    efforts: Vec<String>,
    /// The model was set to `OPENCODE_REJECT_MODE`: mode switches are refused.
    rejects_modes: bool,
    /// The one mode switch (the handshake's) accepted after the model.
    free_mode_switch: bool,
    /// The ID of the prompt still running: held, or waiting for approvals.
    held: Option<Value>,
    /// Permission requests not yet answered.
    awaiting: usize,
    /// The pending permission requests are `APPROVALS_9`'s.
    approvals_9: bool,
    /// A cancel asks for a permission before it ends the held prompt
    /// (`OPENCODE_PERMISSION_ON_CANCEL`).
    asks_on_cancel: bool,
    /// An allowed edit is followed by `fs/write_text_file` (`OPENCODE_EDIT`).
    writes_on_allow: bool,
    /// The `fs/write_text_file` request waits for Octet's reply.
    fs_pending: bool,
}

pub(crate) fn run() {
    let mut fixture = OpencodeFixture {
        session: String::new(),
        opened: String::new(),
        model: MODEL_FREE.to_owned(),
        mode: MODE_BUILD.to_owned(),
        modes: Vec::new(),
        agents: Vec::new(),
        effort: Some(FREE_EFFORTS[0].to_owned()),
        efforts: Vec::new(),
        rejects_modes: false,
        free_mode_switch: false,
        held: None,
        awaiting: 0,
        approvals_9: false,
        asks_on_cancel: false,
        writes_on_allow: false,
        fs_pending: false,
    };
    for line in io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match v["method"].as_str() {
            Some("initialize") => emit(&acp_result(
                &v["id"],
                &json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true},"sessionCapabilities":{"resume":{},"fork":{},"close":{}}}}),
            )),
            Some(method @ ("session/new" | "session/resume" | "session/fork")) => {
                fixture.open(method, &v);
            }
            Some("session/set_config_option") => fixture.set_config_option(&v),
            Some("session/prompt") => fixture.prompt(&v),
            Some("session/cancel") => fixture.cancel(),
            Some("session/close") => emit(&acp_result(&v["id"], &json!({}))),
            Some(_) if v.get("id").is_some() => {
                emit(&acp_error(&v["id"], -32601, "Method not found"));
            }
            Some(_) => {}
            None => fixture.answer(&v),
        }
    }
}

/// `protocol-child session delete <id>`: the stand-in for `opencode session
/// delete <id>`, leaving an empty `deleted-<id>` file in the working directory.
pub(crate) fn session_command() {
    let mut args = env::args().skip(2);
    match (args.next().as_deref(), args.next()) {
        (Some("delete"), Some(id)) => fs::write(format!("deleted-{id}"), "").unwrap(),
        _ => process::exit(2),
    }
}

/// `OPENCODE_GATED`'s gate: says the request for `option` came, then waits
/// (at most ten seconds) for the test to release it.
fn gate(option: &str) {
    fs::write(format!("asked-{option}"), "").unwrap();
    let release = format!("release-{option}");
    for _ in 0..1000 {
        if fs::exists(&release).unwrap_or(false) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// A `tool_call` update for the pending `echo fixture` command `call`.
fn tool_call(sid: &str, call: &str) -> Value {
    acp_update(
        sid,
        &json!({"sessionUpdate":"tool_call","toolCallId":call,"title":"echo fixture","kind":"execute","status":"pending","locations":[],"rawInput":{}}),
    )
}

/// A `session/request_permission` request, numbered `id`, for the command `call`.
fn permission(sid: &str, id: usize, call: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":sid,"toolCall":{"toolCallId":call,"title":"echo fixture","kind":"execute","status":"pending","locations":[],"rawInput":{"command":"echo fixture"}},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"always","kind":"allow_always","name":"Always allow"},{"optionId":"reject","kind":"reject_once","name":"Reject"}]}})
}

/// The wire ID of the fake's `fs/write_text_file` request.
const FS_REQUEST_ID: u64 = 100;

/// The file `OPENCODE_EDIT` edits, in the working directory.
fn edit_path() -> String {
    env::current_dir()
        .unwrap()
        .join("fixture.txt")
        .display()
        .to_string()
}

/// A `session/request_permission` request, numbered 0, to edit `path` in
/// the tool call `call`, as opencode 1.18.25 sends it.
fn edit_permission(sid: &str, call: &str, path: &str) -> Value {
    json!({"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":sid,"toolCall":{"toolCallId":call,"title":path,"kind":"edit","status":"pending","locations":[{"path":path}],"rawInput":{"filepath":path,"diff":"+hi"}},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"reject","kind":"reject_once","name":"Reject"}]}})
}

/// The end of a turn.
fn end_turn(id: &Value) {
    emit(&acp_result(
        id,
        &json!({"stopReason":"end_turn","usage":{"totalTokens":42}}),
    ));
}

impl OpencodeFixture {
    /// The `model`, `mode` and (for a model with levels) `effort`
    /// selectors, with their current values.
    fn config_options(&self) -> Value {
        let mode = |value: &str, description: &str| json!({"value":value,"name":value,"description":description});
        let mut options = json!([
            {"id":"model","name":"Model","category":"model","type":"select","currentValue":self.model,"options":[
                {"value":MODEL_FREE,"name":"OpenCode/Fixture Free"},
                {"value":"openai/gpt-fixture","name":"OpenAI/GPT Fixture"}]},
            {"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":self.mode,"options":[
                mode("build", "The default agent"),
                mode("plan", "Plan without editing"),
                mode("octet-ask", "Ask before each tool"),
                mode("octet-accept-edits", "Accept edits, ask for the rest"),
                mode("octet-full-access", "Allow every tool")]}
        ]);
        if let (Some(current), Some(list)) = (&self.effort, options.as_array_mut()) {
            let levels: Vec<Value> = FREE_EFFORTS
                .iter()
                .map(|level| json!({"value":level,"name":level}))
                .collect();
            list.push(json!({"id":"effort","name":"Effort","category":"thought_level","type":"select","currentValue":current,"options":levels}));
        }
        options
    }

    /// Opens, resumes or forks the session. A resume replays its history
    /// after its reply, before any prompt, as opencode 1.18.25 does.
    fn open(&mut self, method: &str, v: &Value) {
        method.clone_into(&mut self.opened);
        self.session = match method {
            "session/resume" => v["params"]["sessionId"]
                .as_str()
                .unwrap_or("fixture-ses")
                .to_owned(),
            "session/fork" => "fixture-fork".to_owned(),
            _ => "fixture-ses".to_owned(),
        };
        let sid = &self.session;
        emit(&acp_result(
            &v["id"],
            &json!({"sessionId":sid,"configOptions":self.config_options()}),
        ));
        if method == "session/resume" {
            emit(&acp_update(
                sid,
                &json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"earlier question"}}),
            ));
            emit(&acp_text(sid, "REPLAYED"));
            emit(&tool_call(sid, "replayed-call"));
        }
        // As in opencode 1.18.25, `/compact` is not listed, though it runs.
        emit(&acp_update(
            sid,
            &json!({"sessionUpdate":"available_commands_update","availableCommands":[
                {"name":scenario::OPENCODE_SKILL,"description":"An opencode fixture skill"},
                {"name":scenario::OPENCODE_SUBTASK,"description":"An opencode fixture subtask"},
                {"name":"review","description":"review changes [commit|branch|pr], defaults to uncommitted"}]}),
        ));
    }

    /// Accepts any model; refuses modes once the model is `OPENCODE_REJECT_MODE`,
    /// except the one right after it.
    fn set_config_option(&mut self, v: &Value) {
        let value = v["params"]["value"].as_str().unwrap_or("");
        if let Some(option @ ("effort" | "mode")) = v["params"]["configId"].as_str()
            && self.model == scenario::OPENCODE_GATED
        {
            gate(option);
        }
        match v["params"]["configId"].as_str() {
            Some("model") => {
                value.clone_into(&mut self.model);
                // A model's effort starts at its default, as in opencode.
                self.effort = (value == MODEL_FREE || value == scenario::OPENCODE_GATED)
                    .then(|| FREE_EFFORTS[0].to_owned());
                if value == scenario::OPENCODE_REJECT_MODE && !self.rejects_modes {
                    self.rejects_modes = true;
                    self.free_mode_switch = true;
                }
            }
            Some("effort") => {
                self.efforts.push(value.to_owned());
                if self.effort.is_none() || !FREE_EFFORTS.contains(&value) {
                    emit(&acp_error(
                        &v["id"],
                        -32602,
                        &format!("Invalid params: effort not found: {value}"),
                    ));
                    return;
                }
                self.effort = Some(value.to_owned());
            }
            Some("mode") => {
                if self.rejects_modes && !std::mem::take(&mut self.free_mode_switch) {
                    emit(&acp_error(&v["id"], -32602, "unknown agent"));
                    return;
                }
                value.clone_into(&mut self.mode);
                self.modes.push(value.to_owned());
            }
            _ => {
                emit(&acp_error(&v["id"], -32602, "unknown config option"));
                return;
            }
        }
        emit(&acp_result(
            &v["id"],
            &json!({"configOptions":self.config_options()}),
        ));
    }

    /// Runs the script the prompt's first text block names, under the
    /// session's agent as it is now.
    fn prompt(&mut self, v: &Value) {
        let id = &v["id"];
        self.agents.push(self.mode.clone());
        let sid = self.session.clone();
        let blocks = v
            .pointer("/params/prompt")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let text = blocks
            .iter()
            .find(|block| block["type"] == "text")
            .and_then(|block| block["text"].as_str())
            .unwrap_or("");
        // Images are echoed so tests can check what was sent.
        let images: Vec<String> = blocks
            .iter()
            .filter(|block| block["type"] == "image")
            .map(|block| {
                let mime = block["mimeType"].as_str().unwrap_or("?");
                let length = block["data"].as_str().map_or(0, str::len);
                format!("image {mime} {length}")
            })
            .collect();
        if !images.is_empty() {
            emit(&acp_text(&sid, &images.join(",")));
            end_turn(id);
            return;
        }
        let subtask = format!("/{}", scenario::OPENCODE_SUBTASK);
        if text.split_whitespace().next() == Some(subtask.as_str()) {
            // The subagent's tool call starts; its approval requests never
            // reach the client, so the prompt waits for a cancel.
            self.held = Some(id.clone());
            emit(&acp_update(
                &sid,
                &json!({"sessionUpdate":"tool_call","toolCallId":"task-0","title":"task","kind":"think","status":"pending","locations":[],"rawInput":{"prompt":"Do the fixture subtask","description":"A fixture subtask","subagent_type":self.mode,"command":scenario::OPENCODE_SUBTASK}}),
            ));
            return;
        }
        // A slash command is echoed as the reply, as a skill's expansion would be.
        if text.starts_with('/') {
            emit(&acp_text(&sid, text));
            end_turn(id);
            return;
        }
        match text {
            scenario::HOLD => self.held = Some(id.clone()),
            scenario::OPENCODE_OTHER_PERMISSION => {
                self.held = Some(id.clone());
                self.awaiting = 1;
                self.approvals_9 = false;
                emit(&permission("other-session", 0, "other-call"));
            }
            scenario::OPENCODE_PERMISSION_ON_CANCEL => {
                self.held = Some(id.clone());
                self.asks_on_cancel = true;
            }
            scenario::OPENCODE_READ_ENV => {
                self.held = Some(id.clone());
                self.awaiting = 1;
                self.approvals_9 = false;
                emit(&acp_update(
                    &sid,
                    &json!({"sessionUpdate":"tool_call","toolCallId":"read-0","title":"read","kind":"read","status":"pending","locations":[],"rawInput":{}}),
                ));
                emit(&acp_update(
                    &sid,
                    &json!({"sessionUpdate":"tool_call_update","toolCallId":"read-0","status":"in_progress","kind":"read","title":"read","locations":[{"path":"/w/.env"}],"rawInput":{"filePath":"/w/.env"}}),
                ));
                emit(
                    &json!({"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":sid,"toolCall":{"toolCallId":"read-0","title":"read","kind":"read","status":"pending","locations":[],"rawInput":{}},"options":[{"optionId":"once","kind":"allow_once","name":"Allow once"},{"optionId":"reject","kind":"reject_once","name":"Reject"}]}}),
                );
            }
            scenario::CHATTER => {
                // The turn goes silent while another session keeps talking.
                self.held = Some(id.clone());
                for _ in 0..8 {
                    thread::sleep(Duration::from_millis(300));
                    emit(&acp_text("other-session", "other"));
                    emit(&tool_call("other-session", "other-call"));
                }
            }
            scenario::APPROVAL => {
                self.held = Some(id.clone());
                self.awaiting = 1;
                self.approvals_9 = false;
                emit(&tool_call(&sid, "call-0"));
                emit(&permission(&sid, 0, "call-0"));
            }
            scenario::OPENCODE_EDIT => {
                self.held = Some(id.clone());
                self.awaiting = 1;
                self.approvals_9 = false;
                self.writes_on_allow = true;
                let path = edit_path();
                emit(&acp_update(
                    &sid,
                    &json!({"sessionUpdate":"tool_call","toolCallId":"edit-0","title":"write","kind":"edit","status":"pending","locations":[{"path":path}],"rawInput":{"filePath":path,"content":"hi"}}),
                ));
                emit(&edit_permission(&sid, "edit-0", &path));
            }
            scenario::APPROVALS_9 => {
                self.held = Some(id.clone());
                self.awaiting = 9;
                self.approvals_9 = true;
                for n in 0..9 {
                    let call = format!("call-{n}");
                    emit(&tool_call(&sid, &call));
                    emit(&permission(&sid, n, &call));
                }
            }
            scenario::TOOL_FAIL => {
                // As opencode 1.18.25 starts a `bash` call: its location is
                // the working directory, and the command comes later.
                emit(&acp_update(
                    &sid,
                    &json!({"sessionUpdate":"tool_call","toolCallId":"call-9","title":"bash","kind":"execute","status":"pending","locations":[{"path":"/tmp"}],"rawInput":{"cwd":"/tmp"}}),
                ));
                emit(&acp_update(
                    &sid,
                    &json!({"sessionUpdate":"tool_call_update","toolCallId":"call-9","status":"failed","kind":"execute","title":"false","locations":[{"path":"/tmp"}],"rawInput":{"command":"false"},"content":[{"type":"content","content":{"type":"text","text":"boom"}}]}),
                ));
                end_turn(id);
            }
            scenario::FAIL => emit(&acp_error(id, -32603, "Internal error: fixture failure")),
            scenario::PARAMS => {
                emit(&acp_text(&sid, &self.params()));
                end_turn(id);
            }
            _ => {
                emit(&acp_text(&sid, scenario::OPENCODE_REPLY));
                emit(&acp_update(
                    &sid,
                    &json!({"sessionUpdate":"usage_update","used":42,"size":200_000,"cost":{"amount":0,"currency":"USD"}}),
                ));
                end_turn(id);
            }
        }
    }

    /// `PARAMS`'s echo: the launch arguments and environment, and what the
    /// session was given so far.
    fn params(&self) -> String {
        let env_var = |name| env::var(name).ok();
        json!({
            "argv": env::args().skip(1).collect::<Vec<_>>(),
            "env": {
                "OPENCODE_CONFIG_CONTENT": env_var("OPENCODE_CONFIG_CONTENT"),
                "OPENCODE_PURE": env_var("OPENCODE_PURE"),
            },
            "modes": self.modes,
            "agents": self.agents,
            "effort": self.effort,
            "efforts": self.efforts,
            "model": self.model,
            "opened": self.opened,
        })
        .to_string()
    }

    /// Ends a held prompt, as opencode 1.18.25 does: with `end_turn`; or,
    /// for `OPENCODE_PERMISSION_ON_CANCEL`, asks for a permission first and
    /// ends it once that is answered.
    fn cancel(&mut self) {
        if std::mem::take(&mut self.asks_on_cancel) {
            self.awaiting = 1;
            self.approvals_9 = false;
            emit(&permission(&self.session, 0, "late-call"));
            return;
        }
        self.awaiting = 0;
        if let Some(id) = self.held.take() {
            end_turn(&id);
        }
    }

    /// Octet's answer to one of our permission requests, echoed as text; the
    /// last answer ends the turn.
    fn answer(&mut self, v: &Value) {
        if self.fs_pending && v["id"] == FS_REQUEST_ID {
            self.fs_pending = false;
            emit(&acp_text(
                &self.session,
                &format!("fs {}", v["error"]["code"]),
            ));
            if let Some(id) = self.held.take() {
                end_turn(&id);
            }
            return;
        }
        if self.awaiting == 0 {
            return;
        }
        let outcome = &v["result"]["outcome"];
        let choice = outcome["optionId"]
            .as_str()
            .or_else(|| outcome["outcome"].as_str())
            .unwrap_or("bad response");
        let reply = if self.approvals_9 {
            format!("answered {}:{choice}", v["id"])
        } else {
            choice.to_owned()
        };
        emit(&acp_text(&self.session, &reply));
        self.awaiting -= 1;
        if std::mem::take(&mut self.writes_on_allow) && choice == "once" {
            // opencode writes an allowed edit through the client as well.
            self.fs_pending = true;
            let params = json!({"sessionId":self.session,"path":edit_path(),"content":"hi\n"});
            emit(
                &json!({"jsonrpc":"2.0","id":FS_REQUEST_ID,"method":"fs/write_text_file","params":params}),
            );
            return;
        }
        if self.awaiting == 0
            && let Some(id) = self.held.take()
        {
            end_turn(&id);
        }
    }
}
