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
//!                               "spawn_survivor": true, "unmarked_survivor": true,
//!                               "wait_for": "<path>", "exit_after": true,
//!                               "fail": true}}}
//! ```
//!
//! Every turn field is optional, and so is `startup_marker`. A turn is found
//! by its key, `<step>#<round>`; a key seen again plays the same turn. A key
//! may instead map to a list of turns: its first play performs the first,
//! its second the second, and so on, the last repeating (plays are counted
//! in the log). `fail` makes the dialect fail the turn as a usage limit
//! does, after its writes, with no reply.
//!
//! A survivor is this binary again (`fleet-survivor`), not `sh`: on macOS
//! `ps` shows no environment for Apple's binaries, so only a survivor of our
//! own carries an `OCTET_FLEET_ATTEMPT` marker Octet can see. It has no
//! stdio and no fleet script, and sleeps for [`SURVIVOR_SECS`]; its command
//! line names the script, so a test can find every survivor it caused.
//! `spawn_survivor` keeps the vendor's environment, marker included;
//! `unmarked_survivor` removes the marker. Both stay in the vendor's process
//! group.
//!
//! `wait_for`, after the changes and survivors: create `<path>.waiting`, then
//! wait until `<path>` exists (at most [`SURVIVOR_SECS`]) before the reply
//! or the hold, so a test can act at that point.
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
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::{self, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How a fleet prompt's first line starts. The source of truth is
/// `HEADER_PREFIX` in `octet-fleet`'s `contract.rs`, whose
/// `[octet-fleet step=<id> round=<n> attempt=<a> candidate=<c> tree=<t>]`
/// this parses; the testkit cannot depend on that crate, so a change there
/// must be made here too.
const HEADER_PREFIX: &str = "[octet-fleet ";

/// The variable whose value marks a fleet attempt's processes; the source of
/// truth is `octet-fleet`'s `ownership.rs`.
const MARKER_VAR: &str = "OCTET_FLEET_ATTEMPT";

/// How long a survivor lives, and the longest a `wait_for` waits, in seconds.
pub(crate) const SURVIVOR_SECS: u64 = 300;

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
    /// Fail the turn as a usage limit does, instead of replying.
    pub fail: bool,
}

/// A fleet script. Any field or value this does not know is an error: a
/// typo in a script must fail the test, never be ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    /// Where every turn is logged.
    log: PathBuf,
    /// Where each start of the vendor appends its process ID.
    #[serde(default)]
    startup_marker: Option<PathBuf>,
    /// The turns, by `<step>#<round>`.
    turns: BTreeMap<String, Turns>,
}

/// A key's turn, or its turns in the order they are played.
#[derive(Deserialize)]
#[serde(untagged)]
enum Turns {
    /// The same turn on every play.
    One(Turn),
    /// One turn per play, the last repeating.
    Many(Vec<Turn>),
}

impl Turns {
    /// The turn of play `played` (counting from 0).
    fn play(&self, played: usize) -> Option<&Turn> {
        match self {
            Self::One(turn) => Some(turn),
            Self::Many(turns) => turns.get(played).or_else(|| turns.last()),
        }
    }
}

/// One scripted turn; every field is optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    /// Files to write, relative path to content.
    #[serde(default)]
    write: BTreeMap<String, String>,
    /// Files to delete, after the writes.
    #[serde(default)]
    delete: Vec<String>,
    /// The reply text.
    #[serde(default)]
    reply: String,
    /// Where to hold the turn, if at all.
    #[serde(default)]
    hold: Option<Hold>,
    /// Start a process that outlives the turn.
    #[serde(default)]
    spawn_survivor: bool,
    /// Start a process that outlives the turn, without the attempt's marker.
    #[serde(default)]
    unmarked_survivor: bool,
    /// Wait for this file before the reply or the hold.
    #[serde(default)]
    wait_for: Option<PathBuf>,
    /// Exit after the reply.
    #[serde(default)]
    exit_after: bool,
    /// Fail the turn as a usage limit does.
    #[serde(default)]
    fail: bool,
}

/// Where a held turn stops; see the module's hold modes.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Hold {
    /// Before anything is changed.
    BeforeWrite,
    /// After the changes, before the reply.
    AfterWrite,
}

/// The script in `text`; a script that does not parse fails the test.
fn parse_script(text: &str) -> Script {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("invalid fleet script: {error}"))
}

/// The script `OCTET_FLEET_SCRIPT` names, if it is set.
fn script() -> Option<Script> {
    let path = env::var_os(FLEET_SCRIPT_ENV)?;
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("fleet script {}: {error}", path.to_string_lossy()));
    Some(parse_script(&text))
}

/// Appends `text` to the file `path`, in one write, creating it.
fn append(path: &Path, text: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.flush().unwrap();
}

/// How often `key` was played before: its lines in the turn log `log`.
fn plays(log: &Path, key: &str) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|entry| entry["key"].as_str() == Some(key))
        })
        .count()
}

/// Records this process's start in the script's `startup_marker`, if there
/// is a script and it names one: a line with the process ID. Called before
/// any dialect starts, so before it reads its input.
pub(crate) fn record_start() {
    if let Some(script) = script()
        && let Some(marker) = script.startup_marker
    {
        append(&marker, &format!("{}\n", process::id()));
    }
}

/// The script key of a prompt, `<step>#<round>`, or `None` for a prompt that
/// is not a fleet prompt: one whose first line does not start with the
/// header prefix. A first line that does but is not a header, as octet-fleet
/// writes it, is a bug in the fake or in the contract, and fails the test.
///
/// The step ID runs to the first ` round=`, and may contain anything else.
fn key(prompt: &str) -> Option<String> {
    let line = prompt.lines().next()?;
    let fields = line.strip_prefix(HEADER_PREFIX)?;
    let malformed = || -> ! { panic!("malformed fleet header: {line:?}") };
    let (step, rest) = fields
        .strip_prefix("step=")
        .and_then(|rest| rest.split_once(" round="))
        .unwrap_or_else(|| malformed());
    let round = rest.split(' ').next().unwrap_or_default();
    if step.is_empty() || round.is_empty() || !round.bytes().all(|b| b.is_ascii_digit()) {
        malformed();
    }
    Some(format!("{step}#{round}"))
}

/// `relative` under `cwd`; a path that leaves it is a bug in the script.
fn inside(cwd: &Path, relative: &str) -> PathBuf {
    let path = Path::new(relative);
    assert!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "fleet script path {relative:?} must stay inside the working directory"
    );
    cwd.join(path)
}

/// The files a turn changes, relative to `cwd`: its writes, then its deletes.
fn change_files(turn: &Turn, cwd: &Path) {
    for (relative, content) in &turn.write {
        let path = inside(cwd, relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    for relative in &turn.delete {
        match fs::remove_file(inside(cwd, relative)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => panic!("{error}"),
            _ => {}
        }
    }
}

/// Starts a process that outlives the turn: this binary as
/// `fleet-survivor SECS SCRIPT`, with this one's environment less the fleet
/// script (and, unless `marked`, less the attempt's marker), and no stdio;
/// never waited for.
fn spawn_survivor(script: &Path, marked: bool) {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .arg("fleet-survivor")
        .arg(SURVIVOR_SECS.to_string())
        .arg(script)
        .env_remove(FLEET_SCRIPT_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !marked {
        command.env_remove(MARKER_VAR);
    }
    #[expect(
        clippy::zombie_processes,
        reason = "the survivor outlives the turn, and is meant never to be waited for"
    )]
    let _survivor = command.spawn().unwrap();
}

/// Announces `<path>.waiting` and waits until `path` exists, for at most
/// [`SURVIVOR_SECS`].
fn wait_for(path: &Path) {
    let mut waiting = path.as_os_str().to_owned();
    waiting.push(".waiting");
    fs::write(&waiting, "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(SURVIVOR_SECS);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
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
    let script_path = PathBuf::from(env::var_os(FLEET_SCRIPT_ENV)?);
    let script = script()?;
    let key = key(prompt)?;
    let played = plays(&script.log, &key);
    let turn = script.turns.get(&key).and_then(|turns| turns.play(played));
    let mut entry = json!({"key": key, "cwd": cwd, "resumed": resumed, "prompt": prompt});
    if turn.is_none() {
        entry["missing"] = json!(true);
    }
    append(&script.log, &format!("{entry}\n"));
    let Some(turn) = turn else {
        return Some(FleetTurn {
            reply: format!("fleet-fake: no turn {key}"),
            hold: false,
            exit_after: false,
            fail: false,
        });
    };
    if matches!(turn.hold, Some(Hold::BeforeWrite)) {
        return Some(held());
    }
    change_files(turn, cwd);
    if turn.spawn_survivor {
        spawn_survivor(&script_path, true);
    }
    if turn.unmarked_survivor {
        spawn_survivor(&script_path, false);
    }
    if let Some(path) = &turn.wait_for {
        wait_for(path);
    }
    if matches!(turn.hold, Some(Hold::AfterWrite)) {
        return Some(held());
    }
    Some(FleetTurn {
        reply: turn.reply.clone(),
        hold: false,
        exit_after: turn.exit_after,
        fail: turn.fail,
    })
}

/// A turn that stays open until interrupted.
fn held() -> FleetTurn {
    FleetTurn {
        reply: String::new(),
        hold: true,
        exit_after: false,
        fail: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "[octet-fleet step=build round=2 attempt=1 candidate=c1 tree=t1]";

    #[test]
    fn a_header_gives_its_step_and_round() {
        assert_eq!(key(HEADER).as_deref(), Some("build#2"));
        assert_eq!(key(&format!("{HEADER}\nbody")).as_deref(), Some("build#2"));
        assert_eq!(
            key(&format!("{HEADER}\r\nbody")).as_deref(),
            Some("build#2")
        );
    }

    #[test]
    fn a_step_id_may_hold_spaces() {
        let line = "[octet-fleet step=my step round=10 attempt=1 candidate=c tree=t]";
        assert_eq!(key(line).as_deref(), Some("my step#10"));
    }

    #[test]
    fn other_prompts_are_not_fleet_prompts() {
        assert_eq!(key("hold"), None);
        assert_eq!(key(""), None);
        assert_eq!(key(&format!("text first\n{HEADER}")), None);
    }

    #[test]
    #[should_panic(expected = "malformed fleet header")]
    fn a_header_without_a_round_fails() {
        key("[octet-fleet step=a attempt=1]");
    }

    #[test]
    #[should_panic(expected = "malformed fleet header")]
    fn a_header_with_a_round_that_is_not_a_number_fails() {
        key("[octet-fleet step=a round=x attempt=1]");
    }

    #[test]
    #[should_panic(expected = "malformed fleet header")]
    fn a_header_that_does_not_start_with_the_step_fails() {
        key("[octet-fleet round=1 step=a]");
    }

    #[test]
    fn a_complete_script_parses() {
        let script = parse_script(
            r#"{"log":"l","startup_marker":"m","turns":{"a#1":{"write":{"f":"x"},
            "delete":["g"],"reply":"r","hold":"after_write","spawn_survivor":true,
            "unmarked_survivor":true,"wait_for":"w","exit_after":true},"b#1":{}}}"#,
        );
        let first = |key: &str| script.turns[key].play(0).unwrap();
        assert!(matches!(first("a#1").hold, Some(Hold::AfterWrite)));
        assert!(first("a#1").unmarked_survivor);
        assert_eq!(first("a#1").wait_for.as_deref(), Some(Path::new("w")));
        assert!(first("b#1").hold.is_none());
    }

    #[test]
    fn a_list_of_turns_plays_in_order_and_repeats_its_last() {
        let script = parse_script(r#"{"log":"l","turns":{"a#1":[{"fail":true},{"reply":"r"}]}}"#);
        let turns = &script.turns["a#1"];
        assert!(turns.play(0).unwrap().fail);
        assert_eq!(turns.play(1).unwrap().reply, "r");
        assert_eq!(turns.play(5).unwrap().reply, "r");
    }

    #[test]
    fn plays_are_counted_in_the_log() {
        let dir = std::env::temp_dir().join(format!("fleet-plays-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        fs::write(
            &log,
            "{\"key\":\"a#1\"}\n{\"key\":\"b#1\"}\n{\"key\":\"a#1\"}\n",
        )
        .unwrap();
        assert_eq!(plays(&log, "a#1"), 2);
        assert_eq!(plays(&log, "c#1"), 0);
        assert_eq!(plays(&dir.join("absent"), "a#1"), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[should_panic(expected = "invalid fleet script")]
    fn an_unknown_hold_fails() {
        parse_script(r#"{"log":"l","turns":{"a#1":{"hold":"befor_write"}}}"#);
    }

    #[test]
    #[should_panic(expected = "invalid fleet script")]
    fn an_unknown_turn_key_fails() {
        parse_script(r#"{"log":"l","turns":{"a#1":{"exit_afer":true}}}"#);
    }

    #[test]
    #[should_panic(expected = "invalid fleet script")]
    fn an_unknown_script_key_fails() {
        parse_script(r#"{"log":"l","turns":{},"startup_markr":"m"}"#);
    }

    #[test]
    #[should_panic(expected = "invalid fleet script")]
    fn a_field_of_the_wrong_type_fails() {
        parse_script(r#"{"log":"l","turns":{"a#1":{"exit_after":"true"}}}"#);
    }
}
