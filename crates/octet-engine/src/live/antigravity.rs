//! The Antigravity CLI (`agy acp`) over the Agent Client Protocol: its row,
//! the launch (Octet's own settings folder, the workspace's `.agents` check,
//! a probe's scratch home), the handshake, the model catalog and thinking
//! levels, turns, resume and the probe.
mod guarded;
mod settings;
mod trust;

use super::{
    BoxFuture, Channels, Config, DriverError, Event, ImageAttachment, Limits, Mode, NOT_SENT,
    Outcome, Prepared, Provider, ToolCall, ToolPhase, acp,
    activity::one_line,
    deadline_after,
    driver::{effort_notice, seconds},
    protocol::{Core, Phase, Protocol},
    short, valid_identifier,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    io::ErrorKind,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;
pub use trust::{
    WorkspaceConfig, antigravity_trust, antigravity_trusted, antigravity_workspace_config,
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
            "Antigravity asks before commands, edits and other actions; reads inside the workspace ",
            "never ask; a subagent's requests never reach Octet (mode default)"
        ),
        "Edits proceed; commands and other actions ask; a subagent's requests never reach Octet (mode accept-edits)",
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
    launch_args,
    launch_env: super::no_env,
    prepare,
    start,
};

/// Why a session cannot start with plugins or their hooks switched off.
const NO_PLUGIN_SWITCH: &str = concat!(
    "Antigravity has no switch to turn plugins or their hooks off; ",
    "start without --no-plugins and --no-plugin-hooks"
);
/// What Octet says when Antigravity is not signed in.
const SIGN_IN: &str = "Sign in to Antigravity first: run `agy`";
/// Why a session cannot start without Octet's data directory.
const NO_DATA_DIR: &str = "Antigravity needs Octet's data directory to keep its settings in";
/// Why a probe cannot start while another of this process runs.
const PROBE_BUSY: &str = "Octet is already asking Antigravity for its models";
/// What Octet says when a model has no thinking level.
const NO_LEVELS: &str = "This Antigravity model takes no thinking level";
/// The most tool calls one turn remembers.
const MAX_TOOLS: usize = 1024;
/// The most thinking levels one model's option keeps.
const MAX_EFFORTS: usize = 32;
/// How long a probe waits, after the session opens, for Antigravity to list
/// its commands before it settles for the models alone.
const PROBE_COMMANDS_WAIT: Duration = Duration::from_secs(2);
/// How old a probe's home must be before a later probe removes it: one left
/// by a process that died.
const PROBE_STALE: Duration = Duration::from_hours(1);
/// The permission options Octet answers with when a request lists none.
const PERMISSION_IDS: (&str, &str) = ("allow", "deny");

/// Runs one Antigravity session through the shared driver.
fn start(config: Config, limits: Limits, channels: Channels) -> BoxFuture<Result<(), String>> {
    Box::pin(
        async move { super::driver::run::<AntigravityProtocol>(config, limits, channels).await },
    )
}

/// Antigravity's ID for `mode`. Full access is `default` under the
/// `always-proceed` settings.
fn mode_id(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask | Mode::FullAccess => "default",
        Mode::AcceptEdits => "accept-edits",
        Mode::Auto => "auto",
    }
}

/// The mode Antigravity's mode `id` carries in a session launched for
/// `pinned`: `default` is full access under that pin, else ask. `None` for
/// `plan` or a mode Octet does not know.
fn mode_of(id: &str, pinned: Mode) -> Option<Mode> {
    match id {
        "default" if pinned == Mode::FullAccess => Some(Mode::FullAccess),
        "default" => Some(Mode::Ask),
        "accept-edits" => Some(Mode::AcceptEdits),
        "auto" => Some(Mode::Auto),
        _ => None,
    }
}

/// The arguments that start `agy acp` with Octet's settings folder for the
/// mode, and a probe in its scratch home. Without a data directory it is
/// `acp` alone, which never launches: `prepare` fails first.
fn launch_args(config: &Config) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["acp".into()];
    let Some(data_dir) = &config.data_dir else {
        return args;
    };
    args.push("--cli_dir_name".into());
    args.push(settings::cli_dir(data_dir, config.mode).into_os_string());
    if config.catalog_only {
        args.push("--gemini-dir".into());
        args.push(settings::probe_home(data_dir).into_os_string());
    }
    args
}

/// The row's `prepare`: [`antigravity_prepare`] with Antigravity's own folder.
fn prepare(config: &Config) -> Result<Prepared, String> {
    antigravity_prepare(config, settings::vendor_home().as_deref())
}

/// What runs before `agy` starts, in order: Octet's data directory must be
/// known; plugins and their hooks cannot be switched off (except for a
/// probe, which loads none); the workspace's `.agents` configuration must be
/// trusted; Octet's settings file for the mode is written from the user's in
/// `vendor_home`. A probe also removes the probe homes last changed over an
/// hour ago (a process that died left them), and makes its own. `Prepared`
/// carries the settings' notices, the probe's home and the fingerprint of the
/// configuration accepted.
///
/// # Errors
///
/// Why the session cannot start, in words to show.
pub fn antigravity_prepare(
    config: &Config,
    vendor_home: Option<&Path>,
) -> Result<Prepared, String> {
    let data_dir = config
        .data_dir
        .as_deref()
        .ok_or_else(|| NO_DATA_DIR.to_owned())?;
    let switched_off = !config.plugins.load || !config.plugins.hooks;
    if switched_off && !config.catalog_only {
        return Err(NO_PLUGIN_SWITCH.to_owned());
    }
    let workspace = trust::check(config, data_dir)?;
    let notices = settings::write(data_dir, config.mode, vendor_home)?;
    let scratch = if config.catalog_only {
        Some(probe_folder(data_dir)?)
    } else {
        None
    };
    Ok(Prepared {
        notices,
        scratch,
        workspace,
    })
}

/// Makes this process's probe home, owner-only, after removing the stale
/// ones. It is made exclusively: one already there belongs to a probe that
/// is running, which keeps it.
fn probe_folder(data_dir: &Path) -> Result<PathBuf, String> {
    remove_stale_probes(&data_dir.join("antigravity"));
    let home = settings::probe_home(data_dir);
    match std::fs::DirBuilder::new().mode(0o700).create(&home) {
        Ok(()) => Ok(home),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(PROBE_BUSY.to_owned()),
        Err(error) => Err(format!(
            "Octet could not make a home for Antigravity's model list ({}): {error}",
            home.display()
        )),
    }
}

/// Removes the probe homes in `folder` last modified over [`PROBE_STALE`]
/// ago: folders (never links) whose name starts with `probe-`. One whose
/// age cannot be read is left alone, and a failure to remove one ignored.
fn remove_stale_probes(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .as_encoded_bytes()
            .starts_with(settings::PROBE_PREFIX.as_bytes())
        {
            continue;
        }
        let path = entry.path();
        let stale = std::fs::symlink_metadata(&path).is_ok_and(|meta| {
            meta.is_dir()
                && meta
                    .modified()
                    .ok()
                    .and_then(|at| at.elapsed().ok())
                    .is_some_and(|age| age > PROBE_STALE)
        });
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
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

/// What Octet knows of the current model's thinking levels.
#[derive(Default)]
enum Levels {
    /// Nothing yet: no options reported since the model changed.
    #[default]
    Unknown,
    /// Antigravity reported options without a `thought_level` one: the
    /// model has no levels.
    Absent,
    /// The model's `thought_level` option.
    Listed(LevelOption),
}

/// The current model's `thought_level` option, as Antigravity last reported
/// it.
struct LevelOption {
    /// The levels, in Antigravity's order, bounded.
    values: Vec<String>,
    /// The level in force.
    current: Option<String>,
    /// The level the model had when it was first reported in this session:
    /// what `/effort default` goes back to.
    opening: Option<String>,
}

/// A tool call of the running turn, as its first frame named it.
struct Tool {
    /// The tool's name, else its title.
    name: String,
    /// The newest target a frame named.
    target: String,
    /// The kind of its first frame; later frames need not repeat it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the tripwire, which a later commit adds")
    )]
    kind: String,
}

/// Antigravity's per-connection state: Octet's requests awaiting replies,
/// the session's options as Antigravity last reported them, the commands it
/// lists, and the turn's tool calls.
#[derive(Default)]
pub(super) struct AntigravityProtocol {
    /// Octet's last wire request ID.
    next_id: u64,
    /// Octet's requests that have no reply yet, by wire ID, with what each
    /// reply answers.
    requests: HashMap<u64, Outstanding>,
    /// A `session/cancel` was sent this turn: an `end_turn` then means the
    /// turn was interrupted.
    cancel_sent: bool,
    /// The session's model (`currentValue` of its `model` option).
    model: Option<String>,
    /// The session's mode ID (its `mode` option, or `current_mode_update`).
    mode: Option<String>,
    /// The current model's thinking levels.
    levels: Levels,
    /// The names of the commands Antigravity lists, bounded.
    commands: Vec<String>,
    /// This turn's tool calls, by ID, at most [`MAX_TOOLS`].
    tools: HashMap<String, Tool>,
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
        launch_args(config)
    }

    fn prepared(&mut self, workspace: Option<String>) {
        self.workspace = workspace;
    }

    fn answer(wire: &Value, allow: bool) -> Value {
        acp::permission_reply(wire, allow, PERMISSION_IDS)
    }

    fn deadline(&self) -> Option<Instant> {
        self.probe_wait
    }

    /// A probe whose commands never came settles for the models.
    fn on_deadline(
        &mut self,
        core: &mut Core,
    ) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(self.probe_timed_out(core))
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

    /// Sends `initialize`, offering no client capabilities.
    async fn initialize(&mut self, core: &mut Core) -> Result<(), DriverError> {
        let params = json!({"protocolVersion": 1, "clientCapabilities": {}});
        self.request(core, Outstanding::Initialize, "initialize", params)
            .await
    }

    /// The workspace is checked first; then the text, with the images
    /// inline after it.
    async fn send_prompt(
        &mut self,
        core: &mut Core,
        text: &str,
        images: &[ImageAttachment],
    ) -> Result<(), DriverError> {
        if let Err(refusal) = self.workspace_unchanged(core).await {
            return core.finish_turn(Outcome::Failed, Some(format!("{NOT_SENT}{refusal}")));
        }
        let Some(blocks) = acp::image_blocks(core, PROVIDER.title, images).await? else {
            return Ok(());
        };
        let mut prompt = vec![json!({"type": "text", "text": text})];
        prompt.extend(blocks);
        let params = json!({"sessionId": core.session, "prompt": prompt});
        self.request(core, Outstanding::Prompt, "session/prompt", params)
            .await
    }

    /// Not yet offered: the turn fails, and nothing is sent. Nothing here
    /// waits, so the work is done at once and handed back as a ready future.
    fn send_skill(
        &mut self,
        core: &mut Core,
        _name: &str,
        _path: Option<&Path>,
        _args: &str,
        _context: Option<&str>,
    ) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(core.finish_turn(
            Outcome::Failed,
            Some("Antigravity skills arrive with a later commit".to_owned()),
        ))
    }

    /// Not yet offered: the turn fails, and nothing is sent.
    fn compact(&mut self, core: &mut Core) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(core.finish_turn(
            Outcome::Failed,
            Some("Antigravity compaction arrives with a later commit".to_owned()),
        ))
    }

    /// Sends the `session/cancel` notification. An `end_turn` then reads as
    /// interrupted.
    async fn interrupt(&mut self, core: &mut Core) -> Result<(), DriverError> {
        self.cancel_sent = true;
        core.send(acp::cancel(&core.session)).await
    }

    /// Not yet offered: refused with a notice, and nothing is sent; the
    /// header goes back to the mode in force.
    fn set_mode(
        &mut self,
        core: &mut Core,
        _target: Mode,
    ) -> impl Future<Output = Result<(), DriverError>> + Send {
        std::future::ready(
            core.emit(Event::Notice(
                "Antigravity mode switches arrive with a later commit".to_owned(),
            ))
            .and_then(|()| core.emit(Event::ModeChanged(core.mode))),
        )
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
            return refuse_request(core, &v).await;
        }
        let this_session = !core.session.is_empty()
            && v.pointer("/params/sessionId").and_then(Value::as_str)
                == Some(core.session.as_str());
        if method != "session/update" || !this_session {
            return Ok(());
        }
        let update = &v["params"]["update"];
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "available_commands_update" => self.commands_listed(core, &update["availableCommands"]),
            "config_option_update" => {
                self.note_options(&update["configOptions"]);
                Ok(())
            }
            "current_mode_update" => {
                if let Some(id) = update["currentModeId"]
                    .as_str()
                    .filter(|id| valid_identifier(id))
                {
                    self.mode = Some(id.to_owned());
                }
                Ok(())
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
            // Thoughts, the prompt echoed back and the session's title are
            // not shown.
            _ => Ok(()),
        }
    }
}

impl AntigravityProtocol {
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
        core.send(acp::request(self.next_id, method, params)).await
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
            Outstanding::SetEffort(request) => self.effort_reply(core, v, request).await,
            Outstanding::SetMode => self.mode_set(core, v).await,
            Outstanding::Prompt => self.prompt_ended(core, v),
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
        self.request(core, Outstanding::Open(resumed), method, params)
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
    /// a model (and perhaps a bill) they never chose.
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
    /// (then Antigravity decides).
    fn effort_refusal(&self, level: &str) -> Option<String> {
        let (values, current) = match &self.levels {
            Levels::Unknown => return None,
            Levels::Absent => return Some(NO_LEVELS.to_owned()),
            Levels::Listed(option) => (&option.values, option.current.as_deref()),
        };
        if values.iter().any(|value| value == level) {
            return None;
        }
        let (last, rest) = values.split_last()?;
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
            Levels::Listed(option) if option.current != option.opening => option.opening.clone(),
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
    /// is reported and the interface goes back to the level in force. The
    /// handshake then sets the mode.
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
            match &acp::option(options, "mode")["currentValue"] {
                Value::Null => Some("its reply named no mode".to_owned()),
                reported if reported.as_str() == Some(id) => None,
                reported => Some(format!(
                    "Antigravity reported mode {}",
                    reported
                        .as_str()
                        .map_or_else(|| short(&reported.to_string()), one_line)
                )),
            }
        };
        if let Some(reason) = reason {
            return Err(format!(
                "Antigravity would not switch to mode {id} ({reason}); \
                 Octet stopped the session so Antigravity never acts outside the mode you chose"
            )
            .into());
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
        let Some(option) = list.iter().find(|option| option["id"] == "thought_level") else {
            self.levels = Levels::Absent;
            return;
        };
        let values: Vec<String> = option["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|choice| choice["value"].as_str())
            .filter(|value| valid_identifier(value))
            .take(MAX_EFFORTS)
            .map(str::to_owned)
            .collect();
        let current = acp::current(config_options, "thought_level").map(str::to_owned);
        let opening = match std::mem::take(&mut self.levels) {
            Levels::Listed(known) if !changed => known.opening,
            _ => current.clone(),
        };
        self.levels = Levels::Listed(LevelOption {
            values,
            current,
            opening,
        });
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
        if self
            .probe_wait
            .take_if(|wait| *wait <= Instant::now())
            .is_none()
        {
            return Ok(());
        }
        core.emit(Event::Ready {
            session: String::new(),
        })
    }

    /// The prompt's reply ends the turn, with its token count when it has one.
    fn prompt_ended(&mut self, core: &mut Core, v: &Value) -> Result<(), DriverError> {
        if !core.phase.is_running() {
            return Ok(());
        }
        self.tools.clear();
        if let Some(error) = v.get("error") {
            return core.finish_turn(Outcome::Failed, Some(acp::error_text(error)));
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
    /// name and kind are its first frame's; its target is the newest one a
    /// frame names (see `acp::frame_target`), else a command's title. At most
    /// [`MAX_TOOLS`] calls are remembered in a turn.
    fn tool(&mut self, core: &Core, update: &Value) -> Result<(), DriverError> {
        let id = one_line(update["toolCallId"].as_str().unwrap_or_default());
        let target = acp::frame_target(update);
        let (name, target) = if let Some(known) = self.tools.get_mut(&id) {
            if let Some(target) = target {
                known.target = target;
            }
            (known.name.clone(), known.target.clone())
        } else {
            let tool = Tool::first(update, target);
            let shown = (tool.name.clone(), tool.target.clone());
            if !id.is_empty() && self.tools.len() < MAX_TOOLS {
                self.tools.insert(id.clone(), tool);
            }
            shown
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
            detail: acp::content_text(&update["content"]),
        }))
    }
}

impl Tool {
    /// A call as its first frame `update` names it, with the `target` that
    /// frame names, if any.
    fn first(update: &Value, target: Option<String>) -> Self {
        let named = |key: &str| {
            update[key]
                .as_str()
                .map(one_line)
                .filter(|text| !text.is_empty())
        };
        let kind = named("kind").unwrap_or_else(|| "other".to_owned());
        let title = named("title");
        let target = target
            .or_else(|| title.clone().filter(|_| kind == "execute"))
            .unwrap_or_default();
        Self {
            name: named("name").or(title).unwrap_or_else(|| "tool".to_owned()),
            target,
            kind,
        }
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

/// Why the session could not be opened: [`SIGN_IN`] when Antigravity is not
/// signed in (code `-32000`, or a message about signing in), else its
/// message.
fn open_error(error: &Value) -> String {
    let message = acp::error_text(error);
    let lower = message.to_lowercase();
    let signed_out = error["code"] == -32000
        || ["auth", "sign in", "log in", "login"]
            .iter()
            .any(|word| lower.contains(word));
    if signed_out {
        SIGN_IN.to_owned()
    } else {
        format!("Antigravity could not open the session: {message}")
    }
}

/// A request from Antigravity, which Octet does not answer for the user yet:
/// it is rejected, never allowed and never left unanswered. A client
/// capability's (`fs/*`, `terminal/*`) is refused quietly; any other says so.
async fn refuse_request(core: &Core, v: &Value) -> Result<(), DriverError> {
    let Some(reply) = acp::stray_reply(v, PERMISSION_IDS) else {
        return Ok(());
    };
    core.send(reply).await?;
    let method = v["method"].as_str().unwrap_or_default();
    if acp::client_capability(method) {
        return Ok(());
    }
    core.emit(Event::Notice(format!(
        "Request declined: {}",
        short(method)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;

    #[test]
    fn every_mode_has_a_mode_id_and_back() {
        for mode in Mode::ALL {
            assert_eq!(mode_of(mode_id(mode), mode), Some(mode), "{mode:?}");
        }
        assert_eq!(mode_of("default", Mode::Ask), Some(Mode::Ask));
        assert_eq!(mode_of("default", Mode::Auto), Some(Mode::Ask));
        assert_eq!(mode_of("plan", Mode::Ask), None);
    }

    fn data_config(mode: Mode) -> Config {
        let mut config = Config::new(super::super::Engine::ANTIGRAVITY, "agy", "/w");
        config.data_dir = Some("/d".into());
        config.mode = mode;
        config
    }

    #[test]
    fn the_launch_names_octets_settings_folder() {
        assert_eq!(
            launch_args(&data_config(Mode::Ask)),
            ["acp", "--cli_dir_name", "/d/antigravity/review"]
        );
        assert_eq!(
            launch_args(&data_config(Mode::FullAccess)),
            ["acp", "--cli_dir_name", "/d/antigravity/proceed"]
        );
        let mut bare = data_config(Mode::Ask);
        bare.data_dir = None;
        assert_eq!(launch_args(&bare), ["acp"]);
    }

    #[test]
    fn a_probe_runs_in_a_scratch_home() {
        let mut config = data_config(Mode::Ask);
        config.catalog_only = true;
        let args = launch_args(&config);
        let home = format!("/d/antigravity/probe-{}", std::process::id());
        assert_eq!(args[3..], [OsString::from("--gemini-dir"), home.into()]);
    }

    #[test]
    fn preparing_a_probe_removes_only_old_probe_folders() {
        let root = TempDir::new("octet-agy-probe-clean");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = Config::new(super::super::Engine::ANTIGRAVITY, "agy", &cwd);
        config.data_dir = Some(root.path().join("data"));
        config.catalog_only = true;
        let folder = root.path().join("data/antigravity");
        let (old, new) = (folder.join("probe-1"), folder.join("probe-2"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_hours(2);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
        // An old link named like a probe is not followed or removed.
        let target = root.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, folder.join("probe-3")).unwrap();
        let prepared = antigravity_prepare(&config, None).unwrap();
        assert!(!old.exists(), "the old probe's folder is gone");
        assert!(new.is_dir(), "a recent probe keeps its folder");
        assert!(folder.join("probe-3").is_symlink());
        assert!(target.is_dir());
        let scratch = prepared.scratch.unwrap();
        assert!(scratch.is_dir());
        assert_eq!(scratch, settings::probe_home(&root.path().join("data")));
    }

    #[test]
    fn a_probe_ignores_the_plugin_switches_and_a_session_does_not() {
        let root = TempDir::new("octet-agy-prepare-plugins");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = Config::new(super::super::Engine::ANTIGRAVITY, "agy", &cwd);
        config.data_dir = Some(root.path().join("data"));
        config.plugins.hooks = false;
        assert_eq!(
            antigravity_prepare(&config, None),
            Err(NO_PLUGIN_SWITCH.into())
        );
        config.catalog_only = true;
        let prepared = antigravity_prepare(&config, None).unwrap();
        assert!(prepared.scratch.is_some());
        assert_eq!(prepared.workspace, None);
    }

    #[test]
    fn the_accepted_fingerprint_is_handed_on() {
        let root = TempDir::new("octet-agy-prepare-fingerprint");
        let cwd = root.path().join("ws");
        std::fs::create_dir_all(cwd.join(".agents")).unwrap();
        std::fs::write(cwd.join(".agents/hooks.json"), "{}").unwrap();
        let mut config = Config::new(super::super::Engine::ANTIGRAVITY, "agy", &cwd);
        config.data_dir = Some(root.path().join("data"));
        config.trust_workspace = true;
        let prepared = antigravity_prepare(&config, None).unwrap();
        let found = antigravity_workspace_config(&cwd).unwrap().unwrap();
        assert_eq!(prepared.workspace, Some(found.fingerprint));
        assert_eq!(prepared.scratch, None);
    }

    #[test]
    fn signing_in_is_recognised_by_code_or_by_words() {
        for error in [
            json!({"code": -32000, "message": "Something"}),
            json!({"code": -32603, "message": "Please LOG IN again"}),
            json!({"code": -32603, "message": "Unauthenticated"}),
        ] {
            assert_eq!(open_error(&error), SIGN_IN, "{error}");
        }
        assert_eq!(
            open_error(&json!({"code": -32603, "message": "Failed to read session x"})),
            "Antigravity could not open the session: Failed to read session x"
        );
    }

    #[test]
    fn a_tool_is_named_by_its_name_then_its_title() {
        let tool = Tool::first(
            &json!({"name": "run_command", "kind": "execute", "title": "ls -la"}),
            None,
        );
        assert_eq!(
            (tool.name.as_str(), tool.target.as_str()),
            ("run_command", "ls -la")
        );
        assert_eq!(tool.kind, "execute");
        let tool = Tool::first(&json!({"kind": "read", "title": "Read file"}), None);
        assert_eq!(
            (tool.name.as_str(), tool.target.as_str()),
            ("Read file", "")
        );
        let tool = Tool::first(&json!({}), Some("/w/a".into()));
        assert_eq!((tool.name.as_str(), tool.target.as_str()), ("tool", "/w/a"));
        assert_eq!(tool.kind, "other");
    }

    #[test]
    fn a_checked_version_says_nothing() {
        assert_eq!(version_notice(Some("1.5.5")), None);
        assert!(version_notice(None).unwrap().starts_with(
            "Octet has not been checked against Antigravity unknown (checked: 1.5.5)"
        ));
    }
}
