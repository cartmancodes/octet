//! The fake Antigravity agent (`agy acp --cli_dir_name <folder>`): the Agent
//! Client Protocol's `initialize` and `session/*` requests, with the scripts in
//! `scenario` chosen by the prompt's first text block. At start it reads
//! `<folder>/settings.json` (its `toolPermission` decides whether a turn asks)
//! and, when the working directory has one, `fake-agy.json` (see [`Knobs`]).
use crate::wire::{acp_error, acp_result, acp_text, acp_update, emit};
use octet_testkit::scenario;
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    env, fs,
    io::{self, BufRead},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

/// The model a session starts with.
const MODEL_FLASH: &str = "gemini-fixture-flash";
/// The model `AGY_NEW_MODEL` switches to.
const MODEL_CLAUDE: &str = "claude-fixture";
/// The model without a `thought_level` option.
const MODEL_PLAIN: &str = "plain-fixture";
/// The models offered, with their names.
const MODELS: [(&str, &str); 3] = [
    (MODEL_FLASH, "Gemini Fixture Flash"),
    (MODEL_CLAUDE, "Claude Fixture"),
    (MODEL_PLAIN, "Plain Fixture"),
];
/// The thinking levels offered.
const LEVELS: [&str; 3] = ["low", "medium", "high"];
/// The thinking level a session starts at.
const LEVEL_DEFAULT: &str = "high";
/// The modes offered, with their names; the first is the one a session starts in.
const MODES: [(&str, &str); 4] = [
    ("default", "Default"),
    ("accept-edits", "Accept Edits"),
    ("auto", "Auto"),
    ("plan", "Plan"),
];
/// The ID of the session `session/new` opens.
const SESSION: &str = "fixture-agy";
/// The `toolPermission` under which nothing asks.
const ALWAYS_PROCEED: &str = "always-proceed";
/// How long `AGY_UNASKED` and `AGY_FLOOD` wait after their last tool frame for a
/// `session/cancel` before the turn goes on. Octet cancels when it sees the
/// unasked action complete; the wait makes that cancel land inside the turn
/// instead of racing the end of it.
const CANCEL_WINDOW: Duration = Duration::from_millis(300);
/// The ID of the permission request most scripts send.
const REQUEST_ID: &str = "1";

/// What `fake-agy.json` in the working directory can set; a missing file, or a
/// missing key, leaves the default.
struct Knobs {
    /// The version `initialize` reports (default `1.5.5`).
    version: String,
    /// `session/new` and `session/resume` answer "Authentication required".
    signed_out: bool,
    /// Every `mode` request after the first is refused as an unknown mode.
    reject_switches: bool,
    /// Every `mode` request after the first gets no reply.
    hang_switches: bool,
    /// Every `mode` request after the first gets `{}` and no update.
    bare_switch_reply: bool,
    /// `compact` is not among the commands.
    no_compact: bool,
    /// `.agents/hooks.json` (`{}`) is written as the fake starts.
    hooks_at_start: bool,
}

impl Knobs {
    /// Reads `fake-agy.json` from the working directory.
    fn read() -> Self {
        let file = fs::read_to_string("fake-agy.json")
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let flag = |key: &str| file[key].as_bool().unwrap_or(false);
        Self {
            version: file["version"].as_str().unwrap_or("1.5.5").to_owned(),
            signed_out: flag("signedOut"),
            reject_switches: flag("rejectSwitches"),
            hang_switches: flag("hangSwitches"),
            bare_switch_reply: flag("bareSwitchReply"),
            no_compact: flag("noCompact"),
            hooks_at_start: flag("hooksAtStart"),
        }
    }
}

/// A tool call a script runs.
struct Tool {
    /// The tool call's ID.
    call: String,
    /// The tool's name.
    name: &'static str,
    /// The tool's kind.
    kind: &'static str,
    /// The title shown for the call.
    title: &'static str,
    /// The tool's input.
    input: Value,
    /// The files it touches; only the edit names any.
    locations: Option<Value>,
}

impl Tool {
    /// The command `echo fixture`, as the call `call`.
    fn command(call: &str) -> Self {
        Self {
            call: call.to_owned(),
            name: "run_command",
            kind: "execute",
            title: "echo fixture",
            input: json!({"command":"echo fixture"}),
            locations: None,
        }
    }

    /// The write of `fixture.txt` in the working directory, as `call-1`.
    fn edit() -> Self {
        let path = env::current_dir()
            .unwrap()
            .join("fixture.txt")
            .display()
            .to_string();
        Self {
            call: "call-1".to_owned(),
            name: "write_file",
            kind: "edit",
            title: "File fixture.txt",
            input: json!({"path":path,"content":"fixture\n"}),
            locations: Some(json!([{"path":path}])),
        }
    }

    /// The read of a file, as the call `call-<n>`.
    fn read(n: usize) -> Self {
        Self {
            call: format!("call-{n}"),
            name: "read_file",
            kind: "read",
            title: "Read file",
            input: json!({"path":"fixture.txt"}),
            locations: None,
        }
    }

    /// The subagent invocation, as `call-1`.
    fn subagent() -> Self {
        Self {
            call: "call-1".to_owned(),
            name: "invoke_subagent",
            kind: "other",
            title: "Invoke subagent",
            input: json!({}),
            locations: None,
        }
    }

    /// The call's fields, as a permission request and a `tool_call` carry them.
    fn fields(&self, status: &str) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("toolCallId".to_owned(), json!(self.call));
        fields.insert("name".to_owned(), json!(self.name));
        fields.insert("kind".to_owned(), json!(self.kind));
        fields.insert("title".to_owned(), json!(self.title));
        fields.insert("rawInput".to_owned(), self.input.clone());
        fields.insert("status".to_owned(), json!(status));
        if let Some(locations) = &self.locations {
            fields.insert("locations".to_owned(), locations.clone());
        }
        fields
    }

    /// The `tool_call` update that announces the call with `status`.
    fn started(&self, sid: &str, status: &str) -> Value {
        let mut fields = self.fields(status);
        fields.insert("sessionUpdate".to_owned(), json!("tool_call"));
        acp_update(sid, &Value::Object(fields))
    }

    /// A `session/request_permission` request, numbered `id`, for this call.
    fn request(&self, sid: &str, id: &str) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{
            "sessionId":sid,
            "toolCall":self.fields("pending"),
            "options":[
                {"optionId":"allow","name":"Allow","kind":"allow_once"},
                {"optionId":"deny","name":"Deny","kind":"reject_once"}]}})
    }
}

/// What the running prompt is waiting for.
enum Turn {
    /// `HOLD`: a `session/cancel`.
    Held {
        /// The prompt's request ID.
        id: Value,
    },
    /// `APPROVAL` or `AGY_EDIT`: the reply to the request `"1"`.
    Asked {
        /// The prompt's request ID.
        id: Value,
        /// The tool call that asked.
        tool: Tool,
    },
    /// `APPROVALS_9`: the replies to the requests not yet answered.
    Many {
        /// The prompt's request ID.
        id: Value,
        /// The IDs of the requests not yet answered.
        outstanding: Vec<String>,
    },
    /// `AGY_SLOW_EDIT`: the next `session/set_config_option`.
    SlowEdit {
        /// The prompt's request ID.
        id: Value,
    },
    /// `AGY_TO_AUTO`: a `session/cancel`.
    ToAuto {
        /// The prompt's request ID.
        id: Value,
    },
    /// `AGY_TO_AUTO`, cancelled: the next `session/set_config_option`.
    ToAutoLate {
        /// The prompt's request ID.
        id: Value,
    },
    /// `AGY_LATE_COMMAND`: the next `session/set_config_option`.
    LateCommand {
        /// The prompt's request ID.
        id: Value,
    },
    /// `AGY_ASK_ON_CANCEL`: a `session/cancel`.
    AskOnCancel {
        /// The prompt's request ID.
        id: Value,
    },
    /// `AGY_ASK_ON_CANCEL`, cancelled: the reply to the request `"1"`.
    AskedOnCancel {
        /// The prompt's request ID.
        id: Value,
    },
}

/// One Antigravity process; it serves one session at a time.
struct AgyFixture {
    /// What `fake-agy.json` set.
    knobs: Knobs,
    /// The parsed `settings.json`; `Null` when it is missing or not an object.
    settings: Value,
    /// The `toolPermission` is `always-proceed`.
    always_proceed: bool,
    /// The session ID every notification names.
    session: String,
    /// Which request opened the session (`session/new` or `/resume`), for `PARAMS`.
    opened: String,
    /// The current model.
    model: String,
    /// The current thinking level; `None` for a model without levels.
    thought: Option<String>,
    /// The session's mode.
    mode: String,
    /// Every `[configId, value]` set so far, accepted or not, for `PARAMS`.
    sets: Vec<[String; 2]>,
    /// The `session/prompt` requests received so far, for `PARAMS`.
    prompts: usize,
    /// The `mode` requests received so far, across sessions.
    mode_switches: usize,
    /// What the running prompt waits for, if one runs.
    turn: Option<Turn>,
    /// The frames the reader thread sends.
    frames: Receiver<String>,
    /// Frames read ahead while a script looked for a `session/cancel`.
    backlog: VecDeque<Value>,
}

/// The value after the launch argument `name`, if it was given.
fn flag_value(name: &str) -> Option<String> {
    let mut args = env::args();
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}

/// The frame a line holds; `None` for a blank one.
fn parse(line: &str) -> Option<Value> {
    (!line.trim().is_empty()).then(|| serde_json::from_str(line).unwrap())
}

/// `agy acp --cli_dir_name <folder> [--gemini-dir <folder>]`.
pub(crate) fn run() {
    let knobs = Knobs::read();
    if knobs.hooks_at_start {
        fs::create_dir_all(".agents").unwrap();
        fs::write(".agents/hooks.json", "{}").unwrap();
    }
    let settings = flag_value("--cli_dir_name")
        .and_then(|dir| fs::read_to_string(format!("{dir}/settings.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null);
    let (sender, frames) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut fixture = AgyFixture {
        always_proceed: settings["toolPermission"] == ALWAYS_PROCEED,
        knobs,
        settings,
        session: String::new(),
        opened: String::new(),
        model: MODEL_FLASH.to_owned(),
        thought: Some(LEVEL_DEFAULT.to_owned()),
        mode: MODES[0].0.to_owned(),
        sets: Vec::new(),
        prompts: 0,
        mode_switches: 0,
        turn: None,
        frames,
        backlog: VecDeque::new(),
    };
    while let Some(v) = fixture.next_frame() {
        fixture.handle(&v);
    }
    if let Some(dir) = flag_value("--gemini-dir") {
        // The CLI leaves something of its own in the folder; a failure to
        // write the marker is of no interest.
        drop(fs::write(format!("{dir}/left-behind"), ""));
    }
}

/// The reply to `initialize`, naming `version`.
fn initialized(version: &str) -> Value {
    json!({
        "protocolVersion":1,
        "agentInfo":{"name":"antigravity","version":version},
        "agentCapabilities":{
            "loadSession":true,
            "promptCapabilities":{"image":true,"audio":false,"embeddedContext":true},
            "sessionCapabilities":{"resume":{},"close":{},"load":{},"list":{}}}})
}

/// One `select` config option.
fn select(id: &str, name: &str, current: &str, options: &[Value]) -> Value {
    json!({"id":id,"name":name,"category":id,"type":"select","currentValue":current,"options":options})
}

/// A turn that ended: the prompt `id` answered with `stopReason`.
fn stop(id: &Value, reason: &str) {
    emit(&acp_result(id, &json!({"stopReason":reason})));
}

/// The end of a turn.
fn end_turn(id: &Value) {
    emit(&acp_result(
        id,
        &json!({"stopReason":"end_turn","usage":{"inputTokens":40,"outputTokens":2,"totalTokens":42}}),
    ));
}

/// A `tool_call_update` that carries only the call's ID and its `status`.
fn tool_status(sid: &str, call: &str, status: &str) -> Value {
    acp_update(
        sid,
        &json!({"sessionUpdate":"tool_call_update","toolCallId":call,"status":status}),
    )
}

/// A `user_message_chunk` echoing `text`.
fn user_text(sid: &str, text: &str) -> Value {
    acp_update(
        sid,
        &json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":text}}),
    )
}

/// What a permission reply chose: the option, or else the outcome.
fn choice(reply: &Value) -> &str {
    let outcome = &reply["result"]["outcome"];
    outcome["optionId"]
        .as_str()
        .or_else(|| outcome["outcome"].as_str())
        .unwrap_or("bad response")
}

impl AgyFixture {
    /// The next frame: one read ahead, or else the next one on stdin; `None`
    /// once stdin closes.
    fn next_frame(&mut self) -> Option<Value> {
        if let Some(frame) = self.backlog.pop_front() {
            return Some(frame);
        }
        loop {
            let line = self.frames.recv().ok()?;
            if let Some(frame) = parse(&line) {
                return Some(frame);
            }
        }
    }

    /// Whether a `session/cancel` has arrived; it is taken off the line, and
    /// every other frame that came with it stays for the loop.
    fn cancel_arrived(&mut self) -> bool {
        while let Ok(line) = self.frames.try_recv() {
            self.backlog.extend(parse(&line));
        }
        let cancel = self
            .backlog
            .iter()
            .position(|frame| frame["method"] == "session/cancel");
        cancel.is_some_and(|at| self.backlog.remove(at).is_some())
    }

    /// Emits `frames` in order, unless a `session/cancel` arrives first: that
    /// ends the prompt `id` as `cancelled` (`false`). The caller ends it
    /// otherwise.
    fn stream(&mut self, id: &Value, frames: &[Value]) -> bool {
        for frame in frames {
            if self.cancel_arrived() {
                stop(id, "cancelled");
                return false;
            }
            emit(frame);
        }
        if self.cancel_arrived() {
            stop(id, "cancelled");
            return false;
        }
        true
    }

    /// Waits up to `CANCEL_WINDOW` for a `session/cancel`, or takes one that
    /// has already arrived; whether there was one. Other frames that come
    /// meanwhile stay in the backlog, in order.
    fn cancel_within_window(&mut self) -> bool {
        if self.cancel_arrived() {
            return true;
        }
        let deadline = Instant::now() + CANCEL_WINDOW;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(line) = self.frames.recv_timeout(left) else {
                return false;
            };
            match parse(&line) {
                Some(frame) if frame["method"] == "session/cancel" => return true,
                Some(frame) => self.backlog.push_back(frame),
                None => {}
            }
        }
        false
    }

    /// Answers one frame.
    fn handle(&mut self, v: &Value) {
        match v["method"].as_str() {
            Some("initialize") => emit(&acp_result(&v["id"], &initialized(&self.knobs.version))),
            Some(method @ ("session/new" | "session/resume")) => self.open(method, v),
            Some("session/set_config_option") => {
                self.set_config_option(v);
                self.after_set();
            }
            Some("session/prompt") => self.prompt(v),
            Some("session/cancel") => self.cancel(),
            Some("session/close") => emit(&acp_result(&v["id"], &json!({}))),
            // `session/fork` and every other method the CLI does not know.
            Some(_) if v.get("id").is_some() => {
                emit(&acp_error(&v["id"], -32601, "Method not found"));
            }
            Some(_) => {}
            None => self.answer(v),
        }
    }

    /// The `model`, `thought_level` (unless the model has none) and `mode`
    /// selectors, with their current values.
    fn config_options(&self) -> Value {
        let named = |pairs: &[(&str, &str)]| -> Vec<Value> {
            pairs
                .iter()
                .map(|(value, name)| json!({"value":value,"name":name}))
                .collect()
        };
        let levels: Vec<(&str, &str)> = LEVELS.iter().map(|level| (*level, *level)).collect();
        let mut options = vec![select("model", "Model", &self.model, &named(&MODELS))];
        if let Some(thought) = &self.thought {
            options.push(select(
                "thought_level",
                "Thinking Level",
                thought,
                &named(&levels),
            ));
        }
        options.push(select("mode", "Mode", &self.mode, &named(&MODES)));
        Value::Array(options)
    }

    /// The `config_option_update` that confirms the options as they are.
    fn announce(&self) {
        emit(&acp_update(
            &self.session,
            &json!({"sessionUpdate":"config_option_update","configOptions":self.config_options()}),
        ));
    }

    /// The `current_mode_update` that confirms the mode as it is.
    fn announce_mode(&self) {
        emit(&acp_update(
            &self.session,
            &json!({"sessionUpdate":"current_mode_update","currentModeId":self.mode}),
        ));
    }

    /// The agent switches itself to `mode`, and says so.
    fn switch_mode(&mut self, mode: &str) {
        mode.clone_into(&mut self.mode);
        self.announce();
        self.announce_mode();
    }

    /// Makes `model` the current one; a model without levels drops the level,
    /// and one with levels brings it back at the default.
    fn use_model(&mut self, model: &str) {
        model.clone_into(&mut self.model);
        if model == MODEL_PLAIN {
            self.thought = None;
        } else if self.thought.is_none() {
            self.thought = Some(LEVEL_DEFAULT.to_owned());
        }
    }

    /// Opens or resumes a session, and lists its commands. A session starts in
    /// mode `default` at the default level, a resumed one too.
    fn open(&mut self, method: &str, v: &Value) {
        if self.knobs.signed_out {
            emit(&acp_error(&v["id"], -32000, "Authentication required"));
            return;
        }
        let resumed = (method == "session/resume")
            .then(|| v["params"]["sessionId"].as_str().unwrap_or(SESSION));
        if resumed == Some(scenario::AGY_MISSING_SESSION) {
            let message = format!("Failed to read session {}", scenario::AGY_MISSING_SESSION);
            emit(&acp_error(&v["id"], -32603, &message));
            return;
        }
        method.clone_into(&mut self.opened);
        resumed.unwrap_or(SESSION).clone_into(&mut self.session);
        MODES[0].0.clone_into(&mut self.mode);
        self.thought = (self.model != MODEL_PLAIN).then(|| LEVEL_DEFAULT.to_owned());
        emit(&acp_result(
            &v["id"],
            &json!({"sessionId":self.session,"configOptions":self.config_options()}),
        ));
        let mut commands =
            vec![json!({"name":scenario::AGY_SKILL,"description":"An Antigravity fixture skill"})];
        if !self.knobs.no_compact {
            commands.push(json!({"name":"compact","description":"Compact the conversation"}));
        }
        commands.push(json!({"name":"goal","description":"Set a goal for the agent"}));
        commands.push(json!({"name":"plan","description":"Plan before editing"}));
        emit(&acp_update(
            &self.session,
            &json!({"sessionUpdate":"available_commands_update","availableCommands":commands}),
        ));
    }

    /// Sets a model, a thinking level or a mode, and says so. Mode requests
    /// after the first can be refused, ignored or answered bare, by `Knobs`.
    fn set_config_option(&mut self, v: &Value) {
        let id = &v["id"];
        let config = v["params"]["configId"].as_str().unwrap_or("");
        let value = v["params"]["value"].as_str().unwrap_or("");
        self.sets.push([config.to_owned(), value.to_owned()]);
        let unknown =
            |what: &str| emit(&acp_error(id, -32602, &format!("Unknown {what}: {value}")));
        match config {
            "mode" => {
                self.mode_switches += 1;
                let later = self.mode_switches > 1;
                if later && self.knobs.hang_switches {
                    return;
                }
                if later && self.knobs.bare_switch_reply {
                    emit(&acp_result(id, &json!({})));
                    return;
                }
                if !MODES.iter().any(|(mode, _)| *mode == value)
                    || (later && self.knobs.reject_switches)
                {
                    unknown("mode");
                    return;
                }
                value.clone_into(&mut self.mode);
            }
            "model" if MODELS.iter().any(|(model, _)| *model == value) => self.use_model(value),
            "model" => return unknown("model"),
            "thought_level" if LEVELS.contains(&value) => self.thought = Some(value.to_owned()),
            "thought_level" => return unknown("thought level"),
            _ => return unknown("config option"),
        }
        emit(&acp_result(
            id,
            &json!({"configOptions":self.config_options()}),
        ));
        self.announce();
        if config == "mode" {
            self.announce_mode();
        }
    }

    /// A `session/set_config_option` arrived: the prompts that waited for one go on.
    fn after_set(&mut self) {
        match self.turn.take() {
            Some(Turn::SlowEdit { id }) => {
                emit(&tool_status(&self.session, "call-1", "completed"));
                end_turn(&id);
            }
            Some(Turn::LateCommand { id }) => self.unasked(&id),
            Some(Turn::ToAutoLate { id }) => {
                emit(&acp_text(&self.session, "late"));
                stop(&id, "cancelled");
            }
            other => self.turn = other,
        }
    }

    /// Whether a turn asks before the tool: not under `always-proceed`, in mode
    /// `default`, and for a command in `accept-edits` too.
    fn asks(&self, command: bool) -> bool {
        !self.always_proceed && (self.mode == "default" || (command && self.mode == "accept-edits"))
    }

    /// Runs the script the prompt's first text block names.
    fn prompt(&mut self, v: &Value) {
        let id = v["id"].clone();
        let sid = self.session.clone();
        let before = self.prompts;
        self.prompts += 1;
        let blocks = v
            .pointer("/params/prompt")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let text = blocks
            .iter()
            .find(|block| block["type"] == "text")
            .and_then(|block| block["text"].as_str())
            .unwrap_or("");
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
            end_turn(&id);
            return;
        }
        match text {
            scenario::HOLD => self.turn = Some(Turn::Held { id }),
            scenario::APPROVAL => self.tool_turn(&id, Tool::command("call-1")),
            scenario::AGY_EDIT => self.tool_turn(&id, Tool::edit()),
            scenario::APPROVALS_9 => self.nine_approvals(id),
            scenario::TOOL_FAIL => {
                emit(&Tool::command("call-1").started(&sid, "in_progress"));
                emit(&tool_status(&sid, "call-1", "failed"));
                end_turn(&id);
            }
            scenario::FAIL => emit(&acp_error(&id, -32603, "Internal error: fixture failure")),
            scenario::AGY_UNASKED => self.unasked(&id),
            scenario::AGY_SLOW_EDIT => {
                emit(&Tool::edit().started(&sid, "in_progress"));
                self.turn = Some(Turn::SlowEdit { id });
            }
            scenario::AGY_TO_PLAN => {
                self.switch_mode("plan");
                emit(&acp_text(&sid, scenario::AGY_REPLY));
                end_turn(&id);
            }
            scenario::AGY_TO_AUTO => {
                self.switch_mode("auto");
                self.turn = Some(Turn::ToAuto { id });
            }
            scenario::AGY_IDLE_PLAN => {
                self.reply(&id, blocks);
                self.switch_mode("plan");
            }
            scenario::AGY_LATE_COMMAND => self.turn = Some(Turn::LateCommand { id }),
            scenario::AGY_ASK_ON_CANCEL => {
                let tool = Tool::command("call-1");
                emit(&tool.started(&sid, "in_progress"));
                emit(&tool_status(&sid, "call-1", "pending"));
                self.turn = Some(Turn::AskOnCancel { id });
            }
            scenario::AGY_FLOOD => self.flood(&id),
            scenario::AGY_NEW_MODEL => {
                self.use_model(MODEL_CLAUDE);
                self.announce();
                emit(&acp_text(&sid, scenario::AGY_REPLY));
                end_turn(&id);
            }
            scenario::AGY_SUBAGENT => {
                emit(&Tool::subagent().started(&sid, "in_progress"));
                emit(&tool_status(&sid, "call-1", "completed"));
                emit(&acp_text(&sid, scenario::AGY_REPLY));
                end_turn(&id);
            }
            scenario::AGY_OTHER_SESSION => {
                // The request is for a session this process does not serve,
                // and its reply is not waited for.
                emit(&Tool::command("call-other").request("other-ses", "other-1"));
                self.reply(&id, blocks);
            }
            scenario::PARAMS => {
                emit(&acp_text(&sid, &self.params(before)));
                end_turn(&id);
            }
            _ => self.reply(&id, blocks),
        }
    }

    /// The reply of a prompt with no script: the prompt echoed back, a thought
    /// and `AGY_REPLY` (several text blocks, joined by a newline, instead).
    fn reply(&self, id: &Value, blocks: &[Value]) {
        let texts: Vec<&str> = blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect();
        let joined = texts.join("\n");
        let message = if texts.len() > 1 {
            joined.as_str()
        } else {
            scenario::AGY_REPLY
        };
        emit(&user_text(&self.session, &joined));
        emit(&acp_update(
            &self.session,
            &json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Thinking"}}),
        ));
        emit(&acp_text(&self.session, message));
        end_turn(id);
    }

    /// `APPROVAL` and `AGY_EDIT`: the tool starts and, when it asks, waits for
    /// the answer; otherwise it runs.
    fn tool_turn(&mut self, id: &Value, tool: Tool) {
        let sid = self.session.clone();
        emit(&tool.started(&sid, "in_progress"));
        emit(&tool_status(&sid, &tool.call, "pending"));
        if self.asks(tool.kind == "execute") {
            emit(&tool.request(&sid, REQUEST_ID));
            self.turn = Some(Turn::Asked {
                id: id.clone(),
                tool,
            });
        } else {
            let text = if tool.kind == "edit" { "wrote" } else { "ran" };
            self.finish(id, &tool, "completed", text);
        }
    }

    /// The end of a tool turn: the call runs, ends with `status`, says `text`.
    fn finish(&self, id: &Value, tool: &Tool, status: &str, text: &str) {
        emit(&tool_status(&self.session, &tool.call, "in_progress"));
        emit(&tool_status(&self.session, &tool.call, status));
        emit(&acp_text(&self.session, text));
        end_turn(id);
    }

    /// `APPROVALS_9`: nine commands, each asking, numbered `"1"` … `"9"`.
    fn nine_approvals(&mut self, id: Value) {
        let mut outstanding = Vec::new();
        for n in 1..=9 {
            let tool = Tool::command(&format!("call-{n}"));
            emit(&tool.started(&self.session, "in_progress"));
            emit(&tool_status(&self.session, &tool.call, "pending"));
            emit(&tool.request(&self.session, &n.to_string()));
            outstanding.push(n.to_string());
        }
        self.turn = Some(Turn::Many { id, outstanding });
    }

    /// `AGY_UNASKED`: the command runs without asking, whatever the mode. A
    /// `session/cancel` during the turn, or within `CANCEL_WINDOW` of the
    /// `completed` frame, ends it `cancelled` with no text.
    fn unasked(&mut self, id: &Value) {
        let sid = self.session.clone();
        let frames = [
            Tool::command("call-1").started(&sid, "in_progress"),
            tool_status(&sid, "call-1", "pending"),
            tool_status(&sid, "call-1", "in_progress"),
            tool_status(&sid, "call-1", "completed"),
        ];
        self.run_to_end(id, &frames, "ran unasked");
    }

    /// `AGY_FLOOD`: 1,025 reads, each started then completed; cancelled as
    /// `unasked` is.
    fn flood(&mut self, id: &Value) {
        let sid = self.session.clone();
        let mut frames = Vec::new();
        for n in 1..=1025 {
            frames.push(Tool::read(n).started(&sid, "in_progress"));
            frames.push(tool_status(&sid, &format!("call-{n}"), "completed"));
        }
        self.run_to_end(id, &frames, scenario::AGY_REPLY);
    }

    /// Streams the tool `frames`, leaves `CANCEL_WINDOW` for a cancel, then
    /// says `text` and ends the prompt `id`; a cancel ends it `cancelled`
    /// with no further frame.
    fn run_to_end(&mut self, id: &Value, frames: &[Value], text: &str) {
        if !self.stream(id, frames) {
            return;
        }
        if self.cancel_within_window() {
            stop(id, "cancelled");
            return;
        }
        emit(&acp_text(&self.session, text));
        end_turn(id);
    }

    /// `PARAMS`'s echo: the launch arguments, the settings file and what the
    /// session was given so far; `before` prompts came before this one.
    fn params(&self, before: usize) -> String {
        json!({
            "argv": env::args().skip(1).collect::<Vec<_>>(),
            "settings": self.settings,
            "opened": self.opened,
            "sets": self.sets,
            "prompts": before,
        })
        .to_string()
    }

    /// A `session/cancel`: ends the prompt that waits for one, as the CLI does;
    /// `AGY_TO_AUTO` goes on to wait for a `session/set_config_option`, and
    /// `AGY_ASK_ON_CANCEL` asks first.
    fn cancel(&mut self) {
        match self.turn.take() {
            Some(
                Turn::Held { id }
                | Turn::SlowEdit { id }
                | Turn::LateCommand { id }
                | Turn::Asked { id, .. }
                | Turn::Many { id, .. },
            ) => stop(&id, "cancelled"),
            Some(Turn::ToAuto { id }) => self.turn = Some(Turn::ToAutoLate { id }),
            Some(Turn::AskOnCancel { id }) => {
                emit(&Tool::command("call-1").request(&self.session, REQUEST_ID));
                self.turn = Some(Turn::AskedOnCancel { id });
            }
            other => self.turn = other,
        }
    }

    /// A reply to a request of ours. One for a request nothing waits for, or
    /// for one other than the one a prompt waits for, is ignored.
    fn answer(&mut self, v: &Value) {
        match self.turn.take() {
            Some(Turn::Asked { id, tool }) if v["id"] == REQUEST_ID => {
                let chosen = choice(v);
                let status = if chosen == "allow" {
                    "completed"
                } else {
                    "failed"
                };
                self.finish(&id, &tool, status, chosen);
            }
            Some(Turn::Many {
                id,
                mut outstanding,
            }) => {
                if let Some(at) = outstanding
                    .iter()
                    .position(|asked| v["id"] == asked.as_str())
                {
                    outstanding.remove(at);
                    emit(&acp_text(
                        &self.session,
                        &format!("answered {}:{}", v["id"], choice(v)),
                    ));
                }
                if outstanding.is_empty() {
                    end_turn(&id);
                } else {
                    self.turn = Some(Turn::Many { id, outstanding });
                }
            }
            Some(Turn::AskedOnCancel { id }) if v["id"] == REQUEST_ID => {
                emit(&tool_status(&self.session, "call-1", "failed"));
                stop(&id, "cancelled");
            }
            other => self.turn = other,
        }
    }
}
