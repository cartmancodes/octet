//! The gate's Antigravity client: `agy acp` over the Agent Client Protocol,
//! launched exactly as Octet launches it (`antigravity_prepare`, then
//! `launch_args`), plus `--gemini-dir` naming a scratch home so no run adds a
//! session to the user's Antigravity history. Each run keeps Octet's data
//! directory and that home in a second temporary folder, owner-only, removed
//! on every exit path. A scenario ends `passed`, `failed: <why>` or
//! `unproven: <why>`: the run did not exercise what the scenario claims to
//! prove, which is never a pass.
use crate::{Evidence, GateError, GateProcess};
use octet_engine::live::{
    ANTIGRAVITY_VERIFIED_VERSIONS, Config, Engine, Mode, antigravity_prepare,
    antigravity_stray_reply, launch_args, launch_env,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    os::unix::fs::DirBuilderExt as _,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

/// The model every scenario runs on.
const MODEL: &str = "gemini-3.8-flash";
/// The thinking level every scenario runs at.
const LEVEL: &str = "low";
/// Antigravity's mode for Octet's `ask`, the mode every scenario runs in.
const MODE: &str = "default";
/// Octet's data directory inside the scratch folder.
const DATA: &str = "octet-data";
/// The `--gemini-dir` home inside the scratch folder.
const HOME: &str = "gemini-home";
/// `lockdown`'s user settings, in the scratch home where Octet reads the
/// user's: every way of skipping a request the user could have set, and a
/// marker that shows the file went through Octet's merge.
const LOCKDOWN_SETTINGS: &str = concat!(
    r#"{"toolPermission":"always-proceed","agentMode":"accept-edits","#,
    r#""permissions":{"allow":["command(*)","write_file(*)"]},"octetGateMarker":true}"#
);
/// The fixture writes `approval-*` asks for, as the request may show them.
const FIXTURE_COMMANDS: [&str; 3] = [
    "printf READY > probe.out",
    "printf 'READY' > probe.out",
    "printf \"READY\" > probe.out",
];
/// The word `resume` and `compact` have the model reply with, then recall.
const WORD: &str = "PERIWINKLE";
/// The file `lockdown` asks the model to write.
const GATE_FILE: &str = "gate.txt";
/// How `subagent`'s second turn must begin its one-line reply.
const STATE_PREFIX: &str = "STATE: ";
/// The file `subagent` asks a subagent to touch.
const SUB_FILE: &str = "gate-sub.txt";
/// How long `subagent` lets the subagent start before asking for its state.
const SUBAGENT_SETTLE: Duration = Duration::from_secs(15);
/// How long `subagent` watches, after the subagent's state is reported, for
/// its command to run.
const SUBAGENT_WATCH: Duration = Duration::from_secs(30);
/// Tool call kinds that change something: one that completes without a
/// permission request fails every scenario.
const ACTING: [&str; 4] = ["execute", "edit", "delete", "move"];
/// The most tool calls, requests, commands or texts a run keeps.
const LIMIT: usize = 256;
/// The most characters kept of one reply text, vendor message or output.
const TEXT_LIMIT: usize = 512;

/// The scenario's default time limit in seconds: those that run several
/// turns, a second process or a wait need more than the gate's 90.
pub fn default_seconds(scenario: &str) -> u64 {
    match scenario {
        "subagent" => 240,
        "lockdown" | "resume" | "compact" => 180,
        _ => 90,
    }
}

/// What an Antigravity run found.
#[derive(Debug)]
pub struct Run {
    /// `passed`, `failed: <why>` or `unproven: <why>`.
    pub status: String,
    /// The counts every engine reports.
    pub evidence: Evidence,
    /// Every child stopped.
    pub cleaned: bool,
    /// What only Antigravity reports: its version, the settings and launch
    /// the gate checked, the options confirmed, each permission request and
    /// tool call, and the scratch folder's layout.
    pub extra: Value,
}

/// Why a scenario stopped short of passing.
#[derive(Debug)]
enum Stop {
    /// The vendor did something else, or the gate could not go on.
    Failed(String),
    /// The run did not exercise what the scenario claims to prove.
    Unproven(&'static str),
}

impl From<GateError> for Stop {
    fn from(error: GateError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// The status text for `result`.
fn status(result: &Result<(), Stop>) -> String {
    match result {
        Ok(()) => "passed".to_owned(),
        Err(Stop::Failed(why)) => format!("failed: {why}"),
        Err(Stop::Unproven(why)) => format!("unproven: {why}"),
    }
}

/// The second temporary folder: Octet's data directory and the scratch
/// home. Made owner-only; removed, with everything in it, when this drops.
#[derive(Debug)]
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    /// Makes a new folder in the system's temporary folder, with its two
    /// folders.
    fn create() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let root =
            std::env::temp_dir().join(format!("octet-protocol-agy-{}-{nonce}", std::process::id()));
        let private = |path: &Path| fs::DirBuilder::new().mode(0o700).create(path);
        private(&root).map_err(|error| format!("cannot create the scratch folder: {error}"))?;
        let scratch = Self { root };
        for folder in [scratch.data(), scratch.home()] {
            private(&folder).map_err(|error| format!("cannot create {DATA}/{HOME}: {error}"))?;
        }
        Ok(scratch)
    }

    /// Octet's data directory (`Config.data_dir`).
    fn data(&self) -> PathBuf {
        self.root.join(DATA)
    }

    /// The home `agy` runs in (`--gemini-dir`), where Octet reads the
    /// user's settings from.
    fn home(&self) -> PathBuf {
        self.root.join(HOME)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// How the gate starts `agy`: Octet's arguments and environment, and the
/// notices Octet's preparation gave.
struct Launch {
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    notices: Vec<String>,
}

/// Prepares and checks the launch in `cwd` as Octet does in `ask`, with
/// `scratch` as its data directory and the user's home.
fn launch(binary: &Path, cwd: &Path, scratch: &Scratch) -> Result<Launch, String> {
    let mut config = Config::new(Engine::ANTIGRAVITY, binary, cwd);
    config.mode = Mode::Ask;
    config.data_dir = Some(scratch.data());
    let prepared = antigravity_prepare(&config, Some(&scratch.home()))?;
    let mut args = launch_args(&config);
    args.extend([
        OsString::from("--gemini-dir"),
        scratch.home().into_os_string(),
    ]);
    check_launch(&args, &scratch.data(), &scratch.home())?;
    Ok(Launch {
        args,
        env: launch_env(&config),
        notices: prepared.notices,
    })
}

/// The value after the one `flag` in `args`; `None` unless it is there
/// exactly once, with a value.
fn flag_value<'a>(args: &'a [OsString], flag: &str) -> Option<&'a OsString> {
    let mut at = args.iter().enumerate().filter(|(_, arg)| *arg == flag);
    let (index, _) = at.next()?;
    at.next().is_none().then(|| args.get(index + 1))?
}

/// The folder `agy` 1.5.5 uses for `--cli_dir_name` `name` when its home is
/// `home`: `name` joined under `<home>/antigravity-cli` with any leading `/`
/// removed, `.` and `..` folded by name, a `..` at the root dropped.
fn resolved_by_agy(home: &Path, name: &Path) -> PathBuf {
    let joined = home
        .join("antigravity-cli")
        .join(name.strip_prefix("/").unwrap_or(name));
    let mut folded = PathBuf::from("/");
    for part in joined.components() {
        match part {
            Component::Normal(part) => folded.push(part),
            Component::ParentDir => {
                folded.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    folded
}

/// Whether `args` start `agy acp` with `--gemini-dir` exactly `home`, a
/// `--cli_dir_name` that `agy` resolves (see [`resolved_by_agy`]) to a
/// settings folder under `data` (never the full-access one), and nothing
/// that skips permission requests. The gate launches nothing else.
fn check_launch(args: &[OsString], data: &Path, home: &Path) -> Result<(), String> {
    if args.first().is_none_or(|first| first != "acp") {
        return Err("the launch is not `agy acp`; the gate refuses to launch it".into());
    }
    if args
        .iter()
        .any(|arg| arg == "--dangerously-skip-permissions")
    {
        return Err("the launch skips permission requests; the gate refuses to launch it".into());
    }
    if flag_value(args, "--gemini-dir").map(Path::new) != Some(home) {
        return Err(format!(
            "the launch does not run in the scratch home {}; the gate refuses to launch it",
            home.display()
        ));
    }
    let settings =
        flag_value(args, "--cli_dir_name").map(|name| resolved_by_agy(home, Path::new(name)));
    if settings.is_none_or(|dir| !dir.starts_with(data) || dir.ends_with("proceed")) {
        return Err(format!(
            "the launch's settings folder, as agy resolves it, is not Octet's under {}; \
             the gate refuses to launch it",
            data.display()
        ));
    }
    Ok(())
}

/// Octet's settings folder for the modes that ask.
fn review(scratch: &Scratch) -> PathBuf {
    scratch.data().join("antigravity/review")
}

/// Whether `agy` made its `log` folder in Octet's settings folder `review`:
/// the sign that it used that folder, so read Octet's settings there.
fn agy_used(review: &Path) -> bool {
    review.join("log").is_dir()
}

/// Octet's settings file as Octet's preparation wrote it.
fn written_settings(scratch: &Scratch) -> Option<Value> {
    let path = review(scratch).join("settings.json");
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Whether Octet's settings `settings` show `lockdown`'s fixture went
/// through Octet's merge with every way of skipping a request removed.
fn lockdown_merged(settings: Option<&Value>) -> Result<(), Stop> {
    let Some(map) = settings.and_then(Value::as_object) else {
        return Err(Stop::Unproven("Octet's settings file could not be read"));
    };
    let has = |key: &str| map.keys().any(|name| name.eq_ignore_ascii_case(key));
    let allows = map
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("permissions"))
        .any(|(_, value)| {
            value
                .as_object()
                .is_some_and(|rules| rules.keys().any(|name| name.eq_ignore_ascii_case("allow")))
        });
    if map.get("octetGateMarker") != Some(&json!(true)) {
        Err(Stop::Unproven(
            "Octet's settings lack the fixture's marker: the fixture did not go through Octet's merge",
        ))
    } else if map.get("toolPermission") != Some(&json!("request-review")) {
        Err(Stop::Unproven(
            "Octet's settings do not pin toolPermission to request-review",
        ))
    } else if has("agentMode") || allows {
        Err(Stop::Unproven(
            "Octet's settings keep the fixture's agentMode or permissions.allow",
        ))
    } else {
        Ok(())
    }
}

/// What the gate saw of the settings file Octet wrote: its key names and
/// the `toolPermission` it pins. No values beyond that are recorded.
fn settings_summary(settings: Option<&Value>) -> Value {
    let Some(map) = settings.and_then(Value::as_object) else {
        return Value::Null;
    };
    let keys: Vec<&String> = map.keys().take(64).collect();
    json!({"keys": keys, "toolPermission": map.get("toolPermission")})
}

/// `text` cut to `limit` characters, any word that looks like an email
/// address replaced: vendor text must not put an account in the evidence.
fn scrub(text: &str, limit: usize) -> String {
    let cut: String = text.chars().take(limit).collect();
    cut.split(' ')
        .map(|word| {
            if word.contains('@') {
                "<redacted>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The scratch folder's entries, relative, to 16 levels and at most 80,
/// folders ending in `/`; names with `@` in them redacted. Links are not
/// followed.
fn layout(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut queue = vec![(root.to_owned(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = queue.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if found.len() >= 80 {
                return found;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let name = if name.contains('@') {
                "<redacted>".to_owned()
            } else {
                name
            };
            let relative = format!("{prefix}{name}");
            let folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if folder {
                found.push(format!("{relative}/"));
                if depth < 15 {
                    queue.push((entry.path(), format!("{relative}/"), depth + 1));
                }
            } else {
                found.push(relative);
            }
        }
    }
    found
}

/// One tool call, as its updates showed it.
#[derive(Debug)]
struct Tool {
    id: String,
    name: String,
    kind: String,
    /// Each status it reported, in order, repeats dropped (at most 8).
    statuses: Vec<String>,
    /// The names of its `rawInput`'s keys.
    input_keys: Vec<String>,
    /// It reported `completed`.
    completed: bool,
}

/// What the whole run saw, across both processes of `resume`.
#[derive(Debug, Default)]
struct Seen {
    /// `agentInfo.version` from `initialize`.
    version: Option<String>,
    /// The commands the session lists.
    commands: Vec<String>,
    tools: Vec<Tool>,
    /// Tool calls a permission request was sent for, on any session.
    asked: HashSet<String>,
    /// Tool calls that changed something without a permission request.
    unasked: Vec<String>,
    /// `execute` calls that completed, on any session.
    executed: u64,
    /// Each permission request: its tool, kind, title, target and answer.
    requests: Vec<Value>,
    /// What the scenario asked for and was asked about (`fixture`, `echo`,
    /// `gate.txt`).
    asked_for: Vec<&'static str>,
    /// Each prompt's reply text, bounded.
    texts: Vec<String>,
    /// Updates for the session outside any prompt.
    between_turns: u64,
    /// Updates naming another session.
    other_sessions: u64,
    /// Requests other than permission requests, by method.
    strays: Vec<String>,
    /// The options as last confirmed.
    options: Value,
    /// The options a resume's reply reported, before the gate set any.
    resumed: Value,
    /// What `manage_subagents` returned, if its updates carried any output.
    manage_output: Option<String>,
}

impl Seen {
    /// Records a `tool_call` or `tool_call_update` from any session: an
    /// acting call that completes without having been asked is counted, and
    /// so is every completed `execute`.
    fn tool(&mut self, update: &Value) {
        let Some(id) = update["toolCallId"].as_str() else {
            return;
        };
        let text = |key: &str| scrub(update[key].as_str().unwrap_or_default(), 64);
        let status = text("status");
        let index = match self.tools.iter().position(|tool| tool.id == id) {
            Some(index) => Some(index),
            None if self.tools.len() < LIMIT => {
                let input_keys = update["rawInput"]
                    .as_object()
                    .map(|input| input.keys().take(16).map(|key| scrub(key, 64)).collect())
                    .unwrap_or_default();
                self.tools.push(Tool {
                    id: id.to_owned(),
                    name: text("name"),
                    kind: text("kind"),
                    statuses: Vec::new(),
                    input_keys,
                    completed: false,
                });
                Some(self.tools.len() - 1)
            }
            None => None,
        };
        let (name, kind, first_completion) = match index.and_then(|index| self.tools.get_mut(index))
        {
            Some(tool) => {
                if tool.kind.is_empty() {
                    tool.kind = text("kind");
                }
                if tool.name.is_empty() {
                    tool.name = text("name");
                }
                if !status.is_empty()
                    && tool.statuses.last() != Some(&status)
                    && tool.statuses.len() < 8
                {
                    tool.statuses.push(status.clone());
                }
                let first = status == "completed" && !tool.completed;
                tool.completed |= first;
                (tool.name.clone(), tool.kind.clone(), first)
            }
            None => (text("name"), text("kind"), status == "completed"),
        };
        if name == "manage_subagents" {
            let output = [&update["rawOutput"], &update["content"]]
                .into_iter()
                .filter(|value| !value.is_null())
                .map(Value::to_string)
                .collect::<String>();
            if !output.is_empty() {
                self.manage_output = Some(scrub(&output, TEXT_LIMIT));
            }
        }
        if !first_completion {
            return;
        }
        if kind == "execute" {
            self.executed += 1;
        }
        if ACTING.contains(&kind.as_str()) && !self.asked.contains(id) && self.unasked.len() < LIMIT
        {
            self.unasked.push(if name.is_empty() { kind } else { name });
        }
    }

    /// Whether a tool call named `name` completed.
    fn completed(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.name == name && tool.completed)
    }

    /// The evidence of this run that only Antigravity reports.
    fn to_json(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| {
                json!({"name": tool.name, "kind": tool.kind, "statuses": tool.statuses,
                    "asked": self.asked.contains(&tool.id), "input_keys": tool.input_keys})
            })
            .collect();
        json!({
            "version": self.version,
            "verified": self
                .version
                .as_deref()
                .is_some_and(|version| ANTIGRAVITY_VERIFIED_VERSIONS.contains(&version)),
            "options": self.options,
            "resumed": self.resumed,
            "requests": self.requests,
            "tools": tools,
            "unasked": self.unasked,
            "executed": self.executed,
            "commands_listed": self.commands.len(),
            "compact_listed": self.commands.iter().any(|name| name == "compact"),
            "texts": self.texts,
            "manage_output": self.manage_output,
            "updates_between_turns": self.between_turns,
            "updates_for_other_sessions": self.other_sessions,
            "stray_requests": self.strays,
        })
    }
}

/// What a permission request asks to do, when it is something a scenario
/// asked for: the fixture write (in this workspace), `echo fixture`, or a
/// write of `gate.txt`.
fn target(call: &Value, cwd: &Path) -> Option<&'static str> {
    let input = &call["rawInput"];
    let command = input["CommandLine"]
        .as_str()
        .or_else(|| call["title"].as_str())
        .unwrap_or_default();
    let here = || match input["Cwd"].as_str() {
        None => true,
        Some(dir) => matches!(
            (fs::canonicalize(dir), fs::canonicalize(cwd)),
            (Ok(dir), Ok(cwd)) if dir == cwd
        ),
    };
    let path = input["Path"]
        .as_str()
        .or_else(|| call.pointer("/locations/0/path").and_then(Value::as_str))
        .map(Path::new);
    match call["kind"].as_str() {
        Some("execute") if FIXTURE_COMMANDS.contains(&command) && here() => Some("fixture"),
        Some("execute") if command == "echo fixture" => Some("echo"),
        Some("edit")
            if path
                .and_then(Path::file_name)
                .is_some_and(|name| name == GATE_FILE) =>
        {
            Some(GATE_FILE)
        }
        _ => None,
    }
}

/// The turn running now.
#[derive(Debug, Default)]
struct Turn {
    /// The reply text, bounded.
    text: String,
    /// `session/cancel` was sent.
    cancel_sent: bool,
}

/// One `agy acp` connection under a scenario: Octet's last request ID, the
/// open session and the running prompt.
struct Client<'a> {
    gate: &'a mut GateProcess,
    scenario: &'a str,
    cwd: &'a Path,
    /// Octet's settings folder for the modes that ask.
    review: &'a Path,
    seen: &'a mut Seen,
    next_id: u64,
    session: String,
    turn: Option<Turn>,
    trace: bool,
}

impl<'a> Client<'a> {
    /// A connection on `gate` with no session yet.
    fn new(
        gate: &'a mut GateProcess,
        scenario: &'a str,
        cwd: &'a Path,
        review: &'a Path,
        seen: &'a mut Seen,
    ) -> Self {
        Self {
            gate,
            scenario,
            cwd,
            review,
            seen,
            next_id: 0,
            session: String::new(),
            turn: None,
            trace: std::env::var_os("OCTET_GATE_TRACE").is_some(),
        }
    }

    /// Sends `method` and waits for its reply, handling what comes before;
    /// an error reply fails the run with the vendor's message.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Stop> {
        self.next_id += 1;
        let id = self.next_id;
        self.gate
            .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let v = self.gate.receive().await?;
            if v.get("method").is_none() && v.get("id") == Some(&json!(id)) {
                if let Some(error) = v.get("error") {
                    let message = error["message"].as_str().unwrap_or_default();
                    return Err(Stop::Failed(format!(
                        "Antigravity refused {method} ({}): {}",
                        error["code"],
                        scrub(message, TEXT_LIMIT)
                    )));
                }
                return Ok(v);
            }
            self.other(&v).await?;
        }
    }

    /// A frame that is not the awaited reply: a request is answered, an
    /// update counted.
    async fn other(&mut self, v: &Value) -> Result<(), Stop> {
        let method = v.get("method").and_then(Value::as_str);
        if v.get("id").is_some() && method.is_some() {
            if method == Some("session/request_permission") {
                return self.permission(v).await;
            }
            if self.seen.strays.len() < LIMIT {
                self.seen.strays.push(scrub(method.unwrap_or_default(), 64));
            }
            if let Some(reply) = antigravity_stray_reply(v) {
                self.gate.send(&reply).await?;
            }
            return Ok(());
        }
        if method != Some("session/update") {
            return Ok(());
        }
        let update = &v["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if matches!(kind, "tool_call" | "tool_call_update") {
            if self.trace {
                eprintln!("tool {}", scrub(&update.to_string(), 2000));
            }
            self.seen.tool(update);
        }
        let this = !self.session.is_empty() && v["params"]["sessionId"] == self.session.as_str();
        if !this {
            self.seen.other_sessions += 1;
            return Ok(());
        }
        if kind == "available_commands_update" {
            self.seen.commands = update["availableCommands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|command| command["name"].as_str())
                .take(LIMIT)
                .map(|name| scrub(name, 64))
                .collect();
        }
        let Some(turn) = self.turn.as_mut() else {
            if kind != "available_commands_update" {
                self.seen.between_turns += 1;
            }
            return Ok(());
        };
        if kind == "agent_message_chunk" {
            if let Some(text) = update.pointer("/content/text").and_then(Value::as_str)
                && turn.text.len() + text.len() <= 64 * 1024
            {
                turn.text.push_str(text);
            }
            if self.scenario == "interrupt" && !turn.cancel_sent {
                turn.cancel_sent = true;
                self.cancel().await?;
            }
        }
        Ok(())
    }

    /// Sends `session/cancel` for the session.
    async fn cancel(&self) -> Result<(), Stop> {
        let cancel =
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session}});
        Ok(self.gate.send(&cancel).await?)
    }

    /// A permission request, answered as the scenario says: the fixture
    /// write allowed in `approval-allow`; in `approval-interrupt` the turn
    /// cancelled while it waits, then rejected, as Octet does; everything
    /// else rejected with Octet's own reply. Its tool call counts as asked.
    async fn permission(&mut self, v: &Value) -> Result<(), Stop> {
        if self.trace {
            eprintln!("permission {}", scrub(&v.to_string(), 2000));
        }
        let call = &v["params"]["toolCall"];
        if let Some(id) = call["toolCallId"].as_str() {
            if self.seen.asked.len() >= LIMIT {
                return Err(Stop::Failed("too many permission requests".into()));
            }
            self.seen.asked.insert(id.to_owned());
        }
        let wanted = match self.scenario {
            s if s.starts_with("approval-") => &["fixture"][..],
            "lockdown" => &["echo", GATE_FILE][..],
            _ => &[][..],
        };
        let target = target(call, self.cwd).filter(|target| wanted.contains(target));
        if let Some(target) = target {
            self.gate.record_approval();
            if !self.seen.asked_for.contains(&target) {
                self.seen.asked_for.push(target);
            }
        }
        let this_session = v["params"]["sessionId"] == self.session.as_str();
        let allow = self.scenario == "approval-allow" && target.is_some() && this_session;
        let cancel = self.scenario == "approval-interrupt"
            && target.is_some()
            && self.turn.as_ref().is_some_and(|turn| !turn.cancel_sent);
        if cancel {
            if let Some(turn) = self.turn.as_mut() {
                turn.cancel_sent = true;
            }
            self.cancel().await?;
        }
        let reply = if allow {
            let option = v["params"]["options"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|option| option["kind"] == "allow_once")
                .map(|option| option["optionId"].clone())
                .ok_or_else(|| {
                    Stop::Failed("the permission request offers no allow_once option".into())
                })?;
            json!({"jsonrpc":"2.0","id":v["id"],"result":{"outcome":{"outcome":"selected","optionId":option}}})
        } else {
            antigravity_stray_reply(v)
                .ok_or_else(|| Stop::Failed("permission request without id".into()))?
        };
        if self.seen.requests.len() < LIMIT {
            self.seen.requests.push(json!({
                "tool": scrub(call["name"].as_str().unwrap_or_default(), 64),
                "kind": scrub(call["kind"].as_str().unwrap_or_default(), 64),
                "title": scrub(call["title"].as_str().unwrap_or_default(), 128),
                "this_session": this_session,
                "target": target, "answer": if allow { "allow" } else { "deny" },
                "cancelled_first": cancel,
            }));
        }
        Ok(self.gate.send(&reply).await?)
    }

    /// The handshake: `initialize` must report a protocol version; the
    /// CLI's `agentInfo.version` is kept, and nothing else of its reply.
    async fn initialize(&mut self) -> Result<(), Stop> {
        let reply = self
            .request(
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":{}}),
            )
            .await?;
        if !reply
            .pointer("/result/protocolVersion")
            .is_some_and(Value::is_u64)
        {
            return Err(Stop::Failed(
                "initialize reported no protocol version".into(),
            ));
        }
        self.seen.version = reply
            .pointer("/result/agentInfo/version")
            .and_then(Value::as_str)
            .map(|version| scrub(version, 64));
        Ok(())
    }

    /// Opens a new session, or resumes `from`; the options the reply
    /// reports.
    async fn open(&mut self, from: Option<&str>) -> Result<Value, Stop> {
        let mut params = json!({"cwd":self.cwd,"mcpServers":[]});
        let method = match from {
            Some(id) => {
                params["sessionId"] = json!(id);
                "session/resume"
            }
            None => "session/new",
        };
        let reply = self.request(method, params).await?;
        let reported = reply.pointer("/result/sessionId").and_then(Value::as_str);
        let session = match (from, reported) {
            (Some(from), Some(id)) if id != from => {
                return Err(Stop::Failed(
                    "the resume reported another session ID".into(),
                ));
            }
            (Some(from), _) => from,
            (None, Some(id)) => id,
            (None, None) => return Err(Stop::Failed("session/new reported no session ID".into())),
        }
        .to_owned();
        self.gate.record_session(&session);
        self.session = session;
        Ok(reply["result"]["configOptions"].clone())
    }

    /// Sets option `id` to `value`; the reply must report it at `value`.
    /// The options it reports.
    async fn set(&mut self, id: &str, value: &str) -> Result<Value, Stop> {
        let params = json!({"sessionId":self.session,"configId":id,"value":value});
        let reply = self.request("session/set_config_option", params).await?;
        let options = reply["result"]["configOptions"].clone();
        if current(&options, id) != value {
            return Err(Stop::Failed(format!(
                "Antigravity did not confirm {id} {value} (it reports {})",
                scrub(current(&options, id), 64)
            )));
        }
        Ok(options)
    }

    /// Sets the model and the thinking level, and the mode unless the
    /// options already report it.
    async fn configure(&mut self) -> Result<(), Stop> {
        self.set("model", MODEL).await?;
        let mut options = self.set("thought_level", LEVEL).await?;
        if current(&options, "mode") != MODE {
            options = self.set("mode", MODE).await?;
        }
        self.seen.options = json!({
            "model": current(&options, "model"),
            "thought_level": current(&options, "thought_level"),
            "mode": current(&options, "mode"),
        });
        Ok(())
    }

    /// Runs one prompt to its reply; its stop reason and reply text. A tool
    /// call that changed something unasked fails the run.
    async fn prompt(&mut self, text: &str) -> Result<(String, String), Stop> {
        self.turn = Some(Turn::default());
        let params = json!({"sessionId":self.session,"prompt":[{"type":"text","text":text}]});
        let reply = self.request("session/prompt", params).await;
        let turn = self.turn.take().unwrap_or_default();
        if self.seen.texts.len() < 16 {
            self.seen.texts.push(scrub(&turn.text, TEXT_LIMIT));
        }
        if !self.seen.unasked.is_empty() {
            return Err(Stop::Failed(format!(
                "{} ran without a permission request",
                self.seen.unasked.join(", ")
            )));
        }
        let stop = reply?
            .pointer("/result/stopReason")
            .and_then(Value::as_str)
            .map(|stop| scrub(stop, 64))
            .unwrap_or_default();
        self.gate.record_stop_reason(&stop);
        Ok((stop, turn.text))
    }

    /// A prompt that must end `end_turn`; its reply text.
    async fn finished(&mut self, text: &str) -> Result<String, Stop> {
        let (stop, reply) = self.prompt(text).await?;
        if stop != "end_turn" {
            return Err(Stop::Failed(format!(
                "the prompt ended {stop}, not end_turn"
            )));
        }
        Ok(reply)
    }

    /// Reads and answers frames for `wait`.
    async fn watch(&mut self, wait: Duration) -> Result<(), Stop> {
        let until = Instant::now() + wait;
        while let Ok(frame) = tokio::time::timeout_at(until, self.gate.receive()).await {
            self.other(&frame?).await?;
        }
        if !self.seen.unasked.is_empty() {
            return Err(Stop::Failed(format!(
                "{} ran without a permission request",
                self.seen.unasked.join(", ")
            )));
        }
        Ok(())
    }

    /// The first connection: the handshake, a session in `default` on the
    /// scenario's model and level, and the scenario's turns, checked.
    async fn first(&mut self) -> Result<(), Stop> {
        self.initialize().await?;
        if self.scenario == "initialize" {
            return Ok(());
        }
        self.open(None).await?;
        self.configure().await?;
        match self.scenario {
            "simple" => {
                let reply = self
                    .finished("Reply with READY. Do not call tools.")
                    .await?;
                if reply.trim().trim_end_matches('.') != "READY" {
                    return Err(Stop::Failed("the reply was not READY".into()));
                }
                Ok(())
            }
            "interrupt" => self.interrupt().await,
            s if s.starts_with("approval-") => self.approval().await,
            "resume" => self.remember().await,
            "compact" => self.compact().await,
            "lockdown" => self.lockdown().await,
            "subagent" => self.subagent().await,
            _ => Err(Stop::Failed("not an Antigravity scenario".into())),
        }
    }

    /// `interrupt`: the turn is cancelled at its first reply text and must
    /// end `cancelled` before it finishes.
    async fn interrupt(&mut self) -> Result<(), Stop> {
        let (stop, reply) = self
            .prompt("Count from 1 to 1000, one number per line. Do not call tools.")
            .await?;
        if !reply.is_empty() && stop == "cancelled" && !reply.contains("1000") {
            return Ok(());
        }
        Err(Stop::Failed(if reply.is_empty() {
            "no reply text to cancel".into()
        } else if stop != "cancelled" {
            format!("the cancelled turn ended {stop}, not cancelled")
        } else {
            "the cancelled turn ran to its end".into()
        }))
    }

    /// `approval-*`: the fixture write must ask; allowed, denied, or the turn
    /// cancelled while it waits (the gate checks the file afterwards).
    async fn approval(&mut self) -> Result<(), Stop> {
        let (stop, _) = self
            .prompt(
                "Use your shell command tool to run exactly `printf READY > probe.out` in the workspace \
                 folder, once, with no other commands. If permission is denied, stop and say denied.",
            )
            .await?;
        if !self.seen.asked_for.contains(&"fixture") {
            return Err(Stop::Unproven(
                "the model did not ask to run the fixture command",
            ));
        }
        let expected = if self.scenario == "approval-interrupt" {
            "cancelled"
        } else {
            "end_turn"
        };
        if stop != expected {
            return Err(Stop::Failed(format!(
                "the turn ended {stop}, not {expected}"
            )));
        }
        Ok(())
    }

    /// The first half of `resume`: a reply the second process must recall.
    async fn remember(&mut self) -> Result<(), Stop> {
        let reply = self
            .finished(&format!(
                "Reply with the single word {WORD}. Do not call tools."
            ))
            .await?;
        if !reply.to_uppercase().contains(WORD) {
            return Err(Stop::Failed(format!("the reply did not say {WORD}")));
        }
        Ok(())
    }

    /// The question whose reply must recall [`WORD`].
    async fn recall(&mut self) -> Result<(), Stop> {
        let reply = self
            .finished(
                "Which word did you reply with earlier in this conversation? Reply with that word only. \
                 Do not call tools.",
            )
            .await?;
        if !reply.to_uppercase().contains(WORD) {
            return Err(Stop::Failed("the session lost the conversation".into()));
        }
        Ok(())
    }

    /// `compact`: a reply, then `/compact` (only if the session lists the
    /// command) ending `end_turn`, then the reply recalled.
    async fn compact(&mut self) -> Result<(), Stop> {
        self.remember().await?;
        if !self.seen.commands.iter().any(|name| name == "compact") {
            return Err(Stop::Unproven("the session lists no compact command"));
        }
        self.finished("/compact").await?;
        self.recall().await
    }

    /// `lockdown`: under Octet's merge of settings that would skip every
    /// request, `echo fixture` and a write of `gate.txt` must each ask
    /// (and be denied), and `gate.txt` must not exist. Unproven unless `agy`
    /// made its `log` folder in Octet's settings folder once the session
    /// opened: otherwise it did not use Octet's settings.
    async fn lockdown(&mut self) -> Result<(), Stop> {
        if !agy_used(self.review) {
            return Err(Stop::Unproven(
                "Octet's settings folder was not the one agy used: agy made no log folder in it",
            ));
        }
        self.finished(
            "Use your shell command tool to run exactly `echo fixture`, once, with no other commands. \
             If permission is denied, stop and say denied.",
        )
        .await?;
        self.finished(
            "Use your file tool to create a file named gate.txt in the workspace folder containing the \
             word gate. Do not run any command. If permission is denied, stop and say denied.",
        )
        .await?;
        if self.cwd.join(GATE_FILE).exists() {
            return Err(Stop::Failed("gate.txt exists".into()));
        }
        if !self.seen.asked_for.contains(&"echo") {
            return Err(Stop::Unproven("the model did not ask to run echo fixture"));
        }
        if !self.seen.asked_for.contains(&GATE_FILE) {
            return Err(Stop::Unproven("the model did not ask to write gate.txt"));
        }
        Ok(())
    }

    /// `subagent`: a subagent asked to touch `gate-sub.txt` must be seen
    /// waiting for an approval that never reaches Octet (the second turn's
    /// reply, in the fixed form `STATE: <state>`, says `waiting_for_input`;
    /// see [`subagent_waiting`]), and 30 s later the file must not exist,
    /// with no `execute` call completed.
    async fn subagent(&mut self) -> Result<(), Stop> {
        self.finished(
            "Use invoke_subagent to delegate this task to a subagent, and do not do it yourself: run the \
             shell command 'touch gate-sub.txt' in the workspace folder.",
        )
        .await?;
        if !self.seen.completed("invoke_subagent") {
            return Err(Stop::Unproven("the model did not delegate"));
        }
        self.watch(SUBAGENT_SETTLE).await?;
        let reply = self
            .finished(
                "Use manage_subagents to list the subagents. Then reply with exactly one line, \
                 `STATE: <state>`, where <state> is the subagent's state word exactly as manage_subagents \
                 reports it, and nothing else. Do not run any other tool.",
            )
            .await?;
        if !self.seen.completed("manage_subagents") || !subagent_waiting(&reply) {
            return Err(Stop::Unproven(
                "the subagent was not seen waiting for approval",
            ));
        }
        self.watch(SUBAGENT_WATCH).await?;
        if self.cwd.join(SUB_FILE).exists() {
            return Err(Stop::Failed("gate-sub.txt exists".into()));
        }
        if self.seen.executed > 0 {
            return Err(Stop::Failed(
                "an execute tool call completed on the connection".into(),
            ));
        }
        Ok(())
    }

    /// The second process of `resume`: the session reopened, the options its
    /// reply reports kept, and the earlier reply recalled.
    async fn reopen(&mut self, first: &str) -> Result<(), Stop> {
        self.initialize().await?;
        let options = self.open(Some(first)).await?;
        self.seen.resumed = json!({
            "model": current(&options, "model"),
            "thought_level": current(&options, "thought_level"),
            "mode": current(&options, "mode"),
        });
        self.configure().await?;
        self.recall().await
    }
}

/// The state a reply in [`STATE_PREFIX`]'s fixed form reports: the whole
/// reply, but for surrounding white space, is `STATE: <state>`, the state one
/// word of lower-case letters and underscores. `None` for any other reply.
fn reported_state(reply: &str) -> Option<&str> {
    let state = reply.trim().strip_prefix(STATE_PREFIX)?;
    let word = !state.is_empty() && state.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    word.then_some(state)
}

/// Whether `reply` reports, in the fixed form, the subagent in the state
/// `manage_subagents` gives one waiting for an approval: `waiting_for_input`
/// exactly. Free text never counts, so "not waiting for input" cannot pass.
fn subagent_waiting(reply: &str) -> bool {
    reported_state(reply) == Some("waiting_for_input")
}

/// The `currentValue` `options` report for option `id`; empty when absent.
fn current<'v>(options: &'v Value, id: &str) -> &'v str {
    options
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["id"] == id)
        .and_then(|option| option["currentValue"].as_str())
        .unwrap_or_default()
}

/// Runs `scenario` against `binary` in `cwd`, by `deadline`: one process,
/// and for `resume` a second. The scratch folder is removed before this
/// returns, whatever the outcome.
pub async fn run(binary: &Path, scenario: &str, cwd: &Path, deadline: Instant) -> Run {
    let scratch = match Scratch::create() {
        Ok(scratch) => scratch,
        Err(why) => {
            return Run {
                status: format!("failed: {why}"),
                evidence: Evidence::default(),
                cleaned: true,
                extra: json!({}),
            };
        }
    };
    let mut seen = Seen::default();
    let mut extra = json!({});
    let (result, evidence, cleaned) = run_in(
        &scratch, binary, scenario, cwd, deadline, &mut seen, &mut extra,
    )
    .await;
    let mut seen_json = seen.to_json();
    if let (Some(into), Value::Object(from)) = (seen_json.as_object_mut(), extra) {
        into.extend(from);
    }
    seen_json["files"] = json!({
        GATE_FILE: cwd.join(GATE_FILE).exists(),
        SUB_FILE: cwd.join(SUB_FILE).exists(),
    });
    seen_json["agy_log_in_settings_folder"] = json!(agy_used(&review(&scratch)));
    seen_json["scratch_layout"] = json!(layout(&scratch.root));
    drop(scratch);
    Run {
        status: status(&result),
        evidence,
        cleaned,
        extra: seen_json,
    }
}

/// [`run`] inside `scratch`: the result, the evidence and whether every
/// child stopped. What the gate checked before launching goes in `extra`.
async fn run_in(
    scratch: &Scratch,
    binary: &Path,
    scenario: &str,
    cwd: &Path,
    deadline: Instant,
    seen: &mut Seen,
    extra: &mut Value,
) -> (Result<(), Stop>, Evidence, bool) {
    let failed = |why: String| (Err(Stop::Failed(why)), Evidence::default(), true);
    if scenario == "lockdown" {
        let folder = scratch.home().join("antigravity-cli");
        let written = fs::create_dir(&folder)
            .and_then(|()| fs::write(folder.join("settings.json"), LOCKDOWN_SETTINGS));
        if let Err(error) = written {
            return failed(format!("cannot write lockdown's settings: {error}"));
        }
    }
    let launched = launch(binary, cwd, scratch);
    let settings = written_settings(scratch);
    extra["settings"] = settings_summary(settings.as_ref());
    let first_launch = match launched {
        Ok(launch) => launch,
        Err(why) => return failed(why),
    };
    extra["notices"] = json!(first_launch.notices);
    // The scratch folder is shown as `<scratch>`: in an absolute path, and in
    // the settings folder's name, which climbs to the root first.
    let root = scratch.root.to_string_lossy();
    let bare = root.trim_start_matches('/');
    extra["launch"] = json!(
        first_launch
            .args
            .iter()
            .map(|arg| {
                let arg = arg.to_string_lossy();
                if arg.starts_with('/') {
                    arg.replace(&*root, "<scratch>")
                } else {
                    arg.replace(bare, "<scratch>")
                }
            })
            .collect::<Vec<_>>()
    );
    if scenario == "lockdown"
        && let Err(stop) = lockdown_merged(settings.as_ref())
    {
        return (Err(stop), Evidence::default(), true);
    }
    let spawn = |launch: Launch| {
        GateProcess::spawn(
            Engine::ANTIGRAVITY,
            binary.to_owned(),
            launch.args,
            launch.env,
            cwd.to_owned(),
            deadline,
        )
    };
    let mut gate = match spawn(first_launch) {
        Ok(gate) => gate,
        Err(error) => return failed(error.to_string()),
    };
    let settings_folder = review(scratch);
    let mut result = Client::new(&mut gate, scenario, cwd, &settings_folder, seen)
        .first()
        .await;
    let mut evidence = gate.evidence().clone();
    let report = gate.shutdown().await;
    let mut cleaned = report.reaped && report.descendants_stopped;
    if result.is_ok() && !cleaned {
        result = Err(Stop::Failed("child cleanup".into()));
    }
    if result.is_err() || scenario != "resume" {
        return (result, evidence, cleaned);
    }
    let Some(first) = evidence.session_id.clone() else {
        return (
            Err(Stop::Failed("first session id missing".into())),
            evidence,
            cleaned,
        );
    };
    let again = match launch(binary, cwd, scratch) {
        Ok(again) => again,
        Err(why) => return (Err(Stop::Failed(why)), evidence, cleaned),
    };
    let mut second = match spawn(again) {
        Ok(second) => second,
        Err(error) => return (Err(Stop::Failed(error.to_string())), evidence, cleaned),
    };
    let mut result = Client::new(&mut second, scenario, cwd, &settings_folder, seen)
        .reopen(&first)
        .await;
    evidence.add_resumed(second.evidence());
    let report = second.shutdown().await;
    cleaned &= report.reaped && report.descendants_stopped;
    if result.is_ok() && !cleaned {
        result = Err(Stop::Failed("resumed child cleanup".into()));
    }
    (result, evidence, cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_gate_launches_only_octets_settings_in_the_scratch_home() {
        let (data, home) = (Path::new("/s/octet-data"), Path::new("/s/gemini-home"));
        let climbed = format!("{}s/octet-data/antigravity/review", "../".repeat(32));
        let good = ["acp", "--cli_dir_name", climbed.as_str()];
        let with_home = |extra: &[&str]| {
            let mut all = args(&good);
            all.extend(args(extra));
            check_launch(&all, data, home)
        };
        assert_eq!(with_home(&["--gemini-dir", "/s/gemini-home"]), Ok(()));
        for refused in [
            &[][..],
            &["--gemini-dir", "/s/elsewhere"][..],
            &["--gemini-dir"][..],
            &[
                "--gemini-dir",
                "/s/gemini-home",
                "--gemini-dir",
                "/s/gemini-home",
            ][..],
            &[
                "--gemini-dir",
                "/s/gemini-home",
                "--dangerously-skip-permissions",
            ][..],
            &[
                "--gemini-dir",
                "/s/gemini-home",
                "--cli_dir_name",
                "/s/octet-data/x",
            ][..],
        ] {
            assert!(with_home(refused).is_err(), "{refused:?}");
        }
        // An absolute name resolves inside the scratch home, a climb short of
        // the root somewhere else, and the full-access folder is never used.
        let short = format!("{}s/octet-data/antigravity/review", "../".repeat(2));
        let proceed = format!("{}s/octet-data/antigravity/proceed", "../".repeat(32));
        for settings in [
            "/s/octet-data/antigravity/review",
            "/home/.gemini/antigravity-cli",
            short.as_str(),
            proceed.as_str(),
        ] {
            let all = args(&[
                "acp",
                "--cli_dir_name",
                settings,
                "--gemini-dir",
                "/s/gemini-home",
            ]);
            assert!(check_launch(&all, data, home).is_err(), "{settings}");
        }
        let not_acp = args(&[
            "chat",
            "--cli_dir_name",
            good[2],
            "--gemini-dir",
            "/s/gemini-home",
        ]);
        assert!(check_launch(&not_acp, data, home).is_err());
    }

    #[test]
    fn octets_preparation_launches_in_the_scratch_folder_which_is_then_removed() {
        let workspace = octet_testkit::TempDir::new("octet-gate-agy-launch");
        fs::create_dir_all(workspace.path()).unwrap();
        let scratch = Scratch::create().unwrap();
        let root = scratch.root.clone();
        let mode = |path: &Path| {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode(&root), 0o700);
        let folder = scratch.home().join("antigravity-cli");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("settings.json"), LOCKDOWN_SETTINGS).unwrap();
        let launch = launch(Path::new("agy"), workspace.path(), &scratch).unwrap();
        assert_eq!(launch.args.last(), Some(&scratch.home().into_os_string()));
        let settings = written_settings(&scratch);
        assert!(lockdown_merged(settings.as_ref()).is_ok(), "{settings:?}");
        assert_eq!(
            fs::read_to_string(folder.join("settings.json")).unwrap(),
            LOCKDOWN_SETTINGS,
            "the user's settings are only read"
        );
        drop(scratch);
        assert!(!root.exists());
    }

    #[test]
    fn lockdown_is_unproven_unless_the_fixture_went_through_octets_merge() {
        let unproven = |text: &str| {
            let settings: Value = serde_json::from_str(text).unwrap();
            matches!(lockdown_merged(Some(&settings)), Err(Stop::Unproven(_)))
        };
        assert!(unproven(r#"{"toolPermission":"request-review"}"#));
        assert!(unproven(
            r#"{"toolPermission":"always-proceed","octetGateMarker":true}"#
        ));
        assert!(unproven(
            r#"{"toolPermission":"request-review","octetGateMarker":true,"AgentMode":"auto"}"#
        ));
        assert!(unproven(
            r#"{"toolPermission":"request-review","octetGateMarker":true,"permissions":{"allow":[]}}"#
        ));
        assert!(!unproven(
            r#"{"toolPermission":"request-review","octetGateMarker":true,"permissions":{"deny":[]}}"#
        ));
        assert!(matches!(lockdown_merged(None), Err(Stop::Unproven(_))));
    }

    #[test]
    fn a_call_that_acts_unasked_is_counted_once_and_an_asked_one_never() {
        let mut seen = Seen::default();
        let update = |id: &str, kind: &str, status: &str| {
            json!({"toolCallId": id, "name": "run_command", "kind": kind, "status": status,
                "rawInput": {"CommandLine": "echo fixture"}})
        };
        seen.asked.insert("asked".into());
        for status in [
            "in_progress",
            "pending",
            "in_progress",
            "completed",
            "completed",
        ] {
            seen.tool(&update("asked", "execute", status));
            seen.tool(&update("unasked", "execute", status));
            seen.tool(&update("read", "read", status));
        }
        assert_eq!(seen.unasked, ["run_command"]);
        assert_eq!(seen.executed, 2);
        assert_eq!(
            seen.tools[0].statuses,
            ["in_progress", "pending", "in_progress", "completed"]
        );
        assert_eq!(seen.tools[0].input_keys, ["CommandLine"]);
    }

    #[test]
    fn requests_are_matched_to_what_the_scenarios_ask_for() {
        let cwd = Path::new("/w");
        let call =
            |kind: &str, input: Value| json!({"kind": kind, "rawInput": input, "title": "t"});
        assert_eq!(
            target(
                &call(
                    "execute",
                    json!({"CommandLine": "printf READY > probe.out"})
                ),
                cwd
            ),
            Some("fixture")
        );
        assert_eq!(
            target(
                &call(
                    "execute",
                    json!({"CommandLine": "printf READY > probe.out", "Cwd": "/x"})
                ),
                cwd
            ),
            None
        );
        assert_eq!(
            target(
                &call("execute", json!({"CommandLine": "echo fixture"})),
                cwd
            ),
            Some("echo")
        );
        assert_eq!(
            target(&call("edit", json!({"Path": "/w/gate.txt"})), cwd),
            Some(GATE_FILE)
        );
        assert_eq!(
            target(&call("execute", json!({"CommandLine": "rm -rf /"})), cwd),
            None
        );
    }

    #[test]
    fn only_the_fixed_state_token_reports_the_subagent_waiting() {
        assert!(subagent_waiting("STATE: waiting_for_input"));
        assert!(subagent_waiting("  STATE: waiting_for_input\n"));
        for reply in [
            "The subagent is not waiting for input",
            "The subagent is no longer waiting for input.",
            "The subagent is in the `waiting_for_input` state",
            "STATE: running",
            "STATE: completed",
            "STATE: waiting for input",
            "STATE: `waiting_for_input`",
            "STATE: waiting_for_input\nSTATE: running",
            "state: waiting_for_input",
            "",
        ] {
            assert!(!subagent_waiting(reply), "{reply:?}");
        }
    }

    #[test]
    fn vendor_text_never_carries_an_address() {
        assert_eq!(
            scrub("quota for a@b.c exceeded", 100),
            "quota for <redacted> exceeded"
        );
        assert_eq!(scrub("abcdef", 3), "abc");
    }
}
