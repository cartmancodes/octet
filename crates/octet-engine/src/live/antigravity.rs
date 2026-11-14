//! The Antigravity CLI (`agy acp`) over the Agent Client Protocol: its row,
//! the launch (Octet's own settings folder, the workspace's `.agents` check,
//! a probe's scratch home), the handshake, the model catalog and thinking
//! levels, turns and their approvals, live mode switches, skills and
//! compaction, resume and the probe; the tripwire that stops the session
//! when Antigravity acts unasked, and Antigravity changing its own mode or
//! model.
mod follow;
mod guarded;
mod launch;
mod settings;
mod tools;
mod trust;

use super::{
    BoxFuture, Channels, Config, DriverError, Event, ImageAttachment, Limits, Mode, NOT_SENT,
    Outcome, Provider, acp,
    acp::Levels,
    activity::one_line,
    deadline_after,
    driver::{effort_notice, seconds},
    protocol::{Core, Phase, Protocol},
    short, valid_identifier,
};
pub use launch::antigravity_prepare;
use launch::{mode_id, mode_of};
use serde_json::{Value, json};
use std::{ffi::OsString, path::Path, time::Duration};
use tokio::time::Instant;
pub use trust::{
    TrustedAtStart, WorkspaceConfig, antigravity_trust, antigravity_trusted,
    antigravity_workspace_config,
};

/// The Antigravity versions Octet has been checked against.
pub const ANTIGRAVITY_VERIFIED_VERSIONS: &[&str] = &["1.5.5"];

/// Antigravity's row in the provider table.
pub(super) const PROVIDER: Provider = Provider {
    name: "antigravity",
    title: "Antigravity",
    default_binary: "agy",
    offline: false,
    modes: [
        concat!(
            "Antigravity asks before commands and edits; reads inside the workspace never ask; ",
            "a subagent's requests never reach Octet; Octet has not checked MCP tools (mode default)"
        ),
        concat!(
            "Edits proceed; commands and other actions ask; a subagent's requests never reach Octet; ",
            "Octet has not checked MCP tools (mode accept-edits)"
        ),
        concat!(
            "Antigravity's own checker and sandbox decide; it asks rarely ",
            "(network and delete commands ran unasked when tested) (mode auto)"
        ),
        "No permission checks at all (toolPermission always-proceed)",
    ],
    steer: false,
    mode_applies_next_turn: false,
    inline_images: true,
    effort_live: true,
    takes_effort: true,
    ids_carry_name: false,
    // The thinking levels depend on the model.
    efforts: &[],
    // Antigravity 1.5.5 exits within 375 ms once its input closes, with a
    // session open.
    shutdown_grace: Duration::from_secs(1),
    lists_models: true,
    commands_expand_shell: false,
    forks: false,
    carries_others_models: true,
    launch_args: launch::launch_args,
    launch_env: super::no_env,
    prepare: launch::prepare,
    start,
};

/// What Octet says when Antigravity is not signed in.
const SIGN_IN: &str = "Sign in to Antigravity first: run `agy`";
/// What Octet says when a model has no thinking level.
const NO_LEVELS: &str = "This Antigravity model takes no thinking level";
/// How long a probe waits, after the session opens, for Antigravity to list
/// its commands before it settles for the models alone.
const PROBE_COMMANDS_WAIT: Duration = Duration::from_secs(2);
/// The permission options Octet answers with when a request lists none.
const PERMISSION_IDS: (&str, &str) = ("allow", "deny");
/// What Octet says when the session lists no `compact` command.
const NO_COMPACT: &str = "Antigravity does not offer compaction in this session";
/// Why a turn is not sent once Antigravity acted unasked in the session.
const ACTED: &str = "Antigravity acted without asking earlier in this session, so Octet sends it nothing more. \
                     Start a new session";

/// Runs one Antigravity session through the shared driver.
fn start(config: Config, limits: Limits, channels: Channels) -> BoxFuture<Result<(), String>> {
    Box::pin(
        async move { super::driver::run::<AntigravityProtocol>(config, limits, channels).await },
    )
}

/// What a reply from Antigravity answers: one of Octet's own requests.
enum Outstanding {
    /// `initialize`, the handshake's first request.
    Initialize,
    /// `session/new` or `session/resume`, with the session a resume names,
    /// which its reply need not repeat.
    Open(Option<String>),
    /// The model the user asked for, set after the session opens.
    SetModel,
    /// A thinking level.
    SetEffort(EffortRequest),
    /// The mode, set as the handshake's last step.
    SetMode,
    /// The mode the user switched to, in the open session.
    Switch(Mode),
    /// Octet's mode, set back after Antigravity moved itself.
    SetBack(Mode),
    /// `session/prompt`: its reply ends the turn.
    Prompt,
}

/// A thinking level set in the session.
struct EffortRequest {
    /// The level Octet records once Antigravity confirms it; `None` is the
    /// level the model opened at.
    level: Option<String>,
    /// The value sent: the level, or the level the model opened at.
    value: String,
    /// Set during the handshake, which goes on to the mode afterwards.
    handshake: bool,
}

/// A thinking level asked for after the handshake's effort step and before
/// the session is ready; it is set through the live path at the end.
struct Wanted {
    /// The level; `None` is the level the model opened at.
    level: Option<String>,
}

/// Antigravity's per-connection state: Octet's requests awaiting replies,
/// the session's options as Antigravity last reported them, the commands it
/// lists, and the turn's tool calls.
#[derive(Default)]
pub(super) struct AntigravityProtocol {
    /// Octet's requests that have no reply yet, with what each reply
    /// answers.
    requests: acp::Requests<Outstanding>,
    /// The live mode switch awaiting its reply, and the timed-out ones
    /// Antigravity may still confirm.
    switches: acp::Switches,
    /// A `session/cancel` was sent this turn: an `end_turn` then means the
    /// turn was interrupted.
    cancel_sent: bool,
    /// Why Octet cancelled the running turn, or why it fails although the
    /// user cancelled it: it ends `Failed` with this, whatever its stop
    /// reason (see `cancel_turn`).
    stop_reason: Option<String>,
    /// The kept reason is the tripwire's.
    stop_trip: bool,
    /// When the kept reason is shown with the session's stop, if the
    /// cancelled prompt is not answered: the interrupt deadline. The driver's
    /// own interrupt watchdog, which would stop the session without it, is
    /// put a whole interrupt limit later (see `cancel_turn`).
    stop_deadline: Option<Instant>,
    /// Antigravity acted unasked in this session: nothing more is sent.
    tripped: bool,
    /// Octet's mode, to be set back after Antigravity moved itself.
    follow: follow::Follow,
    /// The wire ID of the newest live switch or set-back Antigravity
    /// confirmed: a late reply to an older switch is stale.
    newest_confirmed: u64,
    /// The session's model (`currentValue` of its `model` option).
    model: Option<String>,
    /// The session's mode ID (its `mode` option, or `current_mode_update`).
    mode: Option<String>,
    /// The current model's thinking levels.
    levels: Levels,
    /// The names of the commands Antigravity last listed for this session,
    /// bounded; none before its first list.
    commands: Vec<String>,
    /// This turn's tool calls.
    tools: tools::Tools,
    /// A probe's session is open and its commands are awaited until this
    /// moment; `None` once the probe has been answered.
    probe_wait: Option<Instant>,
    /// The fingerprint of the workspace's `.agents` configuration accepted
    /// before the launch (`None`: there was none).
    workspace: Option<String>,
    /// The handshake's effort step has run: a level asked for before the
    /// session is ready no longer reaches it.
    effort_stepped: bool,
    /// The newest level asked for after the effort step and before the
    /// session is ready.
    effort_wanted: Option<Wanted>,
    /// The level the handshake's effort step set, reported once ready.
    effort_set: Option<String>,
}

impl Protocol for AntigravityProtocol {
    fn launch_args(config: &Config) -> Vec<OsString> {
        launch::launch_args(config)
    }

    fn prepared(&mut self, workspace: Option<String>) {
        self.workspace = workspace;
    }

    fn answer(wire: &Value, allow: bool) -> Value {
        acp::permission_reply(wire, allow, PERMISSION_IDS)
    }

    fn allow_withheld(wire: &Value) -> bool {
        acp::allow_withheld(wire)
    }

    fn mode_change_pending(&self) -> bool {
        self.switches.pending()
    }

    fn mode_unsettled(&self) -> Option<Mode> {
        self.switches.unsettled()
    }

    fn take_switch_timed_out(&mut self) -> bool {
        self.switches.take_timed_out()
    }

    /// Antigravity applies a switch to the running turn, but answers
    /// requests in no fixed order: a prompt sent while a switch is
    /// unanswered could run before the switch applies.
    fn holds_turn_for_switch(&self) -> bool {
        true
    }

    /// The pending switch's deadline is also a set-back's.
    fn deadline(&self) -> Option<Instant> {
        [
            self.switches.deadline(),
            self.probe_wait,
            self.stop_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// A set-back Antigravity has not confirmed in time stops the session,
    /// as does a cancelled turn with a kept reason that Antigravity has not
    /// ended in time, saying the reason. A switch Antigravity has not
    /// confirmed in time is reported, and kept in case Antigravity confirms
    /// it late. A probe whose commands never came settles for the models.
    /// Nothing here waits, so the work is done at once and handed back as a
    /// ready future.
    fn on_deadline(
        &mut self,
        core: &mut Core,
    ) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(
            self.set_back_expired(core)
                .and_then(|()| self.stop_expired(core))
                .and_then(|()| self.switches.report_expired(core, PROVIDER.title))
                .and_then(|()| self.probe_timed_out(core)),
        )
    }

    /// Only frames for this session count: another session's chatter is
    /// not progress.
    fn is_progress(&self, core: &Core, frame: &Value) -> bool {
        acp::for_session(frame, &core.session)
    }

    fn turn_started(&mut self) {
        self.cancel_sent = false;
        self.stop_reason = None;
        self.stop_trip = false;
        self.stop_deadline = None;
        self.tools.clear();
    }

    /// Sends `initialize`, offering no client capabilities.
    async fn initialize(&mut self, core: &mut Core) -> Result<(), DriverError> {
        let params = json!({"protocolVersion": 1, "clientCapabilities": {}});
        self.requests
            .send(core, Outstanding::Initialize, "initialize", params)
            .await
    }

    /// Nothing is sent once Antigravity acted unasked. The workspace is
    /// checked next; then the text, with the images inline after it.
    async fn send_prompt(
        &mut self,
        core: &mut Core,
        text: &str,
        images: &[ImageAttachment],
    ) -> Result<(), DriverError> {
        if self.tripped {
            return core.finish_turn(Outcome::Failed, Some(format!("{NOT_SENT}{ACTED}")));
        }
        if let Err(refusal) = self.workspace_unchanged(core).await {
            return core.finish_turn(Outcome::Failed, Some(format!("{NOT_SENT}{refusal}")));
        }
        let Some(blocks) = acp::image_blocks(core, PROVIDER.title, images).await? else {
            return Ok(());
        };
        let mut prompt = vec![json!({"type": "text", "text": text})];
        prompt.extend(blocks);
        let params = json!({"sessionId": core.session, "prompt": prompt});
        self.requests
            .send(core, Outstanding::Prompt, "session/prompt", params)
            .await
    }

    /// A skill is one of Antigravity's slash commands, sent as a prompt (see
    /// `skill_text`), so the workspace is checked first.
    async fn send_skill(
        &mut self,
        core: &mut Core,
        name: &str,
        _path: Option<&Path>,
        args: &str,
        context: Option<&str>,
    ) -> Result<(), DriverError> {
        self.send_prompt(core, &skill_text(name, args, context), &[])
            .await
    }

    /// Compaction is the prompt `/compact`, sent as a prompt (so the
    /// workspace is checked first) when Antigravity's last command list for
    /// this session holds `compact`. Without it, or before any list came,
    /// the turn fails and nothing is sent; once Antigravity acted unasked,
    /// it fails as every prompt does.
    async fn compact(&mut self, core: &mut Core) -> Result<(), DriverError> {
        if self.tripped {
            return self.send_prompt(core, "/compact", &[]).await;
        }
        if !self.commands.iter().any(|name| name == "compact") {
            return core.finish_turn(Outcome::Failed, Some(NO_COMPACT.to_owned()));
        }
        self.send_prompt(core, "/compact", &[]).await
    }

    /// Sends the `session/cancel` notification. An `end_turn` then reads as
    /// interrupted.
    async fn interrupt(&mut self, core: &mut Core) -> Result<(), DriverError> {
        self.cancel_sent = true;
        core.send(acp::cancel(&core.session)).await
    }

    /// Sets the `mode` option to `target`'s mode ID, live. The mode changes
    /// only when Antigravity's reply names it (see `switched`). While eight
    /// earlier switches wait unconfirmed, another is refused (see
    /// `acp::Switches::refuse_when_full`). The driver sends a switch only
    /// once the session is ready.
    async fn set_mode(&mut self, core: &mut Core, target: Mode) -> Result<(), DriverError> {
        if self.switches.refuse_when_full(core, PROVIDER.title)? {
            return Ok(());
        }
        self.set_option(core, Outstanding::Switch(target), "mode", mode_id(target))
            .await?;
        self.switches.sent(
            self.requests.last_id(),
            target,
            deadline_after(core.limits.mode_confirm),
        );
        Ok(())
    }

    /// Sets the `thought_level` option, live. A level the model does not
    /// list, or a model without levels, is refused here, unsent, and the
    /// interface goes back to the level in force. `/effort default` sets the
    /// level the model opened at, when that is not the one in force.
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
        let request = if let Some(level) = effort {
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
        } else {
            let Some(opening) = self.opening_to_restore() else {
                core.config.effort = None;
                core.emit(Event::Effort(None))?;
                core.emit(Event::Notice(effort_notice(None, core.phase.is_running())))?;
                return Ok(true);
            };
            EffortRequest {
                value: opening,
                level: None,
                handshake: false,
            }
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
            return self.vendor_request(core, v).await;
        }
        if method != "session/update" || !acp::names_session(&v, &core.session) {
            return Ok(());
        }
        let update = &v["params"]["update"];
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "available_commands_update" => self.commands_listed(core, &update["availableCommands"]),
            "config_option_update" => self.options_updated(core, &update["configOptions"]).await,
            "current_mode_update" => {
                let Some(id) = update["currentModeId"]
                    .as_str()
                    .filter(|id| valid_identifier(id))
                else {
                    return Ok(());
                };
                self.mode = Some(id.to_owned());
                self.followed(core, id).await
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
            "tool_call" | "tool_call_update" => self.tool(core, update).await,
            // Thoughts, the prompt echoed back and the session's title are
            // not shown.
            _ => Ok(()),
        }
    }
}

impl AntigravityProtocol {
    /// Sets the session's `option` to `value`.
    async fn set_option(
        &mut self,
        core: &Core,
        kind: Outstanding,
        option: &str,
        value: &str,
    ) -> Result<(), DriverError> {
        let params = json!({"sessionId": core.session, "configId": option, "value": value});
        self.requests
            .send(core, kind, "session/set_config_option", params)
            .await
    }

    /// Whether the workspace's `.agents` configuration is still the one
    /// accepted before the launch. The check reads files, so it runs on a
    /// blocking thread, bounded by `Limits::image_read`; a check that fails
    /// to run counts as a change.
    ///
    /// # Errors
    ///
    /// Why the turn must not be sent, in words to show.
    async fn workspace_unchanged(&self, core: &Core) -> Result<(), String> {
        let cwd = core.config.cwd.clone();
        let started_with = self.workspace.clone();
        let check =
            tokio::task::spawn_blocking(move || trust::unchanged(&cwd, started_with.as_deref()));
        match tokio::time::timeout(core.limits.image_read, check).await {
            Ok(Ok(result)) => result,
            Ok(Err(failure)) => Err(format!(
                "Octet could not check this workspace's .agents configuration: {failure}"
            )),
            Err(_) => Err(format!(
                "Octet could not check this workspace's .agents configuration in {}",
                seconds(core.limits.image_read)
            )),
        }
    }

    /// Replies to Octet's own requests, matched by the request they answer.
    async fn response(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        let Some(kind) = self.requests.answered(v, !core.phase.is_ready())? else {
            return Ok(());
        };
        match kind {
            Outstanding::Initialize => self.handshaken(core, v).await,
            Outstanding::Open(resumed) => self.opened(core, v, resumed).await,
            Outstanding::SetModel => self.model_set(core, v).await,
            Outstanding::SetEffort(request) => self.effort_reply(core, v, request).await,
            Outstanding::SetMode => self.mode_set(core, v).await,
            Outstanding::Switch(target) => self.switched(core, v, target).await,
            Outstanding::SetBack(target) => self.set_back_reply(v, target),
            Outstanding::Prompt => self.prompt_ended(core, v).await,
        }
    }

    /// The handshake's reply: a version Octet was not checked against is
    /// noticed (not for a probe), then the session is opened, or resumed. A
    /// probe opens a new one, since it only reads the catalog.
    async fn handshaken(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            return Err(format!(
                "Antigravity initialization failed: {}",
                acp::error_text(error)
            )
            .into());
        }
        core.phase = Phase::Handshaken;
        let version = v
            .pointer("/result/agentInfo/version")
            .and_then(Value::as_str);
        if let Some(notice) = version_notice(version)
            && !core.config.catalog_only
        {
            core.emit(Event::Notice(notice))?;
        }
        let config = &core.config;
        let mut params = json!({"cwd": config.cwd, "mcpServers": []});
        let (method, resumed) = match &config.resume {
            Some(id) if !config.catalog_only => {
                params["sessionId"] = json!(id);
                ("session/resume", Some(id.clone()))
            }
            _ => ("session/new", None),
        };
        self.requests
            .send(core, Outstanding::Open(resumed), method, params)
            .await
    }

    /// The session is open: its catalog goes out, then the model the user
    /// asked for (when it is not already current), the thinking level and
    /// the mode. A resumed session goes through the same steps: they put
    /// its mode, model and level back.
    async fn opened(
        &mut self,
        core: &mut Core,
        v: &Value,
        resumed: Option<String>,
    ) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            return Err(open_error(error).into());
        }
        let result = &v["result"];
        core.session = result["sessionId"]
            .as_str()
            .filter(|id| valid_identifier(id))
            .map(str::to_owned)
            .or(resumed)
            .ok_or("Antigravity could not open the session")?;
        self.note_options(&result["configOptions"]);
        core.emit(Event::Models(acp::catalog(&result["configOptions"])))?;
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

    /// The model's reply; the thinking level and the mode are set next. A
    /// refusal stops the session: going on would run the user's prompts on
    /// a model (and perhaps a bill) they never chose. A mode the reply
    /// names is followed as any other (see `followed`; during the handshake
    /// the mode step that comes next sets and checks it).
    async fn model_set(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        let requested = core.config.model.clone().unwrap_or_default();
        if let Some(error) = v.get("error") {
            return Err(format!(
                "Antigravity refused the model {}: {}",
                short(&requested),
                acp::error_text(error)
            )
            .into());
        }
        let options = &v["result"]["configOptions"];
        if options.is_array() {
            self.note_options(options);
            self.follow_options(core, options).await?;
        } else {
            // A reply without the options still confirms the request; the
            // new model's levels are not known yet.
            self.model = Some(requested).filter(|model| valid_identifier(model));
            self.levels = Levels::Unknown;
        }
        self.effort_step(core).await
    }

    /// The handshake's effort step: the level the user asked for, when the
    /// model lists it. A level it does not take is dropped with a notice;
    /// effort is not a safety property, so the session carries on.
    async fn effort_step(&mut self, core: &mut Core) -> Result<(), DriverError> {
        self.effort_stepped = true;
        if let Some(level) = core.config.effort.clone() {
            if let Some(refusal) = self.effort_refusal(&level) {
                core.emit(Event::Notice(refusal))?;
                core.config.effort = None;
                core.emit(Event::Effort(None))?;
            } else {
                let request = EffortRequest {
                    value: level.clone(),
                    level: Some(level),
                    handshake: true,
                };
                return self.send_effort(core, request).await;
            }
        }
        self.mode_step(core).await
    }

    /// Why the current model cannot take `level`, from its last reported
    /// options; `None` when it lists the level or its levels are unknown
    /// (then Antigravity decides). The levels are unknown when the reply
    /// that opened or resumed the session, or set the model, carried no
    /// `configOptions`. A `thought_level` option that lists no level is no
    /// option.
    fn effort_refusal(&self, level: &str) -> Option<String> {
        let (values, current) = match &self.levels {
            Levels::Unknown => return None,
            Levels::Absent => return Some(NO_LEVELS.to_owned()),
            Levels::Listed(option) => (&option.values, option.current.as_deref()),
        };
        if values.iter().any(|value| value == level) {
            return None;
        }
        let Some((last, rest)) = values.split_last() else {
            return Some(NO_LEVELS.to_owned());
        };
        let levels = if rest.is_empty() {
            last.clone()
        } else {
            format!("{} or {last}", rest.join(", "))
        };
        Some(format!(
            "Antigravity takes effort {levels} for this model; it keeps {}",
            current.unwrap_or("its current level")
        ))
    }

    /// The level the model opened at, when another is in force.
    fn opening_to_restore(&self) -> Option<String> {
        match &self.levels {
            Levels::Listed(option) if option.current != option.default => option.default.clone(),
            _ => None,
        }
    }

    /// Sends `request`'s `thought_level` value.
    async fn send_effort(
        &mut self,
        core: &Core,
        request: EffortRequest,
    ) -> Result<(), DriverError> {
        let value = request.value.clone();
        self.set_option(
            core,
            Outstanding::SetEffort(request),
            "thought_level",
            &value,
        )
        .await
    }

    /// A thinking level's reply. A confirmation records the level and, live,
    /// tells the interface; the handshake reports it once ready. A refusal
    /// is reported and the interface goes back to the level in force. A mode
    /// a confirmation names is followed (see `followed`). The handshake then
    /// sets the mode.
    async fn effort_reply(
        &mut self,
        core: &mut Core,
        v: &Value,
        request: EffortRequest,
    ) -> Result<(), DriverError> {
        if let Some(error) = v.get("error") {
            core.emit(Event::Notice(format!(
                "Antigravity did not set effort {}: {}",
                short(&request.value),
                acp::error_text(error)
            )))?;
            if request.handshake {
                core.config.effort = None;
            }
            core.emit(Event::Effort(core.config.effort.clone()))?;
        } else {
            let options = &v["result"]["configOptions"];
            if options.is_array() {
                self.note_options(options);
            } else if let Levels::Listed(option) = &mut self.levels {
                // A reply without the options still confirms the value sent.
                option.current = Some(request.value.clone());
            }
            core.config.effort.clone_from(&request.level);
            if request.handshake {
                self.effort_set.clone_from(&request.level);
            } else {
                core.emit(Event::Effort(request.level.clone()))?;
                core.emit(Event::Notice(effort_notice(
                    request.level.as_deref(),
                    core.phase.is_running(),
                )))?;
            }
            self.follow_options(core, options).await?;
        }
        if request.handshake {
            self.mode_step(core).await
        } else {
            Ok(())
        }
    }

    /// The handshake's last step: the session's mode, always.
    async fn mode_step(&mut self, core: &Core) -> Result<(), DriverError> {
        self.set_option(core, Outstanding::SetMode, "mode", mode_id(core.mode))
            .await
    }

    /// The mode's reply ends the handshake: ready, with the model, the level
    /// the effort step set and the mode, then a level asked for meanwhile.
    /// An error, a reply naming no mode or one naming another mode stops the
    /// session: whatever mode Antigravity kept may ask for less than the
    /// mode the user chose, so it must never act under it.
    async fn mode_set(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        let id = mode_id(core.mode);
        let reason = if let Some(error) = v.get("error") {
            Some(acp::error_text(error))
        } else {
            let options = &v["result"]["configOptions"];
            self.note_options(options);
            match reported_mode(options) {
                Value::Null => Some(NAMED_NO_MODE.to_owned()),
                reported if reported.as_str() == Some(id) => None,
                reported => Some(reported_reason(reported)),
            }
        };
        if let Some(reason) = reason {
            return Err(mode_refused(id, &reason));
        }
        core.phase = Phase::Idle;
        core.emit(Event::Ready {
            session: core.session.clone(),
        })?;
        if let Some(model) = &self.model {
            core.emit(Event::ModelSelected(model.clone()))?;
        }
        if let Some(level) = self.effort_set.take() {
            core.emit(Event::Effort(Some(level)))?;
        }
        core.adopt_mode(Some((mode_of(id, core.mode), id.to_owned())))?;
        if let Some(Wanted { level }) = self.effort_wanted.take()
            && level != core.config.effort
        {
            <Self as Protocol>::set_effort(self, core, level).await?;
        }
        Ok(())
    }

    /// A live switch's reply. Only a reply naming `target`'s mode confirms
    /// it. An error, or a reply naming the mode already in force, is
    /// reported and the session keeps its mode. A reply naming no mode, or
    /// any other mode, stops the session: Octet no longer knows the mode
    /// Antigravity runs, which may ask for less than the header shows. A
    /// reply after the switch timed out is handled the same way; the header
    /// was already put back then, and a confirmation says it came late. A
    /// confirmation raises the mode every running tool call is judged by;
    /// a set-back that waited for the switch is sent once it is answered.
    /// A late confirmation that comes after a newer switch was confirmed is
    /// not adopted: the mode it names is followed as Antigravity's own move
    /// (see `followed`), so the header's mode is set back.
    async fn switched(
        &mut self,
        core: &mut Core,
        v: &Value,
        target: Mode,
    ) -> Result<(), DriverError> {
        if let Some(stale) = self.switch_answered(core, v, target)? {
            self.followed(core, stale).await?;
        }
        self.send_due_set_back(core).await
    }

    /// A live switch's reply, as [`Self::switched`] takes it; the mode ID a
    /// stale confirmation named, for `followed`.
    fn switch_answered(
        &mut self,
        core: &mut Core,
        v: &Value,
        target: Mode,
    ) -> Result<Option<&'static str>, DriverError> {
        let late = self.switches.answered(v);
        let wanted = mode_id(target);
        let reason = if let Some(error) = v.get("error") {
            acp::error_text(error)
        } else {
            let options = &v["result"]["configOptions"];
            self.note_options(options);
            let reported = reported_mode(options);
            if reported.as_str() == Some(wanted) {
                let id = v["id"].as_u64().unwrap_or_default();
                if late && id < self.newest_confirmed {
                    return self.stale_confirmation(core, target);
                }
                self.newest_confirmed = self.newest_confirmed.max(id);
                core.mode = target;
                self.tools.raise(target);
                if late {
                    core.emit(Event::Notice(format!(
                        "Antigravity confirmed the switch to {} late",
                        target.label()
                    )))?;
                }
                core.emit(Event::ModeChanged(target))?;
                return Ok(None);
            }
            if reported.is_null() {
                return Err(mode_refused(wanted, NAMED_NO_MODE));
            }
            let reason = reported_reason(reported);
            if reported.as_str() != Some(mode_id(core.mode)) {
                return Err(mode_refused(wanted, &reason));
            }
            reason
        };
        core.emit(Event::Notice(format!(
            "Antigravity did not switch to {}: {reason}",
            target.label()
        )))?;
        if !late {
            core.emit(Event::ModeChanged(core.mode))?;
        }
        Ok(None)
    }

    /// Antigravity confirmed the switch to `target` late, after a newer
    /// switch was confirmed: it may now be in `target`'s mode, so the
    /// header's mode is set back (said here, so `followed` does not say it
    /// again); nothing is needed when that is the header's mode already.
    fn stale_confirmation(
        &mut self,
        core: &Core,
        target: Mode,
    ) -> Result<Option<&'static str>, DriverError> {
        let named = mode_id(target);
        if named == mode_id(core.mode) {
            return Ok(None);
        }
        core.emit(Event::Notice(format!(
            "Antigravity confirmed the switch to {} late, after the switch to {label}. \
             Octet sets {label} back before the next prompt",
            target.label(),
            label = core.mode.label()
        )))?;
        self.follow.due = Some(named.to_owned());
        Ok(Some(named))
    }

    /// A request from Antigravity. A permission request for this session
    /// marks its tool call as asked while a turn runs, being cancelled or
    /// not: a request rejected during a cancel was still a request. While
    /// the turn runs uncancelled, it is shown to the user, who decides.
    /// Anything else gets the stray reply (see `acp::refuse_request`).
    async fn vendor_request(&mut self, core: &mut Core, v: Value) -> Result<(), DriverError> {
        let asks = v["method"] == "session/request_permission"
            && acp::names_session(&v, &core.session)
            && core.phase.is_running();
        if asks {
            let tool_call = &v["params"]["toolCall"];
            let known = self.tools.known_target(tool_call);
            self.tools.mark_asked(tool_call, core.mode);
            if core.phase == Phase::InTurn {
                let (detail, summary) = acp::approval(tool_call, known.as_deref(), PROVIDER.title);
                return core.queue_approval(v, detail, summary).await;
            }
        }
        acp::refuse_request(core, &v, PERMISSION_IDS, PROVIDER.title).await
    }

    /// Records the model, the mode and the thinking levels `config_options`
    /// reports as current. Options without a `thought_level` one mean the
    /// model has no levels; a model's first report of its level is the one
    /// it opened at.
    fn note_options(&mut self, config_options: &Value) {
        let Some(list) = config_options.as_array() else {
            return;
        };
        let model = acp::current(config_options, "model").map(str::to_owned);
        let changed = model.is_some() && model != self.model;
        if let Some(model) = model {
            self.model = Some(model);
        }
        if let Some(mode) = acp::current(config_options, "mode") {
            self.mode = Some(mode.to_owned());
        }
        self.levels.note(list, "thought_level", changed);
    }

    /// Antigravity's commands: kept for compaction and shown as skills. A
    /// probe's arrive once its session is open; then the probe is done.
    fn commands_listed(&mut self, core: &Core, list: &Value) -> Result<(), DriverError> {
        let skills = acp::commands(list);
        self.commands = skills.iter().map(|skill| skill.name.clone()).collect();
        if !core.config.catalog_only {
            return core.emit(Event::Skills(skills));
        }
        // A repeated list, after the probe was answered, is not the probe's.
        if self.probe_wait.take().is_none() {
            return Ok(());
        }
        core.emit(Event::Skills(skills))?;
        core.emit(Event::Ready {
            session: String::new(),
        })
    }

    /// A probe whose commands did not come in time is done with the models
    /// alone.
    fn probe_timed_out(&mut self, core: &Core) -> Result<(), DriverError> {
        if !acp::passed(&mut self.probe_wait) {
            return Ok(());
        }
        core.emit(Event::Ready {
            session: String::new(),
        })
    }

    /// The prompt's reply ends the turn, with its token count when it has
    /// one; a turn Octet cancelled fails with its reason, whatever the reply
    /// says. An approval still waiting is denied first, so Antigravity is not
    /// left with a request open. A set-back due is sent then.
    async fn prompt_ended(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        core.deny_all_pending().await?;
        if !core.phase.is_running() {
            return Ok(());
        }
        self.tools.clear();
        self.stop_deadline = None;
        self.stop_trip = false;
        if let Some(reason) = self.stop_reason.take() {
            core.finish_turn(Outcome::Failed, Some(reason))?;
        } else {
            acp::end_prompt(core, v, self.cancel_sent)?;
        }
        self.send_due_set_back(core).await
    }

    /// A `tool_call` or `tool_call_update`: the call as it now stands (see
    /// `tools::Tools::frame`), judged under the mode Octet shows. A call
    /// that ran unasked where it should have asked trips the session; one
    /// Octet cannot remember while the mode asks cancels the turn; the
    /// turn's first subagent is reported while the mode asks.
    async fn tool(&mut self, core: &mut Core, update: &Value) -> Result<(), DriverError> {
        let (call, stop) = self.tools.frame(update, core.mode);
        let subagent = self.tools.subagent_started(&call.name, core.mode);
        core.emit(Event::Tool(call))?;
        if subagent {
            core.emit(Event::Notice(tools::SUBAGENT_NOTICE.to_owned()))?;
        }
        match stop {
            Some(stop) => {
                let trip = matches!(stop, tools::Stop::Unasked(_));
                self.cancel_turn(core, stop.reason(), trip).await.map(drop)
            }
            None => Ok(()),
        }
    }

    /// Cancels the running turn as opencode's `stop_subtask` does, keeping
    /// `reason`: the turn ends only when the prompt's own reply comes, and
    /// fails with it then. A `trip` marks the session whatever the turn's
    /// state. A turn already being cancelled (by Octet, or by the user) gets
    /// no second cancel; it keeps its reason, except that a trip's replaces
    /// any reason that is not a trip's (the user must hear what ran), so a
    /// turn the user cancelled then fails too. Whether the reason was kept.
    async fn cancel_turn(
        &mut self,
        core: &mut Core,
        reason: String,
        trip: bool,
    ) -> Result<bool, DriverError> {
        self.tripped |= trip;
        match core.phase {
            Phase::InTurn => {
                core.phase = Phase::Interrupting;
                <Self as Protocol>::interrupt(self, core).await?;
                core.deadline = deadline_after(core.limits.interrupt);
                core.deny_all_pending().await?;
            }
            Phase::Interrupting if trip && !self.stop_trip => {}
            _ => return Ok(false),
        }
        self.stop_reason = Some(reason);
        self.stop_trip = trip;
        // Antigravity's own timer, at the interrupt deadline, fires first:
        // the driver's watchdog is put a whole interrupt limit later, so the
        // reason is not lost to it even when the timer wakes late.
        if self.stop_deadline.is_none() {
            self.stop_deadline = Some(core.deadline);
            core.deadline += core.limits.interrupt;
        }
        Ok(true)
    }

    /// A turn cancelled with a kept reason that Antigravity has not ended in
    /// time stops the session, as the driver's interrupt watchdog would, but
    /// saying the reason.
    fn stop_expired(&self, core: &Core) -> Result<(), DriverError> {
        let (Some(at), Some(reason)) = (self.stop_deadline, &self.stop_reason) else {
            return Ok(());
        };
        if at > Instant::now() || !core.phase.is_running() {
            return Ok(());
        }
        Err(format!(
            "{reason}. Antigravity did not stop within {} of the cancel; session stopped",
            seconds(core.limits.interrupt)
        )
        .into())
    }

    /// A `config_option_update`: the options are recorded; a model of
    /// Antigravity's own choosing reaches the header (a reconnect still asks
    /// for the user's), and a mode of its own is followed.
    async fn options_updated(
        &mut self,
        core: &mut Core,
        options: &Value,
    ) -> Result<(), DriverError> {
        let model = self.model.clone();
        self.note_options(options);
        if core.phase.is_ready()
            && self.model != model
            && let Some(model) = &self.model
        {
            core.emit(Event::ModelSelected(model.clone()))?;
        }
        self.follow_options(core, options).await
    }
}

/// What Octet says when Antigravity reports a `version` (`None`: it named
/// none) Octet was not checked against; `None` for a checked one.
fn version_notice(version: Option<&str>) -> Option<String> {
    let version = version.unwrap_or("unknown");
    (!ANTIGRAVITY_VERIFIED_VERSIONS.contains(&version)).then(|| {
        format!(
            "Octet has not been checked against Antigravity {} (checked: {}). If a command or an edit \
             runs without asking in ask or accept-edits, Octet cancels the turn and stops sending prompts",
            one_line(version),
            ANTIGRAVITY_VERIFIED_VERSIONS.join(", ")
        )
    })
}

/// Why the session could not be opened: [`SIGN_IN`] when Antigravity looks
/// signed out (code `-32000`, or a message about signing in), then its own
/// message, which may say otherwise (the signed-out reply of the real CLI
/// is unconfirmed); else its message.
fn open_error(error: &Value) -> String {
    let message = acp::error_text(error);
    let lower = message.to_lowercase();
    let signed_out = error["code"] == -32000
        || ["auth", "sign in", "log in", "login"]
            .iter()
            .any(|word| lower.contains(word));
    if signed_out {
        format!("{SIGN_IN}. Antigravity said: {message}")
    } else {
        format!("Antigravity could not open the session: {message}")
    }
}

/// Why a `mode` reply that names no mode is no confirmation.
const NAMED_NO_MODE: &str = "its reply named no mode";

/// The mode a `mode` reply's `options` report as current; `Null` when they
/// name none.
fn reported_mode(options: &Value) -> &Value {
    &acp::option(options, "mode")["currentValue"]
}

/// Why a reply reporting the mode `reported` is not the one asked for.
fn reported_reason(reported: &Value) -> String {
    format!(
        "Antigravity reported mode {}",
        reported
            .as_str()
            .map_or_else(|| short(&reported.to_string()), one_line)
    )
}

/// The error that stops the session when Antigravity would not switch to
/// mode `id`, for `reason`: whatever mode it kept may ask for less than the
/// mode the user chose.
fn mode_refused(id: &str, reason: &str) -> DriverError {
    format!(
        "Antigravity would not switch to mode {id} ({reason}); \
         Octet stopped the session so Antigravity never acts outside the mode you chose"
    )
    .into()
}

/// A skill's prompt text: its slash command, `/name args` trimmed, after
/// `context` and a blank line when there is context.
fn skill_text(name: &str, args: &str, context: Option<&str>) -> String {
    let command = format!("/{name} {args}");
    let command = command.trim();
    match context.filter(|context| !context.is_empty()) {
        Some(context) => format!("{context}\n\n{command}"),
        None => command.to_owned(),
    }
}

/// Antigravity counterpart of `codex_stray_reply`: the reply to a request
/// Octet will not put in front of the user. A permission request is
/// rejected with its `reject_once` option (else `deny`); anything else
/// (`fs/*` and `terminal/*` included, which Octet does not offer) is an
/// unsupported method. A frame that is not a request gets none.
pub fn antigravity_stray_reply(value: &Value) -> Option<Value> {
    acp::stray_reply(value, PERMISSION_IDS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_in_is_recognised_by_code_or_by_words() {
        for (error, said) in [
            (json!({"code": -32000, "message": "Something"}), "Something"),
            (
                json!({"code": -32603, "message": "Please LOG IN again"}),
                "Please LOG IN again",
            ),
            (
                json!({"code": -32603, "message": "Unauthenticated"}),
                "Unauthenticated",
            ),
        ] {
            assert_eq!(
                open_error(&error),
                format!("{SIGN_IN}. Antigravity said: {said}"),
                "{error}"
            );
        }
        // Antigravity's message is bounded as every vendor message is.
        let long = json!({"code": -32000, "message": "x".repeat(100_000)});
        let shown = open_error(&long);
        assert!(shown.ends_with("[detail exceeds preview limit]"), "{shown}");
        assert!(shown.len() < 40_000);
        assert_eq!(
            open_error(&json!({"code": -32603, "message": "Failed to read session x"})),
            "Antigravity could not open the session: Failed to read session x"
        );
    }

    #[test]
    fn antigravity_stray_requests_are_refused() {
        let read =
            json!({"jsonrpc": "2.0", "id": "7", "method": "fs/read_text_file", "params": {}});
        let reply = antigravity_stray_reply(&read).unwrap();
        assert_eq!(reply["error"]["code"], -32601);
        assert_eq!(reply["id"], "7");
        // A permission request is rejected with its own `reject_once` option,
        // else with Antigravity's `deny`.
        let asked = |options: Value| {
            json!({"jsonrpc": "2.0", "id": "2", "method": "session/request_permission",
                "params": {"sessionId": "s", "toolCall": {}, "options": options}})
        };
        for (options, chosen) in [
            (
                json!([{"optionId": "yes", "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]),
                "no",
            ),
            (json!([]), "deny"),
        ] {
            let reply = antigravity_stray_reply(&asked(options)).unwrap();
            assert_eq!(reply["result"]["outcome"]["optionId"], chosen);
            assert_eq!(reply["id"], "2", "a string ID comes back a string");
        }
        // A notification is no request.
        assert_eq!(
            antigravity_stray_reply(&json!({"jsonrpc": "2.0", "method": "session/update"})),
            None
        );
    }

    #[test]
    fn the_mode_descriptions_name_what_octet_has_not_checked() {
        assert_eq!(
            PROVIDER.modes,
            [
                "Antigravity asks before commands and edits; reads inside the workspace never ask; \
                 a subagent's requests never reach Octet; Octet has not checked MCP tools (mode default)",
                "Edits proceed; commands and other actions ask; a subagent's requests never reach Octet; \
                 Octet has not checked MCP tools (mode accept-edits)",
                "Antigravity's own checker and sandbox decide; it asks rarely \
                 (network and delete commands ran unasked when tested) (mode auto)",
                "No permission checks at all (toolPermission always-proceed)",
            ]
        );
    }

    #[test]
    fn a_skill_is_its_slash_command_after_any_context() {
        assert_eq!(skill_text("review", "src", None), "/review src");
        assert_eq!(skill_text("review", "", None), "/review");
        assert_eq!(skill_text("review", "", Some("")), "/review");
        assert_eq!(
            skill_text("review", " a b ", Some("earlier")),
            "earlier\n\n/review  a b"
        );
    }

    #[test]
    fn a_checked_version_says_nothing() {
        assert_eq!(version_notice(Some("1.5.5")), None);
        assert!(version_notice(None).unwrap().starts_with(
            "Octet has not been checked against Antigravity unknown (checked: 1.5.5)"
        ));
    }

    #[test]
    fn a_level_option_that_lists_no_level_is_no_level_at_all() {
        let mut protocol = AntigravityProtocol::default();
        protocol.note_options(&json!([
            {"id": "model", "currentValue": "m", "options": [{"value": "m"}]},
            {"id": "thought_level", "currentValue": "high", "options": []},
        ]));
        assert_eq!(protocol.effort_refusal("low").as_deref(), Some(NO_LEVELS));
        // A listed level is taken; one not listed is refused, naming them.
        protocol.note_options(&json!([
            {"id": "model", "currentValue": "m", "options": [{"value": "m"}]},
            {"id": "thought_level", "currentValue": "high",
             "options": [{"value": "low"}, {"value": "high"}]},
        ]));
        assert_eq!(protocol.effort_refusal("low"), None);
        assert_eq!(
            protocol.effort_refusal("max").as_deref(),
            Some("Antigravity takes effort low or high for this model; it keeps high")
        );
    }
}
