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
    short, valid_identifier,
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
        "opencode asks before edits, commands and other actions; subagents are off (agent octet-ask)",
        "Edits proceed; commands and other actions ask; subagents are off (agent octet-accept-edits)",
        "opencode's own permission settings decide (agent build; its defaults ask for nothing)",
        "No permission checks at all (agent octet-full-access)",
    ],
    steer: false,
    mode_applies_next_turn: true,
    inline_images: true,
    // Levels depend on the model (its variants) and change live.
    effort_live: true,
    takes_effort: true,
    ids_carry_name: true,
    efforts: &[],
    // opencode 1.18.25 exits within 35 ms once its input closes, idle or
    // mid-turn, with or without plugins.
    shutdown_grace: Duration::from_millis(250),
    lists_models: true,
    // opencode 1.18.25's `command()` runs each `` !`cmd` `` of the expanded
    // template in the shell, before any agent or permission applies.
    commands_expand_shell: true,
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
/// The most effort levels one model's option keeps.
const MAX_EFFORTS: usize = 32;
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
    /// The agent that carries a mode, set in the open session at the
    /// user's request.
    Switch(Switch),
    /// An effort level: what the user asked for (`None`: the model's
    /// default) and the value sent for it.
    SetEffort(EffortRequest),
    /// `session/prompt`: its reply ends the turn.
    Prompt,
}

/// An effort level set in the session.
struct EffortRequest {
    /// The level Octet records once opencode confirms it; `None` is the
    /// model's default.
    level: Option<String>,
    /// The value sent: the level, or the model's default level.
    value: String,
    /// Set during the handshake, which goes on to the agent afterwards.
    handshake: bool,
}

/// An effort level asked for while the session connects.
struct Wanted {
    /// The level; `None` is the model's default.
    level: Option<String>,
}

/// What Octet knows of the current model's effort levels.
#[derive(Default)]
enum Efforts {
    /// Nothing yet: no options reported since the model changed.
    #[default]
    Unknown,
    /// opencode reported options without an `effort` one: the model has
    /// no levels.
    Absent,
    /// The model's `effort` option.
    Listed(EffortOption),
}

impl Efforts {
    /// The model's `effort` option, when it has one.
    fn listed(&self) -> Option<&EffortOption> {
        match self {
            Efforts::Listed(option) => Some(option),
            Efforts::Unknown | Efforts::Absent => None,
        }
    }
}

/// The current model's `effort` option, as opencode last reported it.
struct EffortOption {
    /// The model's levels, in opencode's order, bounded.
    values: Vec<String>,
    /// The level in force.
    current: Option<String>,
    /// The level the model started at in this session: what
    /// `/effort default` goes back to.
    default: Option<String>,
}

/// A live mode switch, as it was sent.
#[derive(Clone, Copy)]
struct Switch {
    /// The mode the user asked for.
    target: Mode,
    /// A turn was running when the switch was sent. opencode applies an
    /// agent at the next `session/prompt`, so only then does the running
    /// turn keep the previous mode; a switch sent while idle reaches the
    /// next prompt, which the driver holds until the switch resolves.
    mid_turn: bool,
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
    /// The wire IDs and targets of timed-out switches opencode may still
    /// confirm, oldest first: the header must follow the agent opencode is
    /// really in, and while any is unanswered that agent is unknown.
    late_modes: VecDeque<(u64, Mode)>,
    /// The last switch was reported unconfirmed and the driver has not
    /// asked yet (see `Protocol::take_switch_timed_out`).
    switch_timed_out: bool,
    /// A `session/cancel` was sent this turn. opencode ends a cancelled
    /// prompt with `end_turn`, so this is what makes it an interruption.
    cancel_sent: bool,
    /// The session's model (`currentValue` of its `model` option).
    model: Option<String>,
    /// The session's agent (`currentValue` of its `mode` option).
    agent: Option<String>,
    /// The current model's effort levels, as opencode last reported them.
    effort: Efforts,
    /// Each tool call's name and target from its first frame, this turn.
    tools: HashMap<String, (String, String)>,
    /// A probe's session is open and its commands are awaited until this
    /// moment; `None` once the probe has been answered.
    probe_wait: Option<Instant>,
    /// The probe's session has been deleted (or its deletion started).
    probe_deleted: bool,
    /// The handshake's effort step has run: a level asked for before the
    /// session is ready no longer reaches it.
    effort_stepped: bool,
    /// The newest level asked for after the effort step and before the
    /// session is ready. It is set through the live path once the
    /// handshake ends, and no handshake confirmation is shown over it.
    effort_wanted: Option<Wanted>,
    /// The mode the running turn's prompt was sent under: opencode runs a
    /// prompt under the agent it started with.
    turn_mode: Option<Mode>,
    /// Why Octet cancelled the running turn: it started a subagent that
    /// cannot ask for approval (see `stop_subtask`).
    subtask_stopped: Option<String>,
}

/// opencode 1.18.25's commands that always run in a subagent: `review` is
/// built in with `subtask: true`.
const SUBTASK_COMMANDS: &[&str] = &["review"];

/// Why opencode's command `name` (`None`: one it did not name) cannot run
/// as a subagent under `mode`, ending with `done`, what Octet did about it;
/// `None` when `mode` lets opencode decide. opencode passes on only the
/// approval requests of the sessions opened over ACP: a subagent's would
/// wait for ever, unseen.
fn subtask_refusal(name: Option<&str>, mode: Mode, done: &str) -> Option<String> {
    let command = name.map_or_else(
        || "a command".to_owned(),
        |name| format!("/{}", short(name)),
    );
    matches!(mode, Mode::Ask | Mode::AcceptEdits).then(|| {
        format!(
            "opencode runs {command} in a subagent, whose approval requests it never passes on to Octet; \
             subagents are off in {}, so {done}. In auto, opencode's own permission settings decide",
            mode.label()
        )
    })
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

    fn mode_unsettled(&self) -> Option<Mode> {
        self.late_modes.back().map(|(_, target)| *target)
    }

    fn take_switch_timed_out(&mut self) -> bool {
        std::mem::take(&mut self.switch_timed_out)
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
        self.turn_mode = None;
        self.subtask_stopped = None;
    }

    /// A probe that ends before it has its catalog (an error, or the
    /// caller giving up) still deletes the session it opened.
    fn stopping(&mut self, core: &Core) {
        if core.config.catalog_only {
            self.delete_probe_session(core);
        }
    }

    /// Says when the user's `OPENCODE_CONFIG_CONTENT` had to be ignored or
    /// replaced, then sends `initialize`, offering no client capabilities.
    async fn initialize(&mut self, core: &mut Core) -> Result<(), DriverError> {
        let (user, unreadable) = user_content();
        if let Some(notice) = unreadable {
            core.emit(Event::Notice(notice.to_owned()))?;
        }
        if let Some(notice) = launch_env_from(&core.config, user.as_deref()).1 {
            core.emit(Event::Notice(notice))?;
        }
        let params = json!({"protocolVersion": 1, "clientCapabilities": {}});
        self.request(core, Outstanding::Initialize, "initialize", params)
            .await
    }

    /// Images go inline after the text (see `send_blocks`).
    async fn send_prompt(
        &mut self,
        core: &mut Core,
        text: &str,
        images: &[ImageAttachment],
    ) -> Result<(), DriverError> {
        self.send_blocks(core, vec![json!({"type": "text", "text": text})], images)
            .await
    }

    /// A skill is one of opencode's slash commands, `/name args`. Context is
    /// never sent with it: opencode 1.18.25 joins every text block of the
    /// prompt into the command's arguments, substitutes them into its
    /// template and runs each `` !`cmd` `` of the result in its shell before
    /// any permission applies, and earlier conversation holds replies and
    /// tool output. The interface keeps it for the next prompt instead.
    async fn send_skill(
        &mut self,
        core: &mut Core,
        name: &str,
        _path: Option<&Path>,
        args: &str,
        context: Option<&str>,
    ) -> Result<(), DriverError> {
        if context.is_some_and(|context| !context.is_empty()) {
            core.emit(Event::Notice(format!(
                "opencode runs a command's text through its shell, so Octet did not send the earlier \
                 conversation with it (/{})",
                short(name)
            )))?;
        }
        self.send_blocks(core, skill_prompt(name, args), &[]).await
    }

    /// Compaction is the prompt `/compact`. opencode 1.18.25 never lists that
    /// command, yet summarizes the session for it (`session.summarize`) when
    /// no command of that name is listed; one that is runs instead.
    async fn compact(&mut self, core: &mut Core) -> Result<(), DriverError> {
        self.send_prompt(core, "/compact", &[]).await
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
    /// opencode confirms it. While [`MAX_LATE_SWITCHES`] earlier switches
    /// wait unconfirmed, another is refused: a late reply to it could not
    /// be matched, and the header must follow the agent opencode is in.
    async fn set_mode(&mut self, core: &mut Core, target: Mode) -> Result<(), DriverError> {
        if self.late_modes.len() >= MAX_LATE_SWITCHES {
            core.emit(Event::Notice(format!(
                "opencode has not confirmed {MAX_LATE_SWITCHES} earlier mode switches; wait or reconnect"
            )))?;
            return core.emit(Event::ModeChanged(core.mode));
        }
        self.switch_timed_out = false;
        let switch = Switch {
            target,
            mid_turn: core.phase.is_running(),
        };
        self.set_option(core, Outstanding::Switch(switch), "mode", agent(target))
            .await?;
        self.mode_request = Some(ModeRequest {
            id: self.next_id,
            target,
            deadline: deadline_after(core.limits.mode_confirm),
        });
        Ok(())
    }

    /// Sets the effort option, live. A level the model does not list is
    /// refused here, unsent, and the interface goes back to the level in
    /// force. `/effort default` sets the level the model started at, when
    /// opencode reported one; otherwise opencode keeps its level, and with
    /// levels not yet reported Octet says so instead of claiming a reset.
    async fn set_effort(
        &mut self,
        core: &mut Core,
        effort: Option<String>,
    ) -> Result<bool, DriverError> {
        if !core.phase.is_ready() {
            if self.effort_stepped {
                // Set once the handshake ends; the newest request wins.
                self.effort_wanted = Some(Wanted { level: effort });
            } else {
                // The handshake's effort step takes it.
                core.config.effort = effort;
            }
            return Ok(true);
        }
        let option = self.effort.listed();
        let request = match effort {
            Some(level) => {
                if let Some(refusal) = self.effort_refusal(&level) {
                    core.emit(Event::Notice(refusal))?;
                    core.emit(Event::Effort(core.config.effort.clone()))?;
                    return Ok(true);
                }
                EffortRequest {
                    value: level.clone(),
                    level: Some(level),
                    handshake: false,
                }
            }
            None if matches!(self.effort, Efforts::Unknown) => {
                // Nothing says where the model started: claiming its default
                // without sending one would be untrue.
                const WHY: &str = "it has not reported its model's default level";
                let notice = match &core.config.effort {
                    Some(level) => format!("opencode keeps effort {}: {WHY}", short(level)),
                    None => format!("opencode keeps its current effort level: {WHY}"),
                };
                core.emit(Event::Notice(notice))?;
                core.emit(Event::Effort(core.config.effort.clone()))?;
                return Ok(true);
            }
            None => match option.map(|option| (&option.current, &option.default)) {
                Some((current, Some(default))) if current.as_ref() != Some(default) => {
                    EffortRequest {
                        value: default.clone(),
                        level: None,
                        handshake: false,
                    }
                }
                Some((Some(current), None)) => {
                    let notice = format!(
                        "opencode keeps effort {} until the model changes",
                        short(current)
                    );
                    core.config.effort = None;
                    core.emit(Event::Notice(notice))?;
                    return Ok(true);
                }
                _ => {
                    // Already at the model's own level, or it has none.
                    core.config.effort = None;
                    core.emit(Event::Effort(None))?;
                    core.emit(Event::Notice(super::driver::effort_notice(
                        None,
                        core.phase.is_running(),
                    )))?;
                    return Ok(true);
                }
            },
        };
        self.send_effort(core, request).await?;
        Ok(true)
    }

    async fn on_frame(&mut self, core: &mut Core, v: Value) -> Result<(), DriverError> {
        let method = v["method"].as_str().unwrap_or("");
        if method.is_empty() {
            return self.response(core, &v).await;
        }
        if v.get("id").is_some() {
            // What the request's tool call named earlier this turn.
            let known = v
                .pointer("/params/toolCall/toolCallId")
                .and_then(Value::as_str)
                .and_then(|id| self.tools.get(&one_line(id)))
                .map(|(_, target)| target.clone())
                .filter(|target| !target.is_empty());
            return server_request(core, v, known.as_deref()).await;
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
            "tool_call" | "tool_call_update" => {
                self.tool(core, update)?;
                self.stop_subtask(core, update).await
            }
            _ => Ok(()),
        }
    }
}

impl OpencodeProtocol {
    /// Cancels the running turn when `update` is a subagent's `task` call
    /// (opencode's only `think` tool) and the turn's mode asks: opencode
    /// never passes the subagent's approval requests on, so it would wait
    /// for ever. The asking agents deny `task` to the model, so such a call
    /// comes from a command that runs as a subagent. The turn then ends,
    /// failed, saying why (see `prompt_ended`).
    async fn stop_subtask(&mut self, core: &mut Core, update: &Value) -> Result<(), DriverError> {
        if update["kind"] != "think"
            || core.phase != Phase::InTurn
            || self.subtask_stopped.is_some()
        {
            return Ok(());
        }
        let name = update
            .pointer("/rawInput/command")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        let Some(refusal) = self
            .turn_mode
            .and_then(|mode| subtask_refusal(name, mode, "Octet stopped the turn"))
        else {
            return Ok(());
        };
        self.subtask_stopped = Some(refusal);
        core.phase = Phase::Interrupting;
        <Self as Protocol>::interrupt(self, core).await?;
        core.deadline = deadline_after(core.limits.interrupt);
        core.deny_all_pending().await
    }

    /// Sends the prompt `prompt`, with `images` inline as base64 blocks
    /// after it, read now; an image that cannot be read fails the turn, and
    /// so, while the mode asks, does a command that always runs in a
    /// subagent (see `subtask_refusal`) or whose arguments hold shell (see
    /// `shell_refusal`).
    async fn send_blocks(
        &mut self,
        core: &mut Core,
        mut prompt: Vec<Value>,
        images: &[ImageAttachment],
    ) -> Result<(), DriverError> {
        if let Some((name, args)) = slash_command(&prompt) {
            let refusal = shell_refusal(&name, &args, core.mode).or_else(|| {
                SUBTASK_COMMANDS
                    .contains(&name.as_str())
                    .then(|| subtask_refusal(Some(&name), core.mode, "Octet did not send it"))
                    .flatten()
            });
            if let Some(refusal) = refusal {
                return fail_turn(core, refusal);
            }
        }
        self.turn_mode = Some(core.mode);
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

    /// Why opencode's model cannot take `level`, from its last reported
    /// options; `None` when it lists the level or its levels are unknown.
    fn effort_refusal(&self, level: &str) -> Option<String> {
        let model = short(self.model.as_deref().unwrap_or("default"));
        match &self.effort {
            Efforts::Absent => Some(format!("opencode's model {model} has no effort levels")),
            Efforts::Listed(option) if !option.values.iter().any(|value| value == level) => {
                let levels = match option.values.split_last() {
                    Some((last, [])) => last.clone(),
                    Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                    None => "no levels".to_owned(),
                };
                Some(format!("opencode's model {model} takes effort {levels}"))
            }
            // Unknown levels are sent, and opencode decides.
            Efforts::Unknown | Efforts::Listed(_) => None,
        }
    }

    /// The handshake's effort step, after the model and before the agent:
    /// the level the user asked for, when the model takes it. A level it
    /// does not take is dropped with a notice; effort is not a safety
    /// property, so the session carries on either way.
    async fn effort_step(&mut self, core: &mut Core) -> Result<(), DriverError> {
        self.effort_stepped = true;
        if let Some(level) = core.config.effort.clone() {
            let current = self
                .effort
                .listed()
                .and_then(|option| option.current.as_deref());
            if let Some(refusal) = self.effort_refusal(&level) {
                core.emit(Event::Notice(refusal))?;
                core.config.effort = None;
                core.emit(Event::Effort(None))?;
            } else if current != Some(level.as_str()) {
                let request = EffortRequest {
                    value: level.clone(),
                    level: Some(level),
                    handshake: true,
                };
                return self.send_effort(core, request).await;
            }
        }
        self.set_agent(core).await
    }

    /// Sends `request`'s `effort` value.
    async fn send_effort(
        &mut self,
        core: &Core,
        request: EffortRequest,
    ) -> Result<(), DriverError> {
        let value = request.value.clone();
        self.set_option(core, Outstanding::SetEffort(request), "effort", &value)
            .await
    }

    /// An effort reply. A confirmation records the level and tells the
    /// interface; a refusal is reported and the interface goes back to the
    /// level in force. The handshake then sets the agent; while a newer
    /// level waits for the handshake's end, the interface keeps showing it.
    async fn effort_set(
        &mut self,
        core: &mut Core,
        v: &Value,
        request: EffortRequest,
    ) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            core.emit(Event::Notice(format!(
                "opencode did not set effort {}: {}",
                short(&request.value),
                error_text(error)
            )))?;
            if request.handshake {
                core.config.effort = None;
            }
            if !request.handshake || self.effort_wanted.is_none() {
                core.emit(Event::Effort(core.config.effort.clone()))?;
            }
        } else {
            let options = &v["result"]["configOptions"];
            if options.is_array() {
                self.note_options(options);
            } else if let Efforts::Listed(option) = &mut self.effort {
                // A reply without the options still confirms the value sent:
                // `/effort default` must know the level moved.
                option.current = Some(request.value.clone());
            }
            core.config.effort.clone_from(&request.level);
            if !request.handshake || self.effort_wanted.is_none() {
                core.emit(Event::Effort(request.level.clone()))?;
            }
            if !request.handshake {
                core.emit(Event::Notice(super::driver::effort_notice(
                    request.level.as_deref(),
                    core.phase.is_running(),
                )))?;
            }
        }
        if request.handshake {
            self.set_agent(core).await
        } else {
            Ok(())
        }
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
            Outstanding::SetMode(target) => self.mode_set(core, v, target).await,
            Outstanding::Switch(switch) => self.switched(core, v, switch),
            Outstanding::SetEffort(request) => self.effort_set(core, v, request).await,
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
            _ => self.effort_step(core).await,
        }
    }

    /// The handshake's last step: the agent that carries the session's mode.
    async fn set_agent(&mut self, core: &Core) -> Result<(), DriverError> {
        let target = core.mode;
        self.set_option(core, Outstanding::SetMode(target), "mode", agent(target))
            .await
    }

    /// The model's reply; the effort and the agent are set next. A refusal
    /// stops the session: going on would run the user's prompts on
    /// opencode's own default, a model (and perhaps a bill) they never
    /// chose. A new model may start at another level, so its levels are
    /// what its reply reports.
    async fn model_set(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        let requested = core.config.model.clone().unwrap_or_default();
        if let Some(error) = v.get("error") {
            return Err(format!(
                "opencode refused the model {}: {}",
                short(&requested),
                error_text(error)
            )
            .into());
        }
        // A reply without the options still confirms the request.
        self.model = Some(requested).filter(|model| valid_identifier(model));
        self.effort = Efforts::Unknown;
        self.note_options(&v["result"]["configOptions"]);
        self.effort_step(core).await
    }

    /// The agent's reply ends the handshake: ready, with the model and the
    /// mode, after setting a level the user asked for meanwhile (when it is
    /// not the one opencode confirmed). An agent opencode refuses, or a reply
    /// naming another agent, stops the session: whatever agent it kept may
    /// ask for less than the mode the user chose, so it must never act
    /// under it.
    async fn mode_set(
        &mut self,
        core: &mut Core,
        v: &Value,
        target: Mode,
    ) -> Result<(), DriverError> {
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
        if let Some(Wanted { level }) = self.effort_wanted.take()
            && level != core.config.effort
        {
            <Self as Protocol>::set_effort(self, core, level).await?;
        }
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
    /// reply after the switch timed out is handled the same way. opencode
    /// applies an agent at the next `session/prompt`, so a switch sent
    /// mid-turn, and confirmed while that turn runs, says the running turn
    /// keeps its mode.
    fn switched(&mut self, core: &mut Core, v: &Value, switch: Switch) -> Result<(), DriverError> {
        let Switch { target, mid_turn } = switch;
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
        core.emit(Event::ModeChanged(target))?;
        if mid_turn && core.phase.is_running() {
            core.emit(Event::Notice(
                super::mode_next_turn_notice(PROVIDER.steer).into(),
            ))?;
        }
        Ok(())
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
        // Kept bounded: `set_mode` refuses a switch while the list is full,
        // so a vendor that never answers cannot grow it or `requests`.
        self.late_modes.push_back((request.id, request.target));
        self.switch_timed_out = true;
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
        self.finish_probe(core)
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
        self.finish_probe(core)
    }

    /// The probe has what it came for: it says so, and its session, which
    /// only read the catalog, is deleted so the user's history keeps no
    /// empty session.
    fn finish_probe(&mut self, core: &Core) -> Result<(), DriverError> {
        self.delete_probe_session(core);
        core.emit(Event::Ready {
            session: String::new(),
        })
    }

    /// Deletes the probe's session once, when it has one.
    fn delete_probe_session(&mut self, core: &Core) {
        if !self.probe_deleted && !core.session.is_empty() {
            self.probe_deleted = true;
            delete_session(&core.config, &core.session);
        }
    }

    /// Records the model, agent and effort levels `config_options` reports
    /// as current. Options without an `effort` one mean the model has no
    /// levels; a model's first report of its level is its default.
    fn note_options(&mut self, config_options: &Value) {
        let Some(list) = config_options.as_array() else {
            return;
        };
        let model = current(config_options, "model").map(str::to_owned);
        let changed = model.is_some() && model != self.model;
        if let Some(model) = model {
            self.model = Some(model);
        }
        if let Some(agent) = current(config_options, "mode") {
            self.agent = Some(agent.to_owned());
        }
        let Some(effort) = list.iter().find(|option| option["id"] == "effort") else {
            self.effort = Efforts::Absent;
            return;
        };
        let values: Vec<String> = effort["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|choice| choice["value"].as_str())
            .filter(|value| valid_identifier(value))
            .take(MAX_EFFORTS)
            .map(str::to_owned)
            .collect();
        let current = effort["currentValue"]
            .as_str()
            .filter(|value| valid_identifier(value))
            .map(str::to_owned);
        let default = match std::mem::take(&mut self.effort) {
            Efforts::Listed(known) if !changed => known.default,
            _ => current.clone(),
        };
        self.effort = Efforts::Listed(EffortOption {
            values,
            current,
            default,
        });
    }

    /// The prompt's reply ends the turn, with its token count when it has one.
    fn prompt_ended(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if !core.phase.is_running() {
            return Ok(());
        }
        self.tools.clear();
        if let Some(refusal) = self.subtask_stopped.take() {
            return core.finish_turn(Outcome::Failed, Some(refusal));
        }
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
    /// name is the title of its first frame; its target is the newest one a
    /// frame names (see `frame_target`).
    fn tool(&mut self, core: &Core, update: &Value) -> Result<(), DriverError> {
        let id = one_line(update["toolCallId"].as_str().unwrap_or_default());
        let target = frame_target(update);
        let (name, target) = if let Some((name, known)) = self.tools.get_mut(&id) {
            if let Some(target) = target {
                *known = target;
            }
            (name.clone(), known.clone())
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

/// What a tool call's frame says the call acts on, in one line: a command's
/// command (its location is only the folder it runs in), else the first path
/// it touches, else its command; `None` when the frame names none of them.
fn frame_target(update: &Value) -> Option<String> {
    let command = update.pointer("/rawInput/command").and_then(Value::as_str);
    let target = if update["kind"] == "execute" {
        command
    } else {
        update
            .pointer("/locations/0/path")
            .and_then(Value::as_str)
            .or(command)
    };
    target.map(one_line).filter(|target| !target.is_empty())
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

/// The prompt blocks that run the slash command `name`: `/name args`,
/// trimmed, in one block. opencode 1.18.25 runs a command only when the
/// prompt's text starts with `/`.
fn skill_prompt(name: &str, args: &str) -> Vec<Value> {
    let command = format!("/{name} {args}");
    vec![json!({"type": "text", "text": command.trim()})]
}

/// The command `prompt` runs and its arguments, read as opencode 1.18.25's
/// ACP agent reads them: every text block joined as it is, trimmed; a
/// command when that starts with `/` and a name follows at once; the
/// arguments are the words after the name, one space apart. `None` for a
/// plain prompt.
fn slash_command(prompt: &[Value]) -> Option<(String, String)> {
    let text: String = prompt
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    let rest = text.trim().strip_prefix('/')?;
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let name = words.next()?.to_owned();
    Some((name, words.collect::<Vec<_>>().join(" ")))
}

/// opencode's mark for a shell command that a command's text runs: each
/// `` !`cmd` `` of the expanded template is replaced by `cmd`'s output.
const SHELL_MARK: &str = "!`";

/// Why the command `name` cannot be sent with `args` under `mode`: the
/// arguments hold [`SHELL_MARK`], which opencode would run in its shell
/// before any permission check. `None` when they do not, or `mode` lets
/// opencode decide.
fn shell_refusal(name: &str, args: &str, mode: Mode) -> Option<String> {
    (matches!(mode, Mode::Ask | Mode::AcceptEdits) && args.contains(SHELL_MARK)).then(|| {
        format!(
            "The arguments of /{} hold `!`…``, which opencode runs in its shell before it asks for any \
             permission; in {} Octet did not send it. In auto, opencode's own permission settings decide",
            short(name),
            mode.label()
        )
    })
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
/// turn is shown to the user, who decides; anything else (another session's,
/// one while the turn is being cancelled, or no turn's) gets the stray
/// reply, with a notice, except a client capability's, which is refused
/// quietly.
async fn server_request(core: &mut Core, v: Value, known: Option<&str>) -> Result<(), DriverError> {
    let method = v["method"].as_str().unwrap_or_default();
    let this_session = !core.session.is_empty()
        && v.pointer("/params/sessionId").and_then(Value::as_str) == Some(core.session.as_str());
    if method == "session/request_permission" && this_session && core.phase == Phase::InTurn {
        let (detail, summary) = approval(&v["params"]["toolCall"], known);
        return core.queue_approval(v, detail, summary).await;
    }
    if let Some(reply) = opencode_stray_reply(&v) {
        if client_capability(method) {
            // opencode 1.18.25 asks the client to write every edit it was
            // allowed, although Octet offers no file system; it has already
            // written the file itself, so the refusal is nothing to report.
            return core.send(reply).await;
        }
        let notice = if method == "session/request_permission" {
            let when = if this_session && core.phase == Phase::Interrupting {
                "while its turn is being cancelled"
            } else {
                "outside this session's running turn"
            };
            format!("opencode asked for a permission {when}; Octet rejected it")
        } else {
            format!("Request declined: {}", short(method))
        };
        core.send(reply).await?;
        core.emit(Event::Notice(notice))?;
    }
    Ok(())
}

/// Whether `method` is one of the client capabilities (`fs/*`, `terminal/*`)
/// Octet does not offer in `initialize`: such a request is refused without
/// a notice.
fn client_capability(method: &str) -> bool {
    method.starts_with("fs/") || method.starts_with("terminal/")
}

/// What a permission request's `toolCall` would do: the dialog's detail
/// (`kind · title`, then its `rawInput` in full) and its summary. `known` is
/// the target its tool call named earlier: opencode 1.18.25 asks to read a
/// file without naming it.
fn approval(tool_call: &Value, known: Option<&str>) -> (String, ApprovalSummary) {
    let kind = tool_call["kind"].as_str().unwrap_or("other");
    let title = tool_call["title"].as_str().unwrap_or("tool");
    let mut detail = format!("{kind} · {title}");
    let input = &tool_call["rawInput"];
    if !input.is_null() {
        detail.push('\n');
        detail.push_str(&serde_json::to_string_pretty(input).unwrap_or_default());
    }
    let locations: Vec<String> = tool_call["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|location| location["path"].as_str())
        .map(str::to_owned)
        .collect();
    let command = input["command"].as_str().map(str::to_owned);
    let summary = match kind {
        "execute" => ApprovalSummary {
            command: Some(title.to_owned()),
            ..ApprovalSummary::new("Run a shell command")
        },
        "edit" => ApprovalSummary {
            files: locations,
            ..ApprovalSummary::new("Edit files")
        },
        "read" => {
            let files = if locations.is_empty() {
                known.map(str::to_owned).into_iter().collect()
            } else {
                locations
            };
            let action = match files.as_slice() {
                [] => "Read a file opencode did not name".to_owned(),
                [file] => format!("Read {}", file_name(file)),
                many => format!("Read {} files", many.len()),
            };
            ApprovalSummary {
                files,
                ..ApprovalSummary::new(action)
            }
        }
        "fetch" => {
            let address = input["url"].as_str().unwrap_or(title);
            ApprovalSummary::new(format!("Fetch {}", one_line(address)))
        }
        // opencode's `external_directory` permission: a path outside the
        // workspace, for a read or edit, or for a command.
        "other"
            if ["filepath", "parentDir", "directories"]
                .iter()
                .any(|key| !input[key].is_null()) =>
        {
            let path = input["filepath"]
                .as_str()
                .or_else(|| input.pointer("/directories/0").and_then(Value::as_str))
                .or_else(|| input["parentDir"].as_str())
                .or(known)
                .unwrap_or(title);
            ApprovalSummary {
                command,
                files: locations,
                ..ApprovalSummary::new(format!("Reach {}, outside the workspace", one_line(path)))
            }
        }
        _ => ApprovalSummary::new("Use a tool"),
    };
    (detail, summary)
}

/// The last part of `path` in one short line, or the path itself when it
/// has none.
fn file_name(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    one_line(name)
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

/// What Octet says when `OPENCODE_CONFIG_CONTENT`'s `agent` is not a JSON
/// object and has to be replaced.
pub(super) const AGENT_OBJECT_NOTICE: &str =
    "OPENCODE_CONFIG_CONTENT's agent is not a JSON object; Octet replaced it for this session";
/// What Octet says when `OPENCODE_CONFIG_CONTENT` is not text and is left
/// out.
pub(super) const UTF8_NOTICE: &str =
    "OPENCODE_CONFIG_CONTENT is not valid UTF-8; Octet ignored it for this session";

/// The user's `OPENCODE_CONFIG_CONTENT`, read from the environment (see
/// `user_content_from`).
fn user_content() -> (Option<String>, Option<&'static str>) {
    user_content_from(std::env::var_os("OPENCODE_CONFIG_CONTENT"))
}

/// The user's `OPENCODE_CONFIG_CONTENT` as text, given its `value`, and the
/// notice when it is not valid UTF-8 and so is treated as absent.
fn user_content_from(value: Option<OsString>) -> (Option<String>, Option<&'static str>) {
    match value.map(OsString::into_string) {
        None => (None, None),
        Some(Ok(text)) => (Some(text), None),
        Some(Err(_)) => (None, Some(UTF8_NOTICE)),
    }
}

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
    let (user, _) = user_content();
    launch_env_from(config, user.as_deref()).0
}

/// The variables that start opencode, given the user's
/// `OPENCODE_CONFIG_CONTENT` as `user`, and a notice when that content, or
/// its `agent`, was not a JSON object and was replaced. Octet's three agents are set over the
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
/// set in its `agent` object, and the notice when `user`, or its `agent`,
/// was not an object.
fn merged_content(user: Option<&str>) -> (String, Option<String>) {
    let (mut content, mut notice) = match user.map(Ordered::parse) {
        None => (Ordered(Vec::new()), None),
        Some(Some(object)) => (object, None),
        Some(None) => (Ordered(Vec::new()), Some(AGENT_NOTICE.to_owned())),
    };
    let mut agents = match content
        .get("agent")
        .map(|agents| Ordered::parse(agents.get()))
    {
        None => Ordered(Vec::new()),
        Some(Some(agents)) => agents,
        Some(None) => {
            notice = Some(AGENT_OBJECT_NOTICE.to_owned());
            Ordered(Vec::new())
        }
    };
    for (name, definition) in OCTET_AGENTS {
        agents.set((*name).to_owned(), raw((*definition).to_owned()));
    }
    content.set("agent".to_owned(), agents.into_raw());
    (content.to_json(), notice)
}

/// The agents Octet defines for its `ask`, `accept-edits` and `full-access`
/// modes, as JSON text. Reads are allowed everywhere, since they change
/// nothing, except `.env` files, which ask as opencode's own defaults do.
/// `question`, `plan_enter` and `plan_exit` stay denied, as opencode denies
/// them by default; the catch-all would make them ask. Each permission map
/// starts with `"*"`: rules are last-match-wins, so the entries after it
/// refine it. The asking agents deny `task`: opencode
/// builds a subagent's permissions from the parent session's deny rules, not
/// from the agent's `"*":"ask"`, so a subagent would act without asking.
const OCTET_AGENTS: &[(&str, &str)] = &[
    (
        "octet-ask",
        r#"{"mode":"primary","description":"Octet: ask before edits and commands",
        "permission":{"*":"ask",
        "read":{"*":"allow","*.env":"ask","*.env.*":"ask","*.env.example":"allow"},
        "glob":"allow","grep":"allow","list":"allow","lsp":"allow","todoread":"allow",
        "todowrite":"allow","skill":"allow","task":"deny",
        "question":"deny","plan_enter":"deny","plan_exit":"deny"}}"#,
    ),
    (
        "octet-accept-edits",
        r#"{"mode":"primary","description":"Octet: edits proceed, other actions ask",
        "permission":{"*":"ask","edit":"allow",
        "read":{"*":"allow","*.env":"ask","*.env.*":"ask","*.env.example":"allow"},
        "glob":"allow","grep":"allow","list":"allow","lsp":"allow","todoread":"allow",
        "todowrite":"allow","skill":"allow","task":"deny",
        "question":"deny","plan_enter":"deny","plan_exit":"deny"}}"#,
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
    fn a_skill_prompt_is_its_slash_command_alone() {
        let text = |text: &str| json!({"type": "text", "text": text});
        assert_eq!(skill_prompt("review", "src"), [text("/review src")]);
        assert_eq!(skill_prompt("review", ""), [text("/review")]);
        assert_eq!(skill_prompt("review", "  a b "), [text("/review   a b")]);
    }

    #[test]
    fn a_slash_command_is_read_as_opencode_reads_it() {
        let text = |text: &str| json!({"type": "text", "text": text});
        let image = json!({"type": "image", "mimeType": "image/png", "data": "AA"});
        let read = |blocks: &[Value]| slash_command(blocks);
        let command = |name: &str, args: &str| Some((name.to_owned(), args.to_owned()));
        assert_eq!(read(&[text("/review  a\n b ")]), command("review", "a b"));
        assert_eq!(read(&[text("  /init")]), command("init", ""));
        // Every text block joins the arguments, as they are.
        assert_eq!(
            read(&[text("/review go"), image, text("\n\nmore")]),
            command("review", "go more")
        );
        assert_eq!(read(&[text("/rev"), text("iew x")]), command("review", "x"));
        for plain in ["hello /review", "/ review", "/", ""] {
            assert_eq!(read(&[text(plain)]), None, "{plain:?}");
        }
    }

    #[test]
    fn shell_in_a_commands_arguments_is_refused_only_while_the_mode_asks() {
        for mode in [Mode::Ask, Mode::AcceptEdits] {
            let refusal = shell_refusal("x", "a !`id` b", mode).unwrap();
            assert!(refusal.contains("/x"), "{refusal}");
            assert!(refusal.contains(mode.label()), "{refusal}");
            assert_eq!(shell_refusal("x", "a `id` ! b", mode), None);
        }
        for mode in [Mode::Auto, Mode::FullAccess] {
            assert_eq!(shell_refusal("x", "!`id`", mode), None);
        }
    }

    /// `configOptions` for `model` with an effort option at `current`
    /// offering `levels`, or none when `levels` is empty.
    fn effort_options(model: &str, current: &str, levels: &[&str]) -> Value {
        let mut options = json!([{"id": "model", "currentValue": model, "options": []}]);
        if !levels.is_empty() {
            let choices: Vec<Value> = levels.iter().map(|l| json!({"value": l})).collect();
            options
                .as_array_mut()
                .unwrap()
                .push(json!({"id": "effort", "currentValue": current, "options": choices}));
        }
        options
    }

    #[test]
    fn effort_levels_follow_the_reported_options() {
        let mut protocol = OpencodeProtocol::default();
        // Unknown until opencode reports options: the level is sent.
        assert_eq!(protocol.effort_refusal("high"), None);
        protocol.note_options(&effort_options("a/free", "low", &["low", "high", "max"]));
        assert_eq!(protocol.effort_refusal("max"), None);
        assert_eq!(
            protocol.effort_refusal("bogus").as_deref(),
            Some("opencode's model a/free takes effort low, high or max")
        );
        // The model's first level is its default; a later level keeps it.
        protocol.note_options(&effort_options("a/free", "max", &["low", "high", "max"]));
        let option = protocol.effort.listed().unwrap();
        assert_eq!(option.current.as_deref(), Some("max"));
        assert_eq!(option.default.as_deref(), Some("low"));
        // Another model starts over.
        protocol.note_options(&effort_options("b/paid", "none", &["none", "xhigh"]));
        let option = protocol.effort.listed().unwrap();
        assert_eq!(option.default.as_deref(), Some("none"));
        assert_eq!(
            protocol.effort_refusal("low").as_deref(),
            Some("opencode's model b/paid takes effort none or xhigh")
        );
        protocol.note_options(&effort_options("c/one", "only", &["only"]));
        assert_eq!(
            protocol.effort_refusal("low").as_deref(),
            Some("opencode's model c/one takes effort only")
        );
        // Options without an effort one: the model has no levels.
        protocol.note_options(&effort_options("d/none", "", &[]));
        assert_eq!(
            protocol.effort_refusal("high").as_deref(),
            Some("opencode's model d/none has no effort levels")
        );
        // A reply without options changes nothing.
        protocol.note_options(&Value::Null);
        assert!(matches!(protocol.effort, Efforts::Absent));
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
        let (detail, summary) = approval(
            &json!({
                "kind": "execute", "title": "rm -rf build",
                "rawInput": {"command": "rm -rf build"}, "locations": [],
            }),
            None,
        );
        assert_eq!(
            detail,
            "execute · rm -rf build\n{\n  \"command\": \"rm -rf build\"\n}"
        );
        assert_eq!(summary.action, "Run a shell command");
        assert_eq!(summary.command.as_deref(), Some("rm -rf build"));
        assert!(summary.files.is_empty());

        let (detail, summary) = approval(
            &json!({
                "kind": "edit", "title": "src/a.rs",
                "locations": [{"path": "/w/src/a.rs"}, {"path": "/w/src/b.rs"}, {"line": 3}],
            }),
            None,
        );
        assert_eq!(detail, "edit · src/a.rs");
        assert_eq!(summary.action, "Edit files");
        assert_eq!(summary.files, ["/w/src/a.rs", "/w/src/b.rs"]);
        assert_eq!(summary.command, None);

        let (_, summary) = approval(&json!({}), None);
        assert_eq!(summary.action, "Use a tool");
        assert_eq!(summary.command, None);
        assert_eq!(approval(&json!({}), None).0, "other · tool");
    }

    #[test]
    fn a_read_approval_names_its_file_even_when_the_request_does_not() {
        // opencode 1.18.25 sends a read's request with no path at all; the
        // path is the one its tool call named.
        let request = json!({"kind": "read", "title": "read", "locations": [], "rawInput": {}});
        let (detail, summary) = approval(&request, Some("/w/.env"));
        assert_eq!(detail, "read · read\n{}");
        assert_eq!(summary.action, "Read .env");
        assert_eq!(summary.files, ["/w/.env"]);
        let (_, summary) = approval(&request, None);
        assert_eq!(summary.action, "Read a file opencode did not name");
        assert!(summary.files.is_empty());
        // A path the request names wins.
        let named = json!({"kind": "read", "title": "a.rs", "locations": [{"path": "/w/a.rs"}]});
        let (_, summary) = approval(&named, Some("/w/.env"));
        assert_eq!(summary.action, "Read a.rs");
        assert_eq!(summary.files, ["/w/a.rs"]);
    }

    #[test]
    fn a_fetch_approval_names_its_address() {
        let request = json!({"kind": "fetch", "title": "https://example.com", "locations": [],
            "rawInput": {"url": "https://example.com/a", "format": "text"}});
        let (_, summary) = approval(&request, None);
        assert_eq!(summary.action, "Fetch https://example.com/a");
        assert!(summary.files.is_empty());
        let (_, summary) = approval(
            &json!({"kind": "fetch", "title": "https://b.example"}),
            None,
        );
        assert_eq!(summary.action, "Fetch https://b.example");
    }

    #[test]
    fn an_outside_the_workspace_approval_names_the_path_and_any_command() {
        // A read outside the workspace, as opencode 1.18.25 asks for it.
        let read = json!({"kind": "other", "title": "/etc",
            "locations": [{"path": "/etc/hosts"}, {"path": "/etc"}],
            "rawInput": {"filepath": "/etc/hosts", "parentDir": "/etc"}});
        let (_, summary) = approval(&read, Some("/etc/hosts"));
        assert_eq!(summary.action, "Reach /etc/hosts, outside the workspace");
        assert_eq!(summary.files, ["/etc/hosts", "/etc"]);
        assert_eq!(summary.command, None);
        // A command that touches another folder.
        let command = json!({"kind": "other", "title": "touch /h/x.txt",
            "locations": [{"path": "/h"}],
            "rawInput": {"command": "touch /h/x.txt", "directories": ["/h"], "patterns": ["/h/*"]}});
        let (_, summary) = approval(&command, None);
        assert_eq!(summary.action, "Reach /h, outside the workspace");
        assert_eq!(summary.command.as_deref(), Some("touch /h/x.txt"));
        assert_eq!(summary.files, ["/h"]);
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
    fn an_agent_that_is_not_an_object_is_replaced_with_a_notice() {
        let user = r#"{"model":"x","agent":5}"#;
        let (env, notice) = launch_env_from(&config(), Some(user));
        assert_eq!(notice.as_deref(), Some(AGENT_OBJECT_NOTICE));
        assert_eq!(
            AGENT_OBJECT_NOTICE,
            "OPENCODE_CONFIG_CONTENT's agent is not a JSON object; Octet replaced it for this session"
        );
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(content["model"], "x");
        assert_eq!(content["agent"].as_object().unwrap().len(), 3);
        // An agent object of the user's gives no notice.
        let (_, notice) = launch_env_from(&config(), Some(r#"{"agent":{}}"#));
        assert_eq!(notice, None);
    }

    #[test]
    #[cfg(unix)]
    fn content_that_is_not_unicode_is_ignored_with_a_notice() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'{', 0xff, b'}']);
        assert_eq!(user_content_from(Some(bad)), (None, Some(UTF8_NOTICE)));
        assert_eq!(
            UTF8_NOTICE,
            "OPENCODE_CONFIG_CONTENT is not valid UTF-8; Octet ignored it for this session"
        );
        assert_eq!(
            user_content_from(Some("{}".into())),
            (Some("{}".to_owned()), None)
        );
        assert_eq!(user_content_from(None), (None, None));
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

    /// The permission map of Octet's agent `name`, as launched, with its
    /// keys in order.
    fn permission_map(name: &str) -> Vec<(String, Value)> {
        let (env, _) = launch_env_from(&config(), None);
        let text = var(&env, "OPENCODE_CONFIG_CONTENT").unwrap();
        let agents = Ordered::parse(&text).unwrap();
        let agent = Ordered::parse(agents.get("agent").unwrap().get()).unwrap();
        let definition = Ordered::parse(agent.get(name).unwrap().get()).unwrap();
        Ordered::parse(definition.get("permission").unwrap().get())
            .unwrap()
            .0
            .into_iter()
            .map(|(key, value)| (key, serde_json::from_str(value.get()).unwrap()))
            .collect()
    }

    #[test]
    fn the_asking_agents_turn_subagents_off() {
        // opencode's task tool gives a subagent the parent session's deny
        // rules, not the agent's "*":"ask", so it would run unasked.
        for name in ["octet-ask", "octet-accept-edits"] {
            let map = permission_map(name);
            assert_eq!(map[0], ("*".to_owned(), json!("ask")), "{name}");
            assert!(
                map.contains(&("task".to_owned(), json!("deny"))),
                "{name}: {map:?}"
            );
        }
        assert_eq!(
            PROVIDER.modes[0],
            "opencode asks before edits, commands and other actions; subagents are off (agent octet-ask)"
        );
        assert_eq!(
            PROVIDER.modes[1],
            "Edits proceed; commands and other actions ask; subagents are off (agent octet-accept-edits)"
        );
    }

    #[test]
    fn the_asking_agents_keep_opencodes_env_file_protection() {
        // A plain "read":"allow" would let the model read .env files that
        // opencode asks for by default.
        for name in ["octet-ask", "octet-accept-edits"] {
            let map = permission_map(name);
            let read = map.iter().find(|(key, _)| key == "read").unwrap();
            assert_eq!(
                serde_json::to_string(&read.1).unwrap(),
                r#"{"*":"allow","*.env":"ask","*.env.*":"ask","*.env.example":"allow"}"#,
                "{name}"
            );
        }
        // Last match wins: the catch-all comes first, the exception last.
        let (env, _) = launch_env_from(&config(), None);
        let text = var(&env, "OPENCODE_CONFIG_CONTENT").unwrap();
        let read = r#""read":{"*":"allow","*.env":"ask","*.env.*":"ask","*.env.example":"allow"}"#;
        assert_eq!(text.matches(read).count(), 2, "{text}");
    }

    #[test]
    fn the_asking_agents_deny_what_opencode_denies_by_default() {
        // "*":"ask" would turn these into questions to the user.
        for name in ["octet-ask", "octet-accept-edits"] {
            let map = permission_map(name);
            for tool in ["question", "plan_enter", "plan_exit"] {
                assert!(
                    map.contains(&(tool.to_owned(), json!("deny"))),
                    "{name} {tool}: {map:?}"
                );
            }
        }
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
