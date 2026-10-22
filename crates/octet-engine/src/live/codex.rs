//! Codex app-server JSON-RPC protocol: session setup, the model catalog,
//! server requests and turn notifications.
use super::{
    ApprovalSummary, BoxFuture, Channels, Config, DriverError, EVENT_BYTES, Event, ImageAttachment,
    Limits, Mode, ModelInfo, Outcome, Provider, SkillInfo, ToolCall, ToolPhase,
    activity::one_line,
    limited, model_catalog_with,
    protocol::{Core, Phase, Protocol},
    push_bounded, short, valid_effort, valid_identifier,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    fmt::Write as _,
    path::Path,
    time::Duration,
};

/// Codex's row in the provider table.
pub(super) const PROVIDER: Provider = Provider {
    name: "codex",
    title: "Codex",
    default_binary: "codex",
    offline: false,
    modes: [
        "Workspace sandbox; untrusted commands ask (untrusted)",
        "Workspace sandbox; asks only to escalate (on-request). Codex has no edits-only mode",
        "Workspace sandbox; Codex's auto-review agent decides escalations (auto_review)",
        "No sandbox; never asks (danger-full-access)",
    ],
    steer: true,
    mode_applies_next_turn: true,
    inline_images: false,
    effort_live: true,
    takes_effort: true,
    ids_carry_name: false,
    efforts: &[],
    shutdown_grace: Duration::from_millis(150),
    lists_models: true,
    launch_args: <CodexProtocol as Protocol>::launch_args,
    launch_env: super::no_env,
    start,
};

fn start(config: Config, limits: Limits, channels: Channels) -> BoxFuture<Result<(), String>> {
    Box::pin(async move { super::driver::run::<CodexProtocol>(config, limits, channels).await })
}

fn answer_wire(wire: &Value, allow: bool) -> Value {
    json!({"id":wire["id"],"result":{"decision":if allow {"accept"} else {"decline"}}})
}

/// What a reply from Codex answers: one of Octet's own requests.
enum Outstanding {
    Initialize,
    OpenThread,
    ModelList,
    SkillsList,
    /// `config/read`: the reasoning effort Codex's config names.
    ConfigRead,
    /// `turn/start` or `thread/compact/start`: an error fails the turn.
    Start,
    Interrupt,
    /// A steer, kept so a refusal can say which text was lost.
    Steer(String),
}

/// What Codex's config says about the reasoning effort (`config/read`).
#[derive(Default)]
enum ConfiguredEffort {
    /// Codex has not said (or could not).
    #[default]
    Unknown,
    /// No level: each model's own default applies.
    Unset,
    /// `model_reasoning_effort`.
    Level(String),
}

/// Codex's per-connection state: the turn in flight, Octet's own requests
/// awaiting replies and the model catalog being paged in.
#[derive(Default)]
pub(super) struct CodexProtocol {
    turn: Option<String>,
    /// Octet's last wire request ID.
    next_id: u64,
    requests: HashMap<u64, Outstanding>,
    text_items: HashSet<String>,
    catalog: Vec<ModelInfo>,
    catalog_pages: usize,
    catalog_cursors: HashSet<String>,
    /// Steering sent before Codex named the turn; sent once it does.
    pending_steer: VecDeque<String>,
    /// Each `fileChange` item's paths from its start, for its approval; cleared as the turn ends.
    file_changes: HashMap<String, Vec<String>>,
    /// Each listed model's default reasoning effort, by its selection.
    model_efforts: HashMap<String, String>,
    /// The effort Codex's config names.
    configured_effort: ConfiguredEffort,
    /// The model Codex opened the thread with.
    thread_model: Option<String>,
    /// The effort the thread runs at, as Codex reported it or Octet last
    /// sent it: Codex keeps a turn's effort for the turns after it.
    thread_effort: Option<String>,
    /// The agent message whose text was shown last, until a tool call:
    /// another message after it starts a new paragraph.
    last_text_item: Option<String>,
}

impl Protocol for CodexProtocol {
    fn launch_args(config: &Config) -> Vec<OsString> {
        launch_args(config)
    }

    fn answer(wire: &Value, allow: bool) -> Value {
        answer_wire(wire, allow)
    }

    /// Only frames for this session's thread count: another thread's
    /// chatter is not progress.
    fn is_progress(&self, core: &Core, frame: &Value) -> bool {
        frame
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .is_none_or(|id| id == core.session)
            && frame["method"] != "skills/changed"
    }

    fn turn_started(&mut self) {
        self.turn = None;
        self.text_items.clear();
        self.pending_steer.clear();
        self.last_text_item = None;
    }

    /// Steers the turn, or holds the text until Codex names the turn.
    async fn steer(&mut self, core: &mut Core, text: &str) -> Result<bool, DriverError> {
        if self.turn.is_none() {
            if let Some(dropped) = push_bounded(&mut self.pending_steer, text.to_owned(), 8) {
                core.emit(Event::Notice(format!("This steer was not sent: {dropped}")))?;
            }
            return Ok(true);
        }
        self.send_steer(core, text).await?;
        Ok(true)
    }

    async fn initialize(&mut self, core: &mut Core) -> Result<(), DriverError> {
        let client = json!({"name":"octet","version":env!("CARGO_PKG_VERSION")});
        let params = json!({"clientInfo": client, "capabilities": {"experimentalApi": true}});
        self.request(core, Outstanding::Initialize, "initialize", params)
            .await
    }

    /// Interrupts the running turn once Codex has said which turn it is.
    async fn interrupt(&mut self, core: &mut Core) -> Result<(), DriverError> {
        if self.turn.is_some() {
            self.request_interrupt(core).await?;
        }
        Ok(())
    }

    /// Images go as local paths after the text; Codex reads them itself.
    async fn send_prompt(
        &mut self,
        core: &mut Core,
        text: &str,
        images: &[ImageAttachment],
    ) -> Result<(), DriverError> {
        let input: Vec<Value> = std::iter::once(json!({"type":"text","text":text}))
            .chain(
                images
                    .iter()
                    .map(|image| json!({"type":"localImage","path":image.path})),
            )
            .collect();
        self.turn_start(core, input).await
    }

    /// A skill item (or `$name` when there is no path), then the context
    /// and the arguments as one text item.
    async fn send_skill(
        &mut self,
        core: &mut Core,
        name: &str,
        path: Option<&Path>,
        args: &str,
        context: Option<&str>,
    ) -> Result<(), DriverError> {
        let mut input = vec![match path {
            Some(path) => json!({"type":"skill","name":name,"path":path}),
            None => json!({"type":"text","text":format!("${name}")}),
        }];
        let text: Vec<&str> = [context.unwrap_or(""), args]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        if !text.is_empty() {
            input.push(json!({"type":"text","text":text.join("\n\n")}));
        }
        self.turn_start(core, input).await
    }

    /// Compaction is a turn on the thread; an error reply fails it as a
    /// rejected `turn/start` does.
    async fn compact(&mut self, core: &mut Core) -> Result<(), DriverError> {
        let params = json!({"threadId": core.session});
        self.request(core, Outstanding::Start, "thread/compact/start", params)
            .await
    }

    /// Codex applies a mode with the next turn's overrides, so no request
    /// is sent: the switch is done at once, as a ready future.
    fn set_mode(
        &mut self,
        core: &mut Core,
        target: Mode,
    ) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(adopt_mode_now(core, target))
    }

    async fn on_frame(&mut self, core: &mut Core, v: Value) -> Result<(), DriverError> {
        let method = v["method"].as_str().unwrap_or("");
        if method.is_empty() {
            return self.response(core, &v).await;
        }
        if v.get("id").is_some() {
            return self.server_request(core, v).await;
        }
        // Not tied to a thread, so before the thread filter.
        if method == "skills/changed" {
            return self.request_skills(core, true).await;
        }
        if v.pointer("/params/threadId").and_then(Value::as_str) != Some(core.session.as_str()) {
            return Ok(());
        }
        if method == "turn/started" && core.phase.is_running() {
            self.turn = v
                .pointer("/params/turn/id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if core.phase == Phase::Interrupting {
                self.drop_held_steers(core)?;
                self.request_interrupt(core).await?;
            } else {
                for text in std::mem::take(&mut self.pending_steer) {
                    self.send_steer(core, &text).await?;
                }
            }
        }
        if method == "thread/tokenUsage/updated" {
            core.emit(Event::Usage(codex_usage(&v["params"]["tokenUsage"])))?;
        }
        if !core.phase.is_running() {
            return Ok(());
        }
        if let Some(event_turn) = v.pointer("/params/turnId").and_then(Value::as_str)
            && Some(event_turn) != self.turn.as_deref()
        {
            return Ok(());
        }
        match method {
            "item/agentMessage/delta" => {
                if let Some(text) = v.pointer("/params/delta").and_then(Value::as_str) {
                    // A repeated item is already counted; only a new one can hit the limit.
                    let id = v.pointer("/params/itemId").and_then(Value::as_str);
                    if let Some(id) = id
                        && !self.text_items.contains(id)
                    {
                        if self.text_items.len() >= 4096 {
                            return Err(DriverError::TurnItemLimit);
                        }
                        self.text_items.insert(id.to_owned());
                    }
                    self.message_text(core, id.unwrap_or_default(), text)?;
                }
            }
            "item/started" | "item/completed" => {
                let item = &v["params"]["item"];
                match item["type"].as_str().unwrap_or("") {
                    "agentMessage" if method == "item/completed" => {
                        let id = item["id"].as_str().unwrap_or("");
                        if !self.text_items.contains(id)
                            && let Some(text) = item["text"].as_str()
                        {
                            self.message_text(core, id, text)?;
                        }
                    }
                    "commandExecution" | "fileChange" | "mcpToolCall" => {
                        let started = method == "item/started";
                        if started && item["type"] == "fileChange" && self.file_changes.len() < 64 {
                            let id = one_line(item["id"].as_str().unwrap_or_default());
                            self.file_changes.insert(id, change_paths(item));
                        }
                        self.last_text_item = None;
                        core.emit(Event::Tool(codex_tool_call(item, started)))?;
                    }
                    _ => {}
                }
            }
            "turn/completed" => {
                let completed = v.pointer("/params/turn/id").and_then(Value::as_str);
                if self.turn.is_none() || completed != self.turn.as_deref() {
                    return Ok(());
                }
                self.file_changes.clear();
                let status = v
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let error = (status == "failed").then(|| error_text(&v["params"]["turn"]["error"]));
                core.finish_turn(Outcome::from_vendor(&short(status)), error)?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl CodexProtocol {
    /// Shows agent message `item`'s `text`. Codex sends each message as its
    /// own item with no break between them, so a new message straight after
    /// another starts a paragraph.
    fn message_text(&mut self, core: &Core, item: &str, text: &str) -> Result<(), DriverError> {
        if text.is_empty() {
            return Ok(());
        }
        if self
            .last_text_item
            .as_deref()
            .is_some_and(|last| last != item)
        {
            core.emit(Event::Text("\n\n".into()))?;
        }
        if self.last_text_item.as_deref() != Some(item) {
            self.last_text_item = Some(item.to_owned());
        }
        core.emit(Event::Text(text.into()))
    }

    /// Sends one of Octet's requests and records what its reply answers.
    async fn request(
        &mut self,
        core: &Core,
        kind: Outstanding,
        method: &str,
        params: Value,
    ) -> Result<(), DriverError> {
        self.next_id += 1;
        self.requests.insert(self.next_id, kind);
        core.send(json!({"id": self.next_id, "method": method, "params": params}))
            .await
    }

    async fn send_steer(&mut self, core: &Core, text: &str) -> Result<(), DriverError> {
        let params = json!({
            "threadId": core.session,
            "expectedTurnId": self.turn,
            "input": [{"type": "text", "text": text}],
        });
        self.request(
            core,
            Outstanding::Steer(text.to_owned()),
            "turn/steer",
            params,
        )
        .await
    }

    async fn request_interrupt(&mut self, core: &Core) -> Result<(), DriverError> {
        let params = json!({"threadId":core.session,"turnId":self.turn});
        self.request(core, Outstanding::Interrupt, "turn/interrupt", params)
            .await
    }

    /// Steers held for a turn Codex never named are reported, not dropped.
    fn drop_held_steers(&mut self, core: &Core) -> Result<(), DriverError> {
        for text in std::mem::take(&mut self.pending_steer) {
            core.emit(Event::Notice(format!("This steer was not sent: {text}")))?;
        }
        Ok(())
    }

    /// Replies to Octet's own requests, matched by the request they answer.
    async fn response(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        let id = v["id"].as_u64();
        let Some(kind) = id.and_then(|id| self.requests.remove(&id)) else {
            // A reply to a request not yet sent, while connecting, means the
            // handshake is out of order; a repeated reply is ignored.
            if !core.phase.is_ready() && id.is_some_and(|id| id > self.next_id) {
                return Err("Unexpected protocol initialization order".into());
            }
            return Ok(());
        };
        match kind {
            Outstanding::Initialize => self.handshaken(core, v).await,
            Outstanding::OpenThread => self.opened(core, v).await,
            Outstanding::ModelList => self.catalog_page(core, v).await,
            Outstanding::SkillsList => {
                if v.get("error").is_none() {
                    core.emit(Event::Skills(skills(&v["result"]["data"])))?;
                }
                if core.config.catalog_only {
                    // Models and skills are in: a probe takes them now.
                    core.emit(Event::Ready {
                        session: String::new(),
                    })?;
                }
                Ok(())
            }
            Outstanding::ConfigRead => {
                if v.get("error").is_none() {
                    let effort = v.pointer("/result/config/model_reasoning_effort");
                    self.configured_effort =
                        match effort.and_then(Value::as_str).filter(|e| valid_effort(e)) {
                            Some(effort) => ConfiguredEffort::Level(effort.to_owned()),
                            None => ConfiguredEffort::Unset,
                        };
                }
                Ok(())
            }
            Outstanding::Start => {
                if v.get("error").is_some() {
                    // Codex may not have taken the turn's effort.
                    self.thread_effort = None;
                    self.drop_held_steers(core)?;
                    core.finish_turn(Outcome::Failed, Some(error_text(&v["error"])))?;
                }
                Ok(())
            }
            Outstanding::Interrupt => match v.get("error") {
                Some(_) => core.emit(Event::Notice(
                    "Interrupt was rejected; waiting for terminal outcome".into(),
                )),
                None => Ok(()),
            },
            Outstanding::Steer(text) => match v.get("error") {
                Some(error) => core.emit(Event::Notice(format!(
                    "Codex did not take the steer ({}). Not sent: {text}",
                    error_text(error)
                ))),
                None => Ok(()),
            },
        }
    }

    /// The handshake's reply: open the session.
    async fn handshaken(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if v.get("error").is_some() {
            return Err("Codex initialization failed".into());
        }
        core.phase = Phase::Handshaken;
        core.send(json!({"method":"initialized","params":{}}))
            .await?;
        if core.config.catalog_only {
            // Only the model list: no thread, so nothing in Codex's history.
            return self.request_models(core, None).await;
        }
        let mut params = codex_thread_params(core.mode);
        params["cwd"] = json!(core.config.cwd);
        let method = match &core.config.resume {
            Some(id) => {
                params["threadId"] = json!(id);
                if core.config.fork {
                    "thread/fork"
                } else {
                    "thread/resume"
                }
            }
            None => "thread/start",
        };
        if let Some(model) = &core.config.model {
            params["model"] = json!(model);
        }
        self.request(core, Outstanding::OpenThread, method, params)
            .await
    }

    /// The session is open: ready, its mode confirmed, its catalog asked for.
    async fn opened(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            return Err(format!("Codex could not open the session: {}", error_text(error)).into());
        }
        core.session = v
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .ok_or("Codex could not open the session")?
            .into();
        core.phase = Phase::Idle;
        core.emit(Event::Ready {
            session: core.session.clone(),
        })?;
        core.adopt_mode(codex_reported(&v["result"]))?;
        if let Some(model) = v["result"]["model"]
            .as_str()
            .filter(|m| valid_identifier(m))
        {
            self.thread_model = Some(model.into());
            core.emit(Event::ModelSelected(model.into()))?;
        }
        self.thread_effort = v["result"]["reasoningEffort"]
            .as_str()
            .filter(|e| valid_effort(e))
            .map(str::to_owned);
        self.request_models(core, None).await?;
        self.request_skills(core, false).await?;
        let params = json!({"cwd": core.config.cwd});
        self.request(core, Outstanding::ConfigRead, "config/read", params)
            .await
    }

    /// The effort Codex picks with no override: its config's, else the
    /// model's own default; `None` while Octet does not know it.
    fn default_effort(&self, core: &Core) -> Option<String> {
        match &self.configured_effort {
            ConfiguredEffort::Level(effort) => Some(effort.clone()),
            ConfiguredEffort::Unset => {
                let model = core.config.model.as_ref().or(self.thread_model.as_ref())?;
                self.model_efforts.get(model).cloned()
            }
            ConfiguredEffort::Unknown => None,
        }
    }

    /// `turn/start` with `input`, the model, effort and the mode's overrides.
    /// The vendor default goes as its level when the thread may run at
    /// another: Codex keeps the last turn's effort, so leaving it out would
    /// not go back.
    async fn turn_start(&mut self, core: &Core, input: Vec<Value>) -> Result<(), DriverError> {
        let mut params = json!({"threadId":core.session,"input":input});
        if let Some(model) = &core.config.model {
            params["model"] = json!(model);
        }
        let effort = match &core.config.effort {
            Some(effort) => Some(effort.clone()),
            None => self
                .default_effort(core)
                .filter(|default| self.thread_effort.as_ref() != Some(default)),
        };
        if let Some(effort) = effort {
            params["effort"] = json!(effort);
            self.thread_effort = Some(effort);
        }
        if let (Some(target), Value::Object(extra)) =
            (params.as_object_mut(), codex_turn_overrides(core.mode))
        {
            target.extend(extra);
        }
        self.request(core, Outstanding::Start, "turn/start", params)
            .await
    }

    /// Asks for the workspace's skills; `reload` after Codex says they changed.
    async fn request_skills(&mut self, core: &Core, reload: bool) -> Result<(), DriverError> {
        let params = json!({"cwds":[core.config.cwd],"forceReload":reload});
        self.request(core, Outstanding::SkillsList, "skills/list", params)
            .await
    }

    /// Asks for a page of the model catalog.
    async fn request_models(
        &mut self,
        core: &Core,
        cursor: Option<&str>,
    ) -> Result<(), DriverError> {
        let mut params = json!({"limit":100,"includeHidden":false});
        if let Some(cursor) = cursor {
            params["cursor"] = json!(cursor);
        }
        self.request(core, Outstanding::ModelList, "model/list", params)
            .await
    }

    async fn catalog_page(&mut self, core: &Core, v: &Value) -> Result<(), DriverError> {
        self.catalog_pages += 1;
        if let Some(error) = v.get("error") {
            if core.config.catalog_only {
                return Err(format!("Codex did not list its models: {}", error_text(error)).into());
            }
            return core.emit(Event::Notice(
                "Model catalog unavailable from this CLI; explicit model IDs remain supported"
                    .into(),
            ));
        }
        for (model, effort) in default_efforts(&v["result"]["data"]) {
            if self.model_efforts.len() < 256 {
                self.model_efforts.insert(model, effort);
            }
        }
        for model in catalog(&v["result"]["data"]) {
            if self.catalog.len() < 256
                && !self.catalog.iter().any(|m| m.selection == model.selection)
            {
                self.catalog.push(model);
            }
        }
        let mut more = false;
        if let Some(cursor) = v["result"]["nextCursor"].as_str().filter(|c| !c.is_empty()) {
            if self.catalog_pages < 8
                && self.catalog.len() < 256
                && cursor.len() <= 4096
                && self.catalog_cursors.insert(cursor.to_owned())
            {
                self.request_models(core, Some(cursor)).await?;
                more = true;
            } else {
                core.emit(Event::Notice(
                    "Model catalog exceeds discovery limits; showing partial results".into(),
                ))?;
            }
        }
        core.emit(Event::Models(self.catalog.clone()))?;
        if core.config.catalog_only && !more {
            // The whole list is in; the skills, then `Ready`.
            self.request_skills(core, false).await?;
        }
        Ok(())
    }

    /// A request from Codex: an approval for the running turn is shown to the
    /// user; anything else gets a stray reply.
    async fn server_request(&self, core: &mut Core, v: Value) -> Result<(), DriverError> {
        let method = v["method"].as_str().unwrap_or("");
        let approval = matches!(
            method,
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
        );
        let this_turn = v.pointer("/params/threadId").and_then(Value::as_str)
            == Some(core.session.as_str())
            && self.turn.is_some()
            && v.pointer("/params/turnId").and_then(Value::as_str) == self.turn.as_deref();
        if approval && core.phase == Phase::InTurn && this_turn {
            let detail = serde_json::to_string_pretty(&v["params"]).unwrap_or_default();
            let item = v
                .pointer("/params/itemId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let files = self.file_changes.get(item).map_or(&[][..], Vec::as_slice);
            let summary = codex_approval_summary(method, &v["params"], files);
            return core.queue_approval(v, detail, summary).await;
        }
        if let Some(reply) = stray(&v) {
            let notice = format!("Request declined: {}", short(method));
            core.send(reply).await?;
            core.emit(Event::Notice(notice))?;
        }
        Ok(())
    }
}

/// The app server, with plugins or their hooks turned off when asked (stable
/// feature flags: `--disable plugins`, `--disable hooks`).
fn launch_args(config: &Config) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["app-server".into()];
    if !config.plugins.load {
        args.extend(["--disable".into(), "plugins".into()]);
    } else if !config.plugins.hooks {
        args.extend(["--disable".into(), "hooks".into()]);
    }
    args
}

/// Codex takes `target` from the next turn: record it and say so.
fn adopt_mode_now(core: &mut Core, target: Mode) -> Result<(), DriverError> {
    core.mode = target;
    core.emit(Event::ModeChanged(core.mode))?;
    if core.phase.is_running() {
        core.emit(Event::Notice(super::MODE_NEXT_TURN_NOTICE.into()))?;
    }
    Ok(())
}

/// The Codex catalog: `model` is both the selection and the resolved ID.
/// `skills/list` data as skills: the enabled ones with a usable name, bounded.
fn skills(data: &Value) -> Vec<SkillInfo> {
    data.as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["skills"].as_array().into_iter().flatten())
        .filter(|v| v["enabled"] == true)
        .filter_map(SkillInfo::from_json)
        .take(super::MAX_SKILLS)
        .collect()
}

/// What the header shows for Codex's `tokenUsage`: the context the last
/// request used (`last`), of the model's window when Codex names it. The
/// running `total` counts every turn's re-read input again, so it grows
/// fast and never drops after compaction; it shows, named as a total, only
/// from a CLI that reports nothing else.
fn codex_usage(usage: &Value) -> String {
    let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
    match (count("/last/totalTokens"), count("/modelContextWindow")) {
        (Some(used), Some(window)) => {
            format!("{}/{} context", short_count(used), short_count(window))
        }
        (Some(used), None) => format!("{} context", short_count(used)),
        (None, _) => {
            let total = count("/total/totalTokens").unwrap_or(0);
            format!("{} tokens in all", short_count(total))
        }
    }
}

/// `n` in a few characters: `950`, `14.3k`, `272k`, `1.3M`.
fn short_count(n: u64) -> String {
    /// `tenths` as a whole number, or with one decimal when it has one.
    fn tenths(tenths: u64, unit: &str) -> String {
        match tenths % 10 {
            0 => format!("{}{unit}", tenths / 10),
            digit => format!("{}.{digit}{unit}", tenths / 10),
        }
    }
    match n {
        0..1_000 => n.to_string(),
        1_000..99_950 => tenths((n + 50) / 100, "k"),
        99_950..999_500 => format!("{}k", (n + 500) / 1_000),
        _ => tenths((n + 50_000) / 100_000, "M"),
    }
}

/// Each catalog entry's model and its default reasoning effort, bounded.
fn default_efforts(value: &Value) -> impl Iterator<Item = (String, String)> + '_ {
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(256)
        .filter_map(|v| {
            let model = v["model"].as_str().filter(|m| valid_identifier(m))?;
            let effort = v["defaultReasoningEffort"]
                .as_str()
                .filter(|e| valid_effort(e))?;
            Some((model.to_owned(), effort.to_owned()))
        })
}

fn catalog(value: &Value) -> Vec<ModelInfo> {
    model_catalog_with(value, "model", "model")
}

/// Codex counterpart of `claude_stray_reply`: the reply to a request Octet
/// will not put in front of the user.
pub fn codex_stray_reply(value: &Value) -> Option<Value> {
    stray(value)
}

/// A Codex item as a transcript preview. A command leads with what ran and how
/// it ended; long output keeps its end, where failures are reported.
pub(super) fn codex_tool_detail(item: &Value, phase: &str) -> String {
    let kind = item["type"].as_str().unwrap_or("tool");
    let mut text = format!("{kind} · {phase}");
    if kind != "commandExecution" {
        text.push('\n');
        text.push_str(&item.to_string());
        return limited(&text);
    }
    match &item["command"] {
        Value::Null => {}
        Value::String(command) => {
            let _ = write!(text, "\n$ {command}");
        }
        other => {
            let _ = write!(text, "\n$ {other}");
        }
    }
    match (item["status"].as_str(), item["exitCode"].as_i64()) {
        (Some(status), Some(code)) => {
            let _ = write!(text, "\n{status} · exit {code}");
        }
        (Some(status), None) => {
            let _ = write!(text, "\n{status}");
        }
        (None, Some(code)) => {
            let _ = write!(text, "\nexit {code}");
        }
        (None, None) => {}
    }
    if let Some(output) = item["aggregatedOutput"].as_str().filter(|o| !o.is_empty()) {
        const CUT: &str = "[earlier output cut]\n";
        let room = EVENT_BYTES.saturating_sub(text.len() + 1 + CUT.len());
        text.push('\n');
        if output.len() > room {
            let start = output.ceil_char_boundary(output.len() - room);
            text.push_str(CUT);
            text.push_str(&output[start..]);
        } else {
            text.push_str(output);
        }
    }
    limited(&text)
}
/// A Codex tool item as a call: running when `started`, else done or failed by its status
/// and exit code.
pub(super) fn codex_tool_call(item: &Value, started: bool) -> ToolCall {
    let kind = item["type"].as_str().unwrap_or("tool");
    let failed = matches!(item["status"].as_str(), Some("failed" | "declined"))
        || item["exitCode"].as_i64().is_some_and(|code| code != 0);
    let phase = match (started, failed) {
        (true, _) => ToolPhase::Running,
        (false, true) => ToolPhase::Failed,
        (false, false) => ToolPhase::Done,
    };
    let target = match kind {
        "commandExecution" => item["command"].as_str(),
        "fileChange" => item.pointer("/changes/0/path").and_then(Value::as_str),
        "mcpToolCall" => item["tool"].as_str(),
        _ => None,
    };
    ToolCall {
        id: one_line(item["id"].as_str().unwrap_or_default()),
        name: one_line(kind),
        target: target.map(one_line).unwrap_or_default(),
        phase,
        detail: codex_tool_detail(item, if started { "running" } else { "finished" }),
    }
}
/// The paths a `fileChange` item would change.
fn change_paths(item: &Value) -> Vec<String> {
    item["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|change| change["path"].as_str())
        .map(str::to_owned)
        .collect()
}
/// What a Codex approval request would do. A file-change request names only its item, so its
/// `files` come from that item's start.
pub(super) fn codex_approval_summary(
    method: &str,
    params: &Value,
    files: &[String],
) -> ApprovalSummary {
    if method == "item/fileChange/requestApproval" {
        let mut summary = ApprovalSummary::new(match files.len() {
            0 => "Change files".to_owned(),
            1 => "Edit 1 file".to_owned(),
            n => format!("Edit {n} files"),
        });
        summary.files = files.to_vec();
        return summary;
    }
    let mut summary = ApprovalSummary::new("Run a shell command");
    summary.command = params["command"].as_str().map(str::to_owned);
    summary.cwd = params["cwd"].as_str().map(str::to_owned);
    summary
}
/// The reply to a Codex request Octet will not put in front of the user:
/// approvals outside a turn are declined, anything else is unsupported.
fn stray(value: &Value) -> Option<Value> {
    let id = value.get("id")?;
    Some(match value.get("method")?.as_str()? {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"id":id,"result":{"decision":"decline"}})
        }
        "item/permissions/requestApproval" => {
            json!({"id":id,"result":{"permissions":{},"scope":"turn"}})
        }
        _ => {
            json!({"id":id,"error":{"code":-32601,"message":"Octet does not support this request"}})
        }
    })
}
/// A vendor error as a person should read it: the message, including one
/// nested as JSON text, with Codex's error kind when it says more than "other".
pub(super) fn error_text(error: &Value) -> String {
    if error.is_null() {
        return "The vendor reported an error without details".into();
    }
    let Some(message) = error.as_str().or(error["message"].as_str()) else {
        return limited(&error.to_string());
    };
    let inner = serde_json::from_str::<Value>(message).ok().and_then(|v| {
        v.pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let text = inner.unwrap_or_else(|| message.to_owned());
    match &error["codexErrorInfo"] {
        Value::String(kind) if kind != "other" => limited(&format!("{text} ({kind})")),
        kind @ Value::Object(_) => limited(&format!("{text} ({kind})")),
        _ => limited(&text),
    }
}

pub(crate) fn codex_thread_params(mode: Mode) -> Value {
    let (sandbox, policy, reviewer) = match mode {
        Mode::Ask => ("workspace-write", "untrusted", "user"),
        Mode::AcceptEdits => ("workspace-write", "on-request", "user"),
        Mode::Auto => ("workspace-write", "on-request", "auto_review"),
        Mode::FullAccess => ("danger-full-access", "never", "user"),
    };
    json!({"sandbox":sandbox,"approvalPolicy":policy,"approvalsReviewer":reviewer})
}
/// The mode a Codex thread reply echoes. Outer None: the reply carries no
/// policy (older CLI). Inner None: a policy Octet does not map, with the
/// raw description for the user.
pub(crate) fn codex_reported(result: &Value) -> Option<(Option<Mode>, String)> {
    let policy = result.get("approvalPolicy")?;
    let sandbox = match result["sandbox"]["type"]
        .as_str()
        .or(result["sandbox"].as_str())
    {
        Some("workspaceWrite" | "workspace-write") => json!("workspace-write"),
        Some("dangerFullAccess" | "danger-full-access") => json!("danger-full-access"),
        _ => result["sandbox"].clone(),
    };
    let reviewer = match result["approvalsReviewer"].as_str() {
        Some("guardian_subagent") => json!("auto_review"),
        Some(reviewer) => json!(reviewer),
        None => json!("user"),
    };
    let echo = json!({"sandbox":sandbox,"approvalPolicy":policy,"approvalsReviewer":reviewer});
    let mode = Mode::ALL
        .into_iter()
        .find(|mode| codex_thread_params(*mode) == echo);
    let raw = format!(
        "sandbox {}, approval {}, reviewer {}",
        result["sandbox"], policy, result["approvalsReviewer"]
    );
    Some((mode, limited(&raw)))
}
/// Codex applies these on the turn and every later turn. Live modes all share
/// the workspace-write sandbox, so no sandboxPolicy override is sent.
pub(crate) fn codex_turn_overrides(mode: Mode) -> Value {
    let params = codex_thread_params(mode);
    json!({"approvalPolicy":params["approvalPolicy"],"approvalsReviewer":params["approvalsReviewer"]})
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::Engine;
    #[test]
    fn usage_is_the_context_in_use_not_the_running_total() {
        let usage = |v: Value| codex_usage(&v);
        let window = json!({"total":{"totalTokens":954_802},"last":{"totalTokens":14_266},
            "modelContextWindow":272_000});
        assert_eq!(usage(window), "14.3k/272k context");
        let no_window = json!({"total":{"totalTokens":954_802},"last":{"totalTokens":950}});
        assert_eq!(usage(no_window), "950 context");
        // An older CLI with only the running total: named as one.
        let total = json!({"total":{"totalTokens":1_250_000}});
        assert_eq!(usage(total), "1.3M tokens in all");
        assert_eq!(usage(json!({})), "0 tokens in all");
    }
    #[test]
    fn token_counts_read_short() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (14_266, "14.3k"),
            (99_960, "100k"),
            (272_000, "272k"),
            (1_000_000, "1M"),
            (1_250_000, "1.3M"),
        ];
        for (n, shown) in cases {
            assert_eq!(short_count(n), shown, "{n}");
        }
    }
    #[test]
    fn plugin_switches_reach_the_app_server() {
        let mut c = Config::new(Engine::CODEX, "codex", ".");
        let args = |c: &Config| {
            launch_args(c)
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(args(&c), ["app-server"]);
        c.plugins.hooks = false;
        assert_eq!(args(&c), ["app-server", "--disable", "hooks"]);
        c.plugins.load = false;
        assert_eq!(args(&c), ["app-server", "--disable", "plugins"]);
    }
    #[test]
    fn error_text_prefers_the_vendor_message() {
        assert_eq!(
            error_text(&json!({"message":"plain","codexErrorInfo":"other"})),
            "plain"
        );
        assert_eq!(
            error_text(&json!({"message":"limit","codexErrorInfo":"usageLimitExceeded"})),
            "limit (usageLimitExceeded)"
        );
        assert_eq!(
            error_text(&json!({"message":"{\"error\":{\"message\":\"inner\"}}"})),
            "inner"
        );
        assert_eq!(error_text(&json!({"code":-1})), "{\"code\":-1}");
        assert_eq!(error_text(&json!("text")), "text");
    }
    #[test]
    fn command_preview_leads_with_the_command_and_its_outcome() {
        let item = json!({
            "type": "commandExecution",
            "command": "cargo test",
            "status": "failed",
            "exitCode": 101,
            "aggregatedOutput": "line 1\nline 2",
        });
        assert_eq!(
            codex_tool_detail(&item, "finished"),
            "commandExecution · finished\n$ cargo test\nfailed · exit 101\nline 1\nline 2"
        );
        let running = json!({"type":"commandExecution","command":"cargo test"});
        assert_eq!(
            codex_tool_detail(&running, "running"),
            "commandExecution · running\n$ cargo test"
        );
        let other = json!({"type":"fileChange","changes":[]});
        assert!(codex_tool_detail(&other, "finished").starts_with("fileChange · finished\n{"));
        // Long output keeps its end, where failures are reported.
        let output = format!("{}END", "y".repeat(EVENT_BYTES * 2));
        let long = json!({"type": "commandExecution", "command": "x", "aggregatedOutput": output});
        let detail = codex_tool_detail(&long, "finished");
        assert!(
            detail.len() <= EVENT_BYTES
                && detail.ends_with("END")
                && detail.contains("[earlier output cut]"),
            "{}",
            detail.len()
        );
    }
    #[test]
    fn codex_mapping_matches_spec_table() {
        let expected = [
            (Mode::Ask, "workspace-write", "untrusted", "user"),
            (Mode::AcceptEdits, "workspace-write", "on-request", "user"),
            (Mode::Auto, "workspace-write", "on-request", "auto_review"),
            (Mode::FullAccess, "danger-full-access", "never", "user"),
        ];
        for (mode, sandbox, policy, reviewer) in expected {
            assert_eq!(
                codex_thread_params(mode),
                json!({"sandbox":sandbox,"approvalPolicy":policy,"approvalsReviewer":reviewer})
            );
            assert_eq!(
                codex_turn_overrides(mode),
                json!({"approvalPolicy":policy,"approvalsReviewer":reviewer})
            );
        }
    }
    #[test]
    fn reported_modes_map_back_or_stay_unmapped() {
        for mode in Mode::ALL {
            let mut echo = codex_thread_params(mode);
            echo["sandbox"] = match mode {
                Mode::FullAccess => json!({"type":"dangerFullAccess"}),
                _ => json!({"type":"workspaceWrite"}),
            };
            assert_eq!(codex_reported(&echo).map(|r| r.0), Some(Some(mode)));
        }
        let legacy = json!({
            "sandbox": {"type": "workspaceWrite"},
            "approvalPolicy": "on-request",
            "approvalsReviewer": "guardian_subagent",
        });
        assert_eq!(codex_reported(&legacy).map(|r| r.0), Some(Some(Mode::Auto)));
        let read_only = json!({
            "sandbox": {"type": "readOnly"},
            "approvalPolicy": "on-request",
            "approvalsReviewer": "user",
        });
        let (mode, raw) = codex_reported(&read_only).unwrap();
        assert_eq!(mode, None);
        assert!(raw.contains("readOnly"), "{raw}");
        assert_eq!(codex_reported(&json!({"thread":{}})), None);
    }
    #[test]
    fn stray_request_replies_use_production_wording() {
        let unknown = json!({"id":7,"method":"item/tool/requestUserInput","params":{}});
        let reply = codex_stray_reply(&unknown).unwrap();
        assert!(!reply.to_string().contains("fixture"), "{reply}");
        assert_eq!(reply["error"]["code"], -32601);
        let approval = json!({"id":8,"method":"item/commandExecution/requestApproval","params":{}});
        assert_eq!(
            codex_stray_reply(&approval).unwrap()["result"]["decision"],
            "decline"
        );
    }
    #[test]
    fn error_text_keeps_structured_kinds_and_names_missing_errors() {
        assert_eq!(
            error_text(&json!({"message":"bad","codexErrorInfo":{"httpStatus":429}})),
            "bad ({\"httpStatus\":429})"
        );
        assert_eq!(
            error_text(&Value::Null),
            "The vendor reported an error without details"
        );
    }
    #[test]
    fn an_item_is_a_call_that_fails_on_status_or_exit() {
        let started = json!({"id":"c1","type":"commandExecution","command":"cargo test","status":"inProgress"});
        let call = codex_tool_call(&started, true);
        assert_eq!(
            (
                call.id.as_str(),
                call.name.as_str(),
                call.target.as_str(),
                call.phase
            ),
            ("c1", "commandExecution", "cargo test", ToolPhase::Running)
        );
        let ok = json!({"id":"c1","type":"commandExecution","command":"x","status":"completed","exitCode":0});
        assert_eq!(codex_tool_call(&ok, false).phase, ToolPhase::Done);
        for failed in [
            json!({"id":"c","type":"commandExecution","status":"completed","exitCode":101}),
            json!({"id":"c","type":"commandExecution","status":"failed"}),
            json!({"id":"c","type":"fileChange","status":"declined","changes":[]}),
            json!({"id":"c","type":"mcpToolCall","status":"failed","tool":"echo"}),
        ] {
            assert_eq!(
                codex_tool_call(&failed, false).phase,
                ToolPhase::Failed,
                "{failed}"
            );
        }
        let change = json!({"path":"a.rs","kind":"update","diff":""});
        let files = json!({"id":"f","type":"fileChange","status":"completed","changes":[change]});
        assert_eq!(codex_tool_call(&files, false).target, "a.rs");
        let mcp =
            json!({"id":"m","type":"mcpToolCall","status":"completed","tool":"echo","server":"fx"});
        assert_eq!(codex_tool_call(&mcp, false).target, "echo");
    }

    #[test]
    fn approvals_name_the_command_folder_or_files() {
        let params = json!({"command":"cargo test","cwd":"/w","itemId":"c1"});
        let summary = codex_approval_summary("item/commandExecution/requestApproval", &params, &[]);
        assert_eq!(summary.action, "Run a shell command");
        assert_eq!(
            (summary.command.as_deref(), summary.cwd.as_deref()),
            (Some("cargo test"), Some("/w"))
        );
        let files = ["a.rs".to_owned(), "b.rs".to_owned()];
        let summary = codex_approval_summary(
            "item/fileChange/requestApproval",
            &json!({"itemId":"f"}),
            &files,
        );
        assert_eq!(
            (summary.action.as_str(), summary.files.len()),
            ("Edit 2 files", 2)
        );
        assert_eq!(
            codex_approval_summary("item/fileChange/requestApproval", &json!({}), &[]).action,
            "Change files"
        );
    }
}
