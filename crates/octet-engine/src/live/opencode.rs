//! opencode over the Agent Client Protocol (`opencode acp`): its row, the
//! launch configuration (its arguments, the agents that carry Octet's
//! permission modes, and the environment that starts it), the handshake, the
//! model catalog, turns, skills and the probe.
use super::{
    ApprovalSummary, BoxFuture, Channels, Config, DriverError, Event, ImageAttachment, Limits,
    Mode, ModelInfo, Outcome, Provider, SkillInfo, ToolCall, ToolPhase,
    activity::one_line,
    check_inline, deadline_after, limited,
    protocol::{Core, Phase, Protocol},
    push_bounded, short, valid_identifier,
};
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    future::Future,
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::time::Instant;

/// opencode's row in the provider table.
pub(super) const PROVIDER: Provider = Provider {
    name: "opencode",
    title: "opencode",
    default_binary: "opencode",
    offline: false,
    modes: [
        "opencode asks before edits, commands and other actions (agent octet-ask)",
        "Edits proceed; commands and other actions ask (agent octet-accept-edits)",
        "opencode's own permission settings decide (agent build; its defaults ask for nothing)",
        "No permission checks at all (agent octet-full-access)",
    ],
    steer: false,
    inline_images: true,
    effort_live: false,
    takes_effort: false,
    ids_carry_name: true,
    efforts: &[],
    // A placeholder until it is timed against the real CLI.
    shutdown_grace: Duration::from_millis(500),
    lists_models: true,
    launch_args: <OpencodeProtocol as Protocol>::launch_args,
    launch_env,
    start,
};

/// Runs one opencode session through the shared driver.
fn start(config: Config, limits: Limits, channels: Channels) -> BoxFuture<Result<(), String>> {
    Box::pin(async move { super::driver::run::<OpencodeProtocol>(config, limits, channels).await })
}

/// The most tool calls one turn remembers the names of.
const MAX_TOOLS: usize = 256;
/// The most models one catalog keeps.
const MAX_MODELS: usize = 256;
/// The most timed-out mode switches kept in case opencode confirms them late.
const MAX_LATE_SWITCHES: usize = 8;
/// How long a probe waits, after the session opens, for opencode to list its
/// commands before it settles for the models alone.
const PROBE_COMMANDS_WAIT: Duration = Duration::from_secs(2);
/// How long `opencode session delete` may run before it is killed.
const SESSION_DELETE_LIMIT: Duration = Duration::from_secs(10);

/// What a reply from opencode answers: one of Octet's own requests.
enum Outstanding {
    /// `initialize`, the handshake's first request.
    Initialize,
    /// `session/new`, `session/resume` or `session/fork`, with the session a
    /// resume names, which its reply need not repeat.
    Open(Option<String>),
    /// The model the user asked for, set after the session opens.
    SetModel,
    /// The agent that carries this mode, set as the handshake's last step.
    SetMode(Mode),
    /// The agent that carries this mode, set in the open session at the
    /// user's request.
    Switch(Mode),
    /// `session/prompt`: its reply ends the turn.
    Prompt,
}

/// A live mode switch awaiting opencode's reply.
struct ModeRequest {
    /// The wire ID of the `session/set_config_option` request.
    id: u64,
    /// The mode the user asked for.
    target: Mode,
    /// When Octet stops waiting and says the switch is unconfirmed.
    deadline: Instant,
}

/// opencode's per-connection state: Octet's requests awaiting replies, the
/// session's model and agent as opencode last reported them, live mode
/// switches awaiting confirmation, and the turn's tool calls.
#[derive(Default)]
pub(super) struct OpencodeProtocol {
    /// Octet's last wire request ID.
    next_id: u64,
    /// Octet's requests that have no reply yet, by wire ID, with what each
    /// reply answers.
    requests: HashMap<u64, Outstanding>,
    /// The live mode switch awaiting its reply, until its deadline.
    mode_request: Option<ModeRequest>,
    /// Timed-out switches opencode may still confirm, oldest first: the
    /// header must follow the agent opencode is really in.
    late_modes: VecDeque<(u64, Mode)>,
    /// A `session/cancel` was sent this turn. opencode ends a cancelled
    /// prompt with `end_turn`, so this is what makes it an interruption.
    cancel_sent: bool,
    /// The session's model (`currentValue` of its `model` option).
    model: Option<String>,
    /// The session's agent (`currentValue` of its `mode` option).
    agent: Option<String>,
    /// Each tool call's name and target from its first frame, this turn.
    tools: HashMap<String, (String, String)>,
    /// A probe's session is open and its commands are awaited until this
    /// moment; `None` once the probe has been answered.
    probe_wait: Option<Instant>,
    /// The last command list holds `compact`.
    compact_offered: bool,
}

impl Protocol for OpencodeProtocol {
    fn launch_args(config: &Config) -> Vec<OsString> {
        launch_args(config)
    }

    fn answer(wire: &Value, allow: bool) -> Value {
        answer_wire(wire, allow)
    }

    fn mode_change_pending(&self) -> bool {
        self.mode_request.is_some()
    }

    fn deadline(&self) -> Option<Instant> {
        let mode = self.mode_request.as_ref().map(|request| request.deadline);
        [mode, self.probe_wait].into_iter().flatten().min()
    }

    /// A switch opencode has not confirmed in time is reported, and kept in
    /// case opencode confirms it late. A probe whose commands never came
    /// settles for the models. Nothing here waits, so the work is done at
    /// once and handed back as a ready future.
    fn on_deadline(
        &mut self,
        core: &mut Core,
    ) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(
            self.report_late_mode(core)
                .and_then(|()| self.probe_timed_out(core)),
        )
    }

    /// Only frames for this session count: another session's chatter is
    /// not progress.
    fn is_progress(&self, core: &Core, frame: &Value) -> bool {
        frame
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .is_none_or(|id| id == core.session)
    }

    fn turn_started(&mut self) {
        self.cancel_sent = false;
        self.tools.clear();
    }

    /// Says when the user's `OPENCODE_CONFIG_CONTENT` had to be replaced,
    /// then sends `initialize`, offering no client capabilities.
    async fn initialize(&mut self, core: &mut Core) -> Result<(), DriverError> {
        let user = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
        if let Some(notice) = launch_env_from(&core.config, user.as_deref()).1 {
            core.emit(Event::Notice(notice))?;
        }
        let params = json!({"protocolVersion": 1, "clientCapabilities": {}});
        self.request(core, Outstanding::Initialize, "initialize", params)
            .await
    }

    /// Images go inline as base64 blocks after the text, read now; an image
    /// that cannot be read fails the turn.
    async fn send_prompt(
        &mut self,
        core: &mut Core,
        text: &str,
        images: &[ImageAttachment],
    ) -> Result<(), DriverError> {
        let mut prompt = vec![json!({"type": "text", "text": text})];
        let mut encoded = Vec::with_capacity(images.len());
        for image in images {
            // A file swapped for a FIFO, or on a stalled mount, must not hold
            // the session: cancel and stop wait while this runs.
            match tokio::time::timeout(core.limits.image_read, image.read_base64()).await {
                Ok(Ok(data)) => encoded.push((image, data)),
                Ok(Err(error)) => return fail_turn(core, error.to_string()),
                Err(_) => {
                    return fail_turn(
                        core,
                        format!(
                            "Reading {} took longer than {}; the prompt was not sent",
                            image.name,
                            super::driver::seconds(core.limits.image_read)
                        ),
                    );
                }
            }
        }
        // Files can change after they were attached; check what is sent.
        let sizes = encoded
            .iter()
            .map(|(image, data)| (image.name.as_str(), data.len() as u64));
        if let Err(error) = check_inline(PROVIDER.title, sizes) {
            return fail_turn(core, error.to_string());
        }
        for (image, data) in encoded {
            prompt.push(json!({"type": "image", "mimeType": image.media_type, "data": data}));
        }
        let params = json!({"sessionId": core.session, "prompt": prompt});
        self.request(core, Outstanding::Prompt, "session/prompt", params)
            .await
    }

    /// A skill is one of opencode's slash commands: the prompt is
    /// `/name args`, after the context and a blank line when there is one.
    async fn send_skill(
        &mut self,
        core: &mut Core,
        name: &str,
        _path: Option<&Path>,
        args: &str,
        context: Option<&str>,
    ) -> Result<(), DriverError> {
        let text = skill_prompt(name, args, context);
        self.send_prompt(core, &text, &[]).await
    }

    /// Compaction is opencode's `/compact` command, when its last command
    /// list offered one; otherwise the turn ends, failed, saying so.
    async fn compact(&mut self, core: &mut Core) -> Result<(), DriverError> {
        if self.compact_offered {
            return self.send_prompt(core, "/compact", &[]).await;
        }
        fail_turn(
            core,
            "opencode does not offer compaction in this session".to_owned(),
        )
    }

    /// Sends the `session/cancel` notification. opencode then ends the
    /// prompt with `end_turn`, which this turn reads as interrupted.
    async fn interrupt(&mut self, core: &mut Core) -> Result<(), DriverError> {
        self.cancel_sent = true;
        core.send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": core.session},
        }))
        .await
    }

    /// Sets the agent that carries `target`. The mode changes only when
    /// opencode confirms it.
    async fn set_mode(&mut self, core: &mut Core, target: Mode) -> Result<(), DriverError> {
        self.set_option(core, Outstanding::Switch(target), "mode", agent(target))
            .await?;
        self.mode_request = Some(ModeRequest {
            id: self.next_id,
            target,
            deadline: deadline_after(core.limits.mode_confirm),
        });
        Ok(())
    }

    async fn on_frame(&mut self, core: &mut Core, v: Value) -> Result<(), DriverError> {
        let method = v["method"].as_str().unwrap_or("");
        if method.is_empty() {
            return self.response(core, &v).await;
        }
        if v.get("id").is_some() {
            return server_request(core, v).await;
        }
        let this_session = !core.session.is_empty()
            && v.pointer("/params/sessionId").and_then(Value::as_str)
                == Some(core.session.as_str());
        if method != "session/update" || !this_session {
            return Ok(());
        }
        let update = &v["params"]["update"];
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "available_commands_update" => {
                let list = &update["availableCommands"];
                self.compact_offered = offers(list, "compact");
                if core.config.catalog_only {
                    return self.probe_listed(core, list);
                }
                core.emit(Event::Skills(commands(list)))
            }
            // A resume replays the session's history before any prompt: it
            // is not this turn's output.
            _ if !core.phase.is_running() => Ok(()),
            "agent_message_chunk" => match update["content"]["text"].as_str() {
                Some(text) if update["content"]["type"] == "text" => {
                    core.emit(Event::Text(text.into()))
                }
                _ => Ok(()),
            },
            "tool_call" | "tool_call_update" => self.tool(core, update),
            _ => Ok(()),
        }
    }
}

impl OpencodeProtocol {
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
        core.send(json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}))
            .await
    }

    /// Sets the session's `option` to `value`.
    async fn set_option(
        &mut self,
        core: &Core,
        kind: Outstanding,
        option: &str,
        value: &str,
    ) -> Result<(), DriverError> {
        let params = json!({"sessionId": core.session, "configId": option, "value": value});
        self.request(core, kind, "session/set_config_option", params)
            .await
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
            Outstanding::Open(resumed) => self.opened(core, v, resumed).await,
            Outstanding::SetModel => self.model_set(core, v).await,
            Outstanding::SetMode(target) => self.mode_set(core, v, target),
            Outstanding::Switch(target) => self.switched(core, v, target),
            Outstanding::Prompt => self.prompt_ended(core, v),
        }
    }

    /// The handshake's reply: open, resume or fork the session. A probe
    /// opens a new one, since it only reads the catalog.
    async fn handshaken(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            return Err(format!("opencode initialization failed: {}", error_text(error)).into());
        }
        core.phase = Phase::Handshaken;
        let config = &core.config;
        let mut params = json!({"cwd": config.cwd, "mcpServers": []});
        let (method, resumed) = match &config.resume {
            Some(id) if !config.catalog_only => {
                params["sessionId"] = json!(id);
                if config.fork {
                    ("session/fork", None)
                } else {
                    ("session/resume", Some(id.clone()))
                }
            }
            _ => ("session/new", None),
        };
        self.request(core, Outstanding::Open(resumed), method, params)
            .await
    }

    /// The session is open: its catalog goes out, then the model the user
    /// asked for (when it is not already current) and the mode's agent.
    async fn opened(
        &mut self,
        core: &mut Core,
        v: &Value,
        resumed: Option<String>,
    ) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            return Err(
                format!("opencode could not open the session: {}", error_text(error)).into(),
            );
        }
        let result = &v["result"];
        core.session = result["sessionId"]
            .as_str()
            .filter(|id| valid_identifier(id))
            .map(str::to_owned)
            .or(resumed)
            .ok_or("opencode could not open the session")?;
        self.note_options(&result["configOptions"]);
        core.emit(Event::Models(catalog(&result["configOptions"])))?;
        if core.config.catalog_only {
            // A probe takes the models with the commands, which follow the
            // session's reply; it waits a little for them.
            self.probe_wait = Some(deadline_after(PROBE_COMMANDS_WAIT));
            return Ok(());
        }
        match core.config.model.clone() {
            Some(model) if self.model.as_deref() != Some(model.as_str()) => {
                self.set_option(core, Outstanding::SetModel, "model", &model)
                    .await
            }
            _ => self.set_agent(core).await,
        }
    }

    /// The handshake's last step: the agent that carries the session's mode.
    async fn set_agent(&mut self, core: &Core) -> Result<(), DriverError> {
        let target = core.mode;
        self.set_option(core, Outstanding::SetMode(target), "mode", agent(target))
            .await
    }

    /// The model's reply. A refusal is reported and the session keeps
    /// opencode's model; either way the agent is set next.
    async fn model_set(&mut self, core: &Core, v: &Value) -> Result<(), DriverError> {
        let requested = core.config.model.clone().unwrap_or_default();
        if let Some(error) = v.get("error") {
            core.emit(Event::Notice(format!(
                "opencode did not switch to model {}: {}",
                short(&requested),
                error_text(error)
            )))?;
        } else {
            // A reply without the options still confirms the request.
            self.model = Some(requested).filter(|model| valid_identifier(model));
            self.note_options(&v["result"]["configOptions"]);
        }
        self.set_agent(core).await
    }

    /// The agent's reply ends the handshake: ready, with the model and the
    /// mode. An agent opencode refuses, or a reply naming another agent,
    /// stops the session: whatever agent it kept may ask for less than the
    /// mode the user chose, so it must never act under it.
    fn mode_set(&mut self, core: &mut Core, v: &Value, target: Mode) -> Result<(), DriverError> {
        let wanted = agent(target);
        let refusal = self.agent_refusal(v, target).map(|refusal| match refusal {
            Refusal::Error(message) => message,
            Refusal::Reported { shown, .. } => {
                format!("opencode reported agent {shown} instead of {wanted}")
            }
        });
        if let Some(reason) = refusal {
            return Err(format!(
                "opencode would not switch to agent {wanted} ({reason}); Octet stopped the session so opencode never acts outside the mode you chose"
            )
            .into());
        }
        self.agent = Some(wanted.to_owned());
        core.phase = Phase::Idle;
        core.emit(Event::Ready {
            session: core.session.clone(),
        })?;
        if let Some(model) = &self.model {
            core.emit(Event::ModelSelected(model.clone()))?;
        }
        core.adopt_mode(Some((mode_of(wanted), wanted.to_owned())))
    }

    /// Why the reply `v` to setting the agent for `target` does not confirm
    /// it, or `None` when it does. A reply without the options still
    /// confirms the request; one naming another agent does not.
    fn agent_refusal(&mut self, v: &Value, target: Mode) -> Option<Refusal> {
        if let Some(error) = v.get("error") {
            return Some(Refusal::Error(error_text(error)));
        }
        let options = &v["result"]["configOptions"];
        self.note_options(options);
        let reported = &option(options, "mode")["currentValue"];
        (!reported.is_null() && reported.as_str() != Some(agent(target))).then(|| {
            Refusal::Reported {
                agent: reported.as_str().map(str::to_owned),
                shown: reported
                    .as_str()
                    .map_or_else(|| short(&reported.to_string()), short),
            }
        })
    }

    /// A live switch's reply. Only a confirmation changes the mode. An
    /// error, or a reply naming the agent already in force, is reported and
    /// the session keeps its mode. A reply naming any other agent stops the
    /// session: opencode may then ask for less than the header shows. A
    /// reply after the switch timed out is handled the same way.
    fn switched(&mut self, core: &mut Core, v: &Value, target: Mode) -> Result<(), DriverError> {
        let id = v["id"].as_u64();
        let late = self
            .mode_request
            .take_if(|request| Some(request.id) == id)
            .is_none();
        if late {
            self.late_modes.retain(|(late, _)| Some(*late) != id);
        }
        if let Some(refusal) = self.agent_refusal(v, target) {
            let reason = match refusal {
                Refusal::Error(message) => message,
                Refusal::Reported { agent: kept, shown } => {
                    if kept.as_deref() != Some(agent(core.mode)) {
                        return Err(format!(
                            "opencode switched to agent {shown} instead of {}; Octet stopped the session so opencode never acts outside the mode you chose",
                            agent(target)
                        )
                        .into());
                    }
                    format!("opencode reported agent {shown}")
                }
            };
            core.emit(Event::Notice(format!(
                "opencode did not switch to {}: {reason}",
                target.label()
            )))?;
            // The header was already put back when the switch timed out.
            return if late {
                Ok(())
            } else {
                core.emit(Event::ModeChanged(core.mode))
            };
        }
        self.agent = Some(agent(target).to_owned());
        core.mode = target;
        if late {
            core.emit(Event::Notice(format!(
                "opencode confirmed the switch to {} late",
                target.label()
            )))?;
        }
        core.emit(Event::ModeChanged(target))
    }

    /// Reports a live switch opencode did not confirm in time; the header
    /// keeps the current mode until it does.
    fn report_late_mode(&mut self, core: &Core) -> Result<(), DriverError> {
        let Some(request) = self
            .mode_request
            .take_if(|request| request.deadline <= Instant::now())
        else {
            return Ok(());
        };
        // Kept bounded: a vendor that never answers cannot grow either list.
        if let Some((dropped, _)) = push_bounded(
            &mut self.late_modes,
            (request.id, request.target),
            MAX_LATE_SWITCHES,
        ) {
            self.requests.remove(&dropped);
        }
        core.emit(Event::Notice(format!(
            "opencode has not confirmed the switch to {}",
            request.target.label()
        )))?;
        core.emit(Event::ModeChanged(core.mode))
    }

    /// A probe's commands arrived: they go out as its skills, then the probe
    /// is done. A repeated list, after that, is not the probe's.
    fn probe_listed(&mut self, core: &Core, list: &Value) -> Result<(), DriverError> {
        if self.probe_wait.take().is_none() {
            return Ok(());
        }
        core.emit(Event::Skills(commands(list)))?;
        finish_probe(core)
    }

    /// A probe whose commands did not come in time is done with the models
    /// alone.
    fn probe_timed_out(&mut self, core: &Core) -> Result<(), DriverError> {
        if self
            .probe_wait
            .take_if(|wait| *wait <= Instant::now())
            .is_none()
        {
            return Ok(());
        }
        finish_probe(core)
    }

    /// Records the model and agent `config_options` reports as current.
    fn note_options(&mut self, config_options: &Value) {
        if let Some(model) = current(config_options, "model") {
            self.model = Some(model.to_owned());
        }
        if let Some(agent) = current(config_options, "mode") {
            self.agent = Some(agent.to_owned());
        }
    }

    /// The prompt's reply ends the turn, with its token count when it has one.
    fn prompt_ended(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if !core.phase.is_running() {
            return Ok(());
        }
        self.tools.clear();
        if let Some(error) = v.get("error") {
            return core.finish_turn(Outcome::Failed, Some(error_text(error)));
        }
        let result = &v["result"];
        if let Some(total) = result.pointer("/usage/totalTokens").and_then(Value::as_u64) {
            core.emit(Event::Usage(format!("{total} tokens")))?;
        }
        let outcome = match result["stopReason"].as_str().unwrap_or("unknown") {
            "end_turn" if self.cancel_sent => Outcome::Interrupted,
            "end_turn" => Outcome::Completed,
            "cancelled" => Outcome::Interrupted,
            other => Outcome::Other(short(other)),
        };
        core.finish_turn(outcome, None)
    }

    /// A `tool_call` or `tool_call_update`: the call as it now stands. Its
    /// name is the title of its first frame; its target is the first path it
    /// touches, else its command.
    fn tool(&mut self, core: &Core, update: &Value) -> Result<(), DriverError> {
        let id = one_line(update["toolCallId"].as_str().unwrap_or_default());
        let target = update
            .pointer("/locations/0/path")
            .and_then(Value::as_str)
            .or_else(|| update.pointer("/rawInput/command").and_then(Value::as_str))
            .map(one_line)
            .filter(|target| !target.is_empty());
        let (name, target) = if let Some((name, known)) = self.tools.get(&id) {
            (name.clone(), target.unwrap_or_else(|| known.clone()))
        } else {
            let name = update["title"]
                .as_str()
                .map(one_line)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "tool".to_owned());
            let target = target.unwrap_or_default();
            if !id.is_empty() && self.tools.len() < MAX_TOOLS {
                self.tools
                    .insert(id.clone(), (name.clone(), target.clone()));
            }
            (name, target)
        };
        let phase = match update["status"].as_str() {
            Some("completed") => ToolPhase::Done,
            Some("failed") => ToolPhase::Failed,
            _ => ToolPhase::Running,
        };
        core.emit(Event::Tool(ToolCall {
            id,
            name,
            target,
            phase,
            detail: content_text(&update["content"]),
        }))
    }
}

/// The probe has what it came for: it says so, and its session, which only
/// read the catalog, is deleted so the user's history keeps no empty session.
fn finish_probe(core: &Core) -> Result<(), DriverError> {
    delete_session(&core.config, &core.session);
    core.emit(Event::Ready {
        session: String::new(),
    })
}

/// Deletes `session` with `opencode session delete`, on a task of its own:
/// neither the probe nor the session's shutdown waits for it, and its result
/// is ignored. The command is killed after [`SESSION_DELETE_LIMIT`], and when
/// the task is dropped with the runtime.
fn delete_session(config: &Config, session: &str) {
    // An ID that looks like an option must not be read as one.
    if session.starts_with('-') || !valid_identifier(session) {
        return;
    }
    let mut command = tokio::process::Command::new(&config.binary);
    command
        .args(["session", "delete", session])
        .envs(launch_env(config))
        .current_dir(&config.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    tokio::spawn(async move {
        let _ = tokio::time::timeout(SESSION_DELETE_LIMIT, command.status()).await;
    });
}

/// The prompt that runs the slash command `name`: `/name args`, trimmed,
/// after `context` and a blank line when there is context.
fn skill_prompt(name: &str, args: &str, context: Option<&str>) -> String {
    let command = format!("/{name} {args}");
    match context.filter(|context| !context.is_empty()) {
        Some(context) => format!("{context}\n\n{}", command.trim()),
        None => command.trim().to_owned(),
    }
}

/// Whether the command list `list` holds the command `name`.
fn offers(list: &Value, name: &str) -> bool {
    list.as_array()
        .into_iter()
        .flatten()
        .any(|command| command["name"] == name)
}

/// Why opencode's reply to setting an agent is not a confirmation.
enum Refusal {
    /// An error reply, with its message.
    Error(String),
    /// A success reply naming another agent as current.
    Reported {
        /// The agent named, when it is a string.
        agent: Option<String>,
        /// The agent named, bounded for display.
        shown: String,
    },
}

/// A request from opencode. A permission request for this session's running
/// turn is shown to the user, who decides; anything else gets the stray
/// reply, with a notice.
async fn server_request(core: &mut Core, v: Value) -> Result<(), DriverError> {
    let method = v["method"].as_str().unwrap_or_default();
    let this_session = !core.session.is_empty()
        && v.pointer("/params/sessionId").and_then(Value::as_str) == Some(core.session.as_str());
    if method == "session/request_permission" && this_session && core.phase == Phase::InTurn {
        let (detail, summary) = approval(&v["params"]["toolCall"]);
        return core.queue_approval(v, detail, summary).await;
    }
    if let Some(reply) = opencode_stray_reply(&v) {
        let notice = if method == "session/request_permission" {
            "opencode asked for a permission outside this session's running turn; Octet rejected it"
                .to_owned()
        } else {
            format!("Request declined: {}", short(method))
        };
        core.send(reply).await?;
        core.emit(Event::Notice(notice))?;
    }
    Ok(())
}

/// What a permission request's `toolCall` would do: the dialog's detail
/// (`kind · title`, then its `rawInput` in full) and its summary.
fn approval(tool_call: &Value) -> (String, ApprovalSummary) {
    let kind = tool_call["kind"].as_str().unwrap_or("other");
    let title = tool_call["title"].as_str().unwrap_or("tool");
    let mut detail = format!("{kind} · {title}");
    let input = &tool_call["rawInput"];
    if !input.is_null() {
        detail.push('\n');
        detail.push_str(&serde_json::to_string_pretty(input).unwrap_or_default());
    }
    let summary = match kind {
        "execute" => ApprovalSummary {
            command: Some(title.to_owned()),
            ..ApprovalSummary::new("Run a shell command")
        },
        "edit" => ApprovalSummary {
            files: tool_call["locations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|location| location["path"].as_str())
                .map(str::to_owned)
                .collect(),
            ..ApprovalSummary::new("Edit files")
        },
        _ => ApprovalSummary::new("Use a tool"),
    };
    (detail, summary)
}

/// The reply to a `session/request_permission`: the request's own
/// `allow_once` or `reject_once` option, never one that allows for longer.
fn answer_wire(wire: &Value, allow: bool) -> Value {
    let (kind, fallback) = if allow {
        ("allow_once", "once")
    } else {
        ("reject_once", "reject")
    };
    let option = wire
        .pointer("/params/options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option["kind"] == kind)
        .and_then(|option| option["optionId"].as_str())
        .unwrap_or(fallback);
    json!({
        "jsonrpc": "2.0",
        "id": wire["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": option}},
    })
}

/// opencode counterpart of `codex_stray_reply`: the reply to a request Octet
/// will not put in front of the user. A permission request is rejected with
/// its `reject_once` option; anything else (`fs/*` and `terminal/*`
/// included, which Octet does not offer) is an unsupported method. A frame
/// that is not a request gets none.
pub fn opencode_stray_reply(value: &Value) -> Option<Value> {
    let id = value.get("id")?;
    let method = value.get("method")?.as_str()?;
    Some(if method == "session/request_permission" {
        answer_wire(value, false)
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("Octet does not support {}", short(method))},
        })
    })
}

/// Ends a turn that could not be sent, keeping the session.
fn fail_turn(core: &mut Core, message: String) -> Result<(), DriverError> {
    core.finish_turn(Outcome::Failed, Some(message))
}

/// An ACP error as a person should read it: its message, else the error
/// itself, bounded.
fn error_text(error: &Value) -> String {
    match error["message"].as_str() {
        Some(message) => limited(message),
        None => limited(&error.to_string()),
    }
}

/// The session option `id` in `config_options`, or `Null`.
fn option<'a>(config_options: &'a Value, id: &str) -> &'a Value {
    config_options
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["id"] == id)
        .unwrap_or(&Value::Null)
}

/// The current value of the option `id`, when it is one Octet can use.
fn current<'a>(config_options: &'a Value, id: &str) -> Option<&'a str> {
    option(config_options, id)["currentValue"]
        .as_str()
        .filter(|value| valid_identifier(value))
}

/// The `model` option's choices as the catalog: each `value` is both what
/// selects the model and its ID, and `name` is opencode's display name.
/// Values Octet cannot use are dropped, and the list is bounded.
fn catalog(config_options: &Value) -> Vec<ModelInfo> {
    option(config_options, "model")["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            let value = choice["value"].as_str().filter(|v| valid_identifier(v))?;
            let name = choice["name"]
                .as_str()
                .filter(|name| !name.is_empty() && name.len() <= 512)
                .unwrap_or(value);
            Some(ModelInfo {
                selection: value.to_owned(),
                id: Some(value.to_owned()),
                name: name.to_owned(),
                description: String::new(),
            })
        })
        .take(MAX_MODELS)
        .collect()
}

/// `availableCommands` as skills: each with a usable name and its
/// description, bounded.
fn commands(list: &Value) -> Vec<SkillInfo> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| {
            SkillInfo::from_json(
                &json!({"name": command["name"], "description": command["description"]}),
            )
        })
        .take(super::MAX_SKILLS)
        .collect()
}

/// A tool call's `content` as text: the text of each of its blocks.
fn content_text(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.pointer("/content/text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What Octet says when `OPENCODE_CONFIG_CONTENT` is not a JSON object and
/// has to be replaced.
pub(super) const AGENT_NOTICE: &str =
    "OPENCODE_CONFIG_CONTENT is not a JSON object; Octet replaced it for this session";

/// The agent that carries `mode`. `Auto` is opencode's own `build`, whose
/// permissions are the user's.
pub(super) fn agent(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "octet-ask",
        Mode::AcceptEdits => "octet-accept-edits",
        Mode::Auto => "build",
        Mode::FullAccess => "octet-full-access",
    }
}

/// The mode `agent` carries; `None` for `plan` or an agent of the user's.
pub(super) fn mode_of(agent_name: &str) -> Option<Mode> {
    Mode::ALL
        .into_iter()
        .find(|mode| agent(*mode) == agent_name)
}

/// The arguments that start `opencode acp` in the workspace.
pub(super) fn launch_args(config: &Config) -> Vec<OsString> {
    vec![
        "acp".into(),
        "--cwd".into(),
        config.cwd.clone().into_os_string(),
    ]
}

/// The variables that start opencode, with the user's
/// `OPENCODE_CONFIG_CONTENT` read from the environment. The notice is for
/// the caller that can show one; this one drops it.
pub(super) fn launch_env(config: &Config) -> Vec<(OsString, OsString)> {
    let user = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    launch_env_from(config, user.as_deref()).0
}

/// The variables that start opencode, given the user's
/// `OPENCODE_CONFIG_CONTENT` as `user`, and a notice when that content was
/// not a JSON object and was replaced. Octet's three agents are set over the
/// user's `agent` object; everything else the user wrote is kept, including
/// a top-level `permission`, which `build` uses.
pub(super) fn launch_env_from(
    config: &Config,
    user: Option<&str>,
) -> (Vec<(OsString, OsString)>, Option<String>) {
    let (content, notice) = merged_content(user);
    let mut env: Vec<(OsString, OsString)> = vec![
        ("OPENCODE_CONFIG_CONTENT".into(), content.into()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()),
    ];
    if !config.plugins.load || !config.plugins.hooks || config.catalog_only {
        env.push(("OPENCODE_PURE".into(), "1".into()));
    }
    (env, notice)
}

/// A JSON object as its keys and values in source order. opencode applies a
/// permission map's rules last-match-wins in that order, so a merge must not
/// sort them; each value is kept as the text it was written in.
struct Ordered(Vec<(String, Box<RawValue>)>);

/// `text` without the leading whitespace JSON allows.
fn skip_space(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\n', '\r'])
}

impl Ordered {
    /// The members of `text` in source order; `None` unless it is exactly
    /// one JSON object.
    fn parse(text: &str) -> Option<Ordered> {
        let mut ordered = Ordered(Vec::new());
        let mut rest = skip_space(skip_space(text).strip_prefix('{')?);
        if let Some(end) = rest.strip_prefix('}') {
            return skip_space(end).is_empty().then_some(ordered);
        }
        loop {
            let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            let key = keys.next()?.ok()?;
            rest = skip_space(&rest[keys.byte_offset()..]);
            rest = skip_space(rest.strip_prefix(':')?);
            let mut values = serde_json::Deserializer::from_str(rest).into_iter::<&RawValue>();
            let value = values.next()?.ok()?.to_owned();
            rest = skip_space(&rest[values.byte_offset()..]);
            ordered.set(key, value);
            if let Some(more) = rest.strip_prefix(',') {
                rest = skip_space(more);
            } else {
                return skip_space(rest.strip_prefix('}')?)
                    .is_empty()
                    .then_some(ordered);
            }
        }
    }

    /// Give `key` the value `value`: where `key` already is, or at the end.
    /// A repeated key keeps its first place and its last value, as when a
    /// parser builds a map.
    fn set(&mut self, key: String, value: Box<RawValue>) {
        match self.0.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key, value)),
        }
    }

    /// The value of `key`, if it is set.
    fn get(&self, key: &str) -> Option<&RawValue> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| &**value)
    }

    /// The object as JSON text, in order, each value as it was.
    fn to_json(&self) -> String {
        let mut out = String::from("{");
        for (index, (key, value)) in self.0.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&Value::String(key.clone()).to_string());
            out.push(':');
            out.push_str(value.get());
        }
        out.push('}');
        out
    }

    /// This object as a value to put in another.
    fn into_raw(self) -> Box<RawValue> {
        raw(self.to_json())
    }
}

/// `json` as a raw value. Callers pass text they just built from valid JSON.
fn raw(json: String) -> Box<RawValue> {
    RawValue::from_string(json).expect("the text was built from valid JSON")
}

/// The `OPENCODE_CONFIG_CONTENT` text: the user's object with Octet's agents
/// set in its `agent` object, and the notice when `user` was not an object.
fn merged_content(user: Option<&str>) -> (String, Option<String>) {
    let (mut content, notice) = match user.map(Ordered::parse) {
        None => (Ordered(Vec::new()), None),
        Some(Some(object)) => (object, None),
        Some(None) => (Ordered(Vec::new()), Some(AGENT_NOTICE.to_owned())),
    };
    let mut agents = content
        .get("agent")
        .and_then(|agents| Ordered::parse(agents.get()))
        .unwrap_or(Ordered(Vec::new()));
    for (name, definition) in OCTET_AGENTS {
        agents.set((*name).to_owned(), raw((*definition).to_owned()));
    }
    content.set("agent".to_owned(), agents.into_raw());
    (content.to_json(), notice)
}

/// The agents Octet defines for its `ask`, `accept-edits` and `full-access`
/// modes, as JSON text. Reads are allowed everywhere, since they change
/// nothing. Each permission map starts with `"*"`: rules are last-match-wins,
/// so the entries after it refine it.
const OCTET_AGENTS: &[(&str, &str)] = &[
    (
        "octet-ask",
        r#"{"mode":"primary","description":"Octet: ask before edits and commands",
        "permission":{"*":"ask","read":"allow","glob":"allow","grep":"allow","list":"allow",
        "lsp":"allow","todoread":"allow","todowrite":"allow","skill":"allow"}}"#,
    ),
    (
        "octet-accept-edits",
        r#"{"mode":"primary","description":"Octet: edits proceed, other actions ask",
        "permission":{"*":"ask","edit":"allow","read":"allow","glob":"allow","grep":"allow",
        "list":"allow","lsp":"allow","todoread":"allow","todowrite":"allow","skill":"allow"}}"#,
    ),
    (
        "octet-full-access",
        r#"{"mode":"primary","description":"Octet: no permission checks",
        "permission":{"*":"allow"}}"#,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Config, Engine, Mode, Plugins};
    use serde_json::{Value, json};
    use std::ffi::OsString;

    /// A configuration in `/w` with the default switches.
    fn config() -> Config {
        Config::new(Engine::OPENCODE, "opencode", "/w")
    }

    /// The value of `name` in `env`, if it is set.
    fn var(env: &[(OsString, OsString)], name: &str) -> Option<String> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    /// `configOptions` whose `model` option offers `choices`.
    fn model_options(choices: &Value) -> Value {
        json!([
            {"id": "mode", "currentValue": "build", "options": [{"value": "build", "name": "build"}]},
            {"id": "model", "currentValue": "a/b", "options": choices},
        ])
    }

    #[test]
    fn a_skill_prompt_is_its_slash_command_after_any_context() {
        assert_eq!(skill_prompt("review", "src", None), "/review src");
        assert_eq!(skill_prompt("review", "", None), "/review");
        assert_eq!(skill_prompt("review", "  a b ", None), "/review   a b");
        assert_eq!(skill_prompt("review", "go", Some("")), "/review go");
        assert_eq!(
            skill_prompt("review", "go", Some("earlier")),
            "earlier\n\n/review go"
        );
    }

    #[test]
    fn compaction_is_offered_when_the_command_list_holds_it() {
        let list = json!([{"name": "init"}, {"name": "compact"}]);
        assert!(offers(&list, "compact"));
        assert!(!offers(&json!([{"name": "init"}]), "compact"));
        assert!(!offers(&json!([]), "compact"));
        assert!(!offers(&Value::Null, "compact"));
    }

    #[test]
    fn the_catalog_is_the_model_options_values_and_names() {
        let models = catalog(&model_options(&json!([
            {"value": "opencode/free", "name": "OpenCode/Free"},
            {"value": "bad\u{1b}id", "name": "Bad"},
            {"value": "x".repeat(257), "name": "Long"},
            {"value": "openai/gpt", "name": ""},
            {"name": "No value"},
        ])));
        assert_eq!(
            models,
            [
                ModelInfo {
                    selection: "opencode/free".into(),
                    id: Some("opencode/free".into()),
                    name: "OpenCode/Free".into(),
                    description: String::new(),
                },
                ModelInfo {
                    selection: "openai/gpt".into(),
                    id: Some("openai/gpt".into()),
                    name: "openai/gpt".into(),
                    description: String::new(),
                },
            ]
        );
        let many: Vec<Value> = (0..300)
            .map(|n| json!({"value": format!("p/m{n}"), "name": "M"}))
            .collect();
        assert_eq!(catalog(&model_options(&Value::Array(many))).len(), 256);
        assert!(catalog(&json!([{"id": "mode", "options": []}])).is_empty());
        assert!(catalog(&Value::Null).is_empty());
    }

    #[test]
    fn current_values_are_read_from_their_option() {
        let options = model_options(&json!([]));
        assert_eq!(current(&options, "model"), Some("a/b"));
        assert_eq!(current(&options, "mode"), Some("build"));
        assert_eq!(current(&options, "effort"), None);
    }

    #[test]
    fn opencode_stray_requests_are_refused() {
        for method in ["fs/read_text_file", "terminal/create", "x/unknown"] {
            let reply =
                opencode_stray_reply(&json!({"jsonrpc":"2.0","id":4,"method":method})).unwrap();
            assert_eq!(reply["jsonrpc"], "2.0");
            assert_eq!(reply["id"], 4);
            assert_eq!(reply["error"]["code"], -32601);
            assert_eq!(
                reply["error"]["message"],
                format!("Octet does not support {method}")
            );
        }
        let options = json!([
            {"optionId": "yes", "kind": "allow_once"},
            {"optionId": "ever", "kind": "allow_always"},
            {"optionId": "no", "kind": "reject_once"},
        ]);
        let permission = json!({"jsonrpc":"2.0","id":5,"method":"session/request_permission",
            "params":{"sessionId":"s","options":options}});
        let reply = opencode_stray_reply(&permission).unwrap();
        assert_eq!(
            reply,
            json!({"jsonrpc":"2.0","id":5,"result":{"outcome":{"outcome":"selected","optionId":"no"}}})
        );
        // opencode's own option IDs: an out-of-turn request is rejected.
        let options = json!([
            {"optionId": "once", "kind": "allow_once"},
            {"optionId": "always", "kind": "allow_always"},
            {"optionId": "reject", "kind": "reject_once"},
        ]);
        let permission = json!({"jsonrpc":"2.0","id":6,"method":"session/request_permission",
            "params":{"sessionId":"s","options":options}});
        assert_eq!(
            opencode_stray_reply(&permission).unwrap()["result"]["outcome"]["optionId"],
            "reject"
        );
        // Notifications and replies are not requests: they get no reply.
        assert_eq!(
            opencode_stray_reply(&json!({"method":"session/update"})),
            None
        );
        assert_eq!(opencode_stray_reply(&json!({"id":1,"result":{}})), None);
    }

    #[test]
    fn an_answer_picks_the_requests_once_option_never_always() {
        let options = json!([
            {"optionId": "ever", "kind": "allow_always"},
            {"optionId": "yes", "kind": "allow_once"},
            {"optionId": "no", "kind": "reject_once"},
        ]);
        let wire = json!({"id": 9, "params": {"options": options}});
        let chosen = |allow| answer_wire(&wire, allow)["result"]["outcome"]["optionId"].clone();
        assert_eq!(chosen(true), "yes");
        assert_eq!(chosen(false), "no");
        // A request without options gets opencode's usual IDs.
        let bare = json!({"id": 9, "params": {}});
        assert_eq!(
            answer_wire(&bare, true)["result"]["outcome"]["optionId"],
            "once"
        );
        assert_eq!(
            answer_wire(&bare, false)["result"]["outcome"]["optionId"],
            "reject"
        );
    }

    #[test]
    fn an_approval_shows_the_tool_call_and_what_it_would_do() {
        let (detail, summary) = approval(&json!({
            "kind": "execute", "title": "rm -rf build",
            "rawInput": {"command": "rm -rf build"}, "locations": [],
        }));
        assert_eq!(
            detail,
            "execute · rm -rf build\n{\n  \"command\": \"rm -rf build\"\n}"
        );
        assert_eq!(summary.action, "Run a shell command");
        assert_eq!(summary.command.as_deref(), Some("rm -rf build"));
        assert!(summary.files.is_empty());

        let (detail, summary) = approval(&json!({
            "kind": "edit", "title": "src/a.rs",
            "locations": [{"path": "/w/src/a.rs"}, {"path": "/w/src/b.rs"}, {"line": 3}],
        }));
        assert_eq!(detail, "edit · src/a.rs");
        assert_eq!(summary.action, "Edit files");
        assert_eq!(summary.files, ["/w/src/a.rs", "/w/src/b.rs"]);
        assert_eq!(summary.command, None);

        for call in [
            json!({"kind": "fetch", "title": "https://example.com", "rawInput": {}}),
            json!({}),
        ] {
            let (_, summary) = approval(&call);
            assert_eq!(summary.action, "Use a tool");
            assert_eq!(summary.command, None);
        }
        assert_eq!(approval(&json!({})).0, "other · tool");
    }

    #[test]
    fn every_mode_has_an_agent_and_back() {
        for mode in Mode::ALL {
            assert_eq!(mode_of(agent(mode)), Some(mode));
        }
        assert_eq!(mode_of("plan"), None);
        assert_eq!(mode_of("mine"), None);
    }

    #[test]
    fn the_launch_is_acp_in_the_workspace() {
        assert_eq!(launch_args(&config()), ["acp", "--cwd", "/w"]);
    }

    #[test]
    fn octet_agents_win_over_the_users_and_the_rest_is_kept() {
        let user = r#"{"agent":{"octet-ask":{"permission":{"*":"allow"}},"mine":{}},
            "model":"x","permission":{"bash":"allow"}}"#;
        let (env, notice) = launch_env_from(&config(), Some(user));
        assert_eq!(notice, None);
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(content["agent"]["octet-ask"]["permission"]["*"], "ask");
        assert_eq!(
            content["agent"]["octet-full-access"]["permission"]["*"],
            "allow"
        );
        assert_eq!(
            content["agent"]["octet-accept-edits"]["permission"]["edit"],
            "allow"
        );
        assert_eq!(content["agent"]["mine"], json!({}));
        assert_eq!(content["model"], "x");
        assert_eq!(content["permission"]["bash"], "allow");
    }

    #[test]
    fn content_that_is_not_an_object_is_replaced_with_a_notice() {
        let (env, notice) = launch_env_from(&config(), Some("[1]"));
        assert_eq!(notice.as_deref(), Some(AGENT_NOTICE));
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(
            content.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["agent"]
        );
        // Content that is not JSON at all is replaced the same way.
        let (_, notice) = launch_env_from(&config(), Some("{oops"));
        assert_eq!(notice.as_deref(), Some(AGENT_NOTICE));
    }

    #[test]
    fn absent_content_gives_the_agents_alone_and_no_notice() {
        let (env, notice) = launch_env_from(&config(), None);
        assert_eq!(notice, None);
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(
            content.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["agent"]
        );
        assert_eq!(content["agent"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn project_config_is_always_off_and_plugins_follow_the_switches() {
        let pure = |config: &Config| var(&launch_env_from(config, None).0, "OPENCODE_PURE");
        let disabled = |config: &Config| {
            var(
                &launch_env_from(config, None).0,
                "OPENCODE_DISABLE_PROJECT_CONFIG",
            )
        };
        let mut c = config();
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), None);
        c.plugins = Plugins {
            load: false,
            hooks: true,
        };
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), Some("1".into()));
        c.plugins = Plugins {
            load: true,
            hooks: false,
        };
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), Some("1".into()));
        c.plugins = Plugins::default();
        c.catalog_only = true;
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), Some("1".into()));
    }

    #[test]
    fn the_users_key_order_survives_the_merge() {
        // opencode's rules are last-match-wins in object order, so a re-sort
        // would change what the user's own configuration decides.
        let user = r#"{"permission":{"rm *":"deny","*":"allow"},
            "agent":{"mine":{"permission":{"z":"ask","a":"allow"}}},"model":"x"}"#;
        let (env, notice) = launch_env_from(&config(), Some(user));
        assert_eq!(notice, None);
        let text = var(&env, "OPENCODE_CONFIG_CONTENT").unwrap();
        let before = |first: &str, second: &str| {
            let at = |needle: &str| {
                text.find(needle)
                    .unwrap_or_else(|| panic!("{needle} in {text}"))
            };
            assert!(at(first) < at(second), "{first} before {second} in {text}");
        };
        before(r#""rm *":"deny""#, r#""*":"allow""#);
        before(r#""z":"ask""#, r#""a":"allow""#);
        // The top-level keys keep their places too, and the agents are added.
        before(r#""permission""#, r#""agent""#);
        before(r#""agent""#, r#""model""#);
        let content: Value = serde_json::from_str(&text).unwrap();
        for name in [
            "octet-ask",
            "octet-accept-edits",
            "octet-full-access",
            "mine",
        ] {
            assert!(content["agent"][name].is_object(), "{name}");
        }
        assert_eq!(content["model"], "x");
    }

    #[test]
    fn octet_permission_maps_start_with_the_catch_all() {
        let (env, _) = launch_env_from(&config(), None);
        let text = var(&env, "OPENCODE_CONFIG_CONTENT").unwrap();
        // Last match wins, so a leading "*" is what lets the entries after it
        // refine it.
        assert_eq!(text.matches(r#""permission":{"*":""#).count(), 3, "{text}");
    }

    #[test]
    fn only_one_json_object_is_parsed_as_the_users_content() {
        let keys = |text: &str| {
            Ordered::parse(text).map(|o| {
                o.0.into_iter()
                    .map(|(k, v)| (k, v.get().to_owned()))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(keys(" { } "), Some(vec![]));
        assert_eq!(
            keys(r#" {"b" : [1, {"x":"}"}] , "a":"\"q\"" , "b":2 } "#),
            Some(vec![
                ("b".into(), "2".into()),
                ("a".into(), r#""\"q\"""#.into())
            ])
        );
        for bad in [
            "",
            "[1]",
            "{",
            r#"{"a":1,}"#,
            r#"{"a":1} x"#,
            r#"{"a" 1}"#,
            "{1:2}",
        ] {
            assert!(Ordered::parse(bad).is_none(), "{bad}");
        }
    }
}
