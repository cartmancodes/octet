//! Scripted turns for fleet runs. When `OCTET_FLEET_SCRIPT` names a JSON
//! script and a prompt's first line is a fleet header, the Codex and Claude
//! fakes perform the turn that script gives for that header, instead of
//! choosing a scenario by the prompt's text. Anything else is not this
//! module's business: [`turn`] returns `None` and the dialect carries on.
//!
//! The script:
//!
//! ```json
//! {"log": "<path>", "startup_marker": "<path>",
//!  "turns": {"<step>#<round>": {"write": {"<rel path>": "<content>"},
//!                               "delete": ["<rel path>"], "reply": "<text>",
//!                               "hold": "before_write" | "after_write",
//!                               "spawn_survivor": true, "exit_after": true}}}
//! ```
//!
//! Every turn field is optional, and so is `startup_marker`. A turn is found
//! by its key, `<step>#<round>`; a key seen again plays the same turn.
//!
//! Hold modes, for a turn that must be stopped by the run's own interrupt:
//! - `before_write`: the turn is logged and then held. Nothing else happens:
//!   no file is written or deleted and no survivor started, so a test sees a
//!   turn that was cut off before it did anything.
//! - `after_write`: the writes, deletes and survivor happen, then the turn is
//!   held, with no reply: a turn cut off after it changed the tree.
//!
//! A held turn ends only when the dialect's interrupt handling ends it, and
//! `exit_after` does not apply to it.
use octet_testkit::scenario::FLEET_SCRIPT_ENV;
use serde_json::{Map, Value, json};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path},
    process::{self, Command, Stdio},
};

/// How a fleet prompt's first line starts. The source of truth is
/// `HEADER_PREFIX` in `octet-fleet`'s `contract.rs`, whose
/// `[octet-fleet step=<id> round=<n> attempt=<a> candidate=<c> tree=<t>]`
/// this parses; the testkit cannot depend on that crate, so a change there
/// must be made here too.
const HEADER_PREFIX: &str = "[octet-fleet ";

/// What the dialect does for a scripted turn, after [`turn`] has done the
/// rest.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FleetTurn {
    /// The reply text; the dialect sends it as the assistant's message and
    /// completes the turn.
    pub reply: String,
    /// Hold the turn open, with no reply and no completion, until the
    /// dialect's interrupt ends it.
    pub hold: bool,
    /// Exit the process once the reply and the turn's completion are sent.
    pub exit_after: bool,
}

/// The script `OCTET_FLEET_SCRIPT` names, if it is set.
fn script() -> Option<Value> {
    let path = env::var_os(FLEET_SCRIPT_ENV)?;
    let text = fs::read_to_string(&path).unwrap();
    Some(serde_json::from_str(&text).unwrap())
}

/// Appends `text` to the file `path`, in one write, creating it.
fn append(path: &Value, text: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.as_str().unwrap())
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.flush().unwrap();
}

/// Records this process's start in the script's `startup_marker`, if there
/// is a script and it names one: a line with the process ID. Called before
/// any dialect starts, so before it reads its input.
pub(crate) fn record_start() {
    if let Some(script) = script()
        && script["startup_marker"].is_string()
    {
        append(&script["startup_marker"], &format!("{}\n", process::id()));
    }
}

/// The script key of a prompt whose first line is a fleet header:
/// `<step>#<round>`.
fn key(prompt: &str) -> Option<String> {
    let line = prompt.lines().next()?;
    let fields = line.strip_prefix(HEADER_PREFIX)?.strip_suffix(']')?;
    let field = |name: &str| {
        fields
            .split(' ')
            .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
    };
    Some(format!("{}#{}", field("step")?, field("round")?))
}

/// `relative` under `cwd`; a path that leaves it is a bug in the script.
fn inside(cwd: &Path, relative: &str) -> std::path::PathBuf {
    let path = Path::new(relative);
    assert!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "fleet script path {relative:?} must stay inside the working directory"
    );
    cwd.join(path)
}

/// The files a turn changes, relative to `cwd`: its writes, then its deletes.
fn change_files(turn: &Value, cwd: &Path) {
    let none = Map::new();
    for (relative, content) in turn["write"].as_object().unwrap_or(&none) {
        let path = inside(cwd, relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content.as_str().unwrap()).unwrap();
    }
    for relative in turn["delete"].as_array().into_iter().flatten() {
        match fs::remove_file(inside(cwd, relative.as_str().unwrap())) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => panic!("{error}"),
            _ => {}
        }
    }
}

/// Starts a process that outlives the turn: `sh -c 'sleep 60'` with this
/// one's environment and no stdio, never waited for.
fn spawn_survivor() {
    #[expect(
        clippy::zombie_processes,
        reason = "the survivor outlives the turn, and is meant never to be waited for"
    )]
    let _survivor = Command::new("sh")
        .args(["-c", "sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
}

/// Performs the scripted turn for `prompt`, if there is one to perform: the
/// script is set and the prompt's first line is a fleet header. `cwd` is the
/// vendor's working directory, which the turn's files are relative to, and
/// `resumed` whether this session resumed an earlier one.
///
/// The turn is logged (one JSON line in the script's `log`: `key`, `cwd`,
/// `resumed` and the whole `prompt`), its files changed and its survivor
/// started, as the module's hold modes say. A key the script lacks is
/// logged with `"missing": true` and answered `fleet-fake: no turn <key>`.
/// What the dialect still has to do is in the [`FleetTurn`].
pub(crate) fn turn(prompt: &str, cwd: &Path, resumed: bool) -> Option<FleetTurn> {
    let script = script()?;
    let key = key(prompt)?;
    let turn = script["turns"].get(&key);
    let mut entry = json!({"key": key, "cwd": cwd, "resumed": resumed, "prompt": prompt});
    if turn.is_none() {
        entry["missing"] = json!(true);
    }
    append(&script["log"], &format!("{entry}\n"));
    let Some(turn) = turn else {
        return Some(FleetTurn {
            reply: format!("fleet-fake: no turn {key}"),
            hold: false,
            exit_after: false,
        });
    };
    let hold = turn["hold"].as_str();
    if hold == Some("before_write") {
        return Some(held());
    }
    change_files(turn, cwd);
    if turn["spawn_survivor"] == true {
        spawn_survivor();
    }
    if hold == Some("after_write") {
        return Some(held());
    }
    Some(FleetTurn {
        reply: turn["reply"].as_str().unwrap_or("").to_owned(),
        hold: false,
        exit_after: turn["exit_after"] == true,
    })
}

/// A turn that stays open until interrupted.
fn held() -> FleetTurn {
    FleetTurn {
        reply: String::new(),
        hold: true,
        exit_after: false,
    }
}
