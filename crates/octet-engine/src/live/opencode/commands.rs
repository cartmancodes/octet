//! opencode's slash commands, checked before one is sent while the mode
//! asks. opencode 1.18.25 runs a command (a configured one, a skill, an MCP
//! prompt or a built-in) in ways no permission check sees: under the agent
//! its definition names instead of the session's, as a subagent whose
//! approval requests never reach Octet, and with every `` !`cmd` `` of its
//! expanded text run in the shell first. Octet reads each definition as
//! that opencode would (`opencode debug config` and `opencode debug skill`,
//! with the session's launch environment, in its workspace), and refuses a
//! command it cannot read or that would run any of those ways. opencode
//! 1.18.25 loads its commands once per process and never reloads them, so
//! Octet reads them once too, when opencode lists them, and not again after
//! a turn that may have changed their files unasked.
use super::{SHELL_MARK, agent, shell_refusal, subtask_refusal};
use crate::live::{Config, Mode, short};
use serde_json::Value;
use std::{
    collections::HashMap,
    ffi::OsString,
    io::{Read as _, Seek as _},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

/// The opencode versions Octet's check is verified against: their
/// built-in commands (see `built_ins`), `command()` expansion (see
/// `expand`), ACP prompt parsing (`slash_command`) and subtask rule are the
/// ones emulated here. While the mode asks, an opencode reporting any other
/// version (`agentInfo.version` in its `initialize` reply) runs no command.
///
/// To verify a new version: read its `Command.state`, `command()` and
/// `ACP.prompt` against this file; run the protocol gate's opencode
/// `command-expansion`, `lockdown` and `subagent` scenarios against it
/// (`docs/rust/protocol-gate.md`), which must all pass; then add it here.
pub const VERIFIED_VERSIONS: &[&str] = &["1.18.25"];

/// The most characters of a reported version Octet shows.
const MAX_VERSION: usize = 64;

/// The most commands one reading keeps; the rest are unknown, so refused.
const MAX_COMMANDS: usize = 4096;
/// The most bytes one `opencode debug` output may have.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// How often Octet looks at the size of `opencode debug`'s output while it
/// runs.
const OUTPUT_POLL: Duration = Duration::from_millis(50);
/// How long a changed command list must stay unchanged before Octet reads
/// it: each `opencode debug` run starts opencode, plugins included, and
/// opencode may list its commands again within moments (as MCP servers
/// connect one by one).
const LIST_SETTLE: Duration = Duration::from_millis(200);
/// The most bytes a command's expanded text may have before Octet stops
/// expanding it and refuses the command (a `$'` in its arguments repeats
/// the template).
const MAX_EXPANDED: usize = 4 * 1024 * 1024;

/// What opencode would run for one command, as far as the checks need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Definition {
    /// The agent the definition names, which replaces the session's; a
    /// value that is not text is kept as its JSON.
    pub(super) agent: Option<String>,
    /// Its `subtask` setting; a value that is not a boolean reads as `true`.
    pub(super) subtask: Option<bool>,
    /// The template its arguments go into; `None` when it is not text.
    pub(super) template: Option<String>,
    /// The description opencode lists it with.
    pub(super) description: String,
}

/// Every command Octet read, by name.
pub(super) type Book = HashMap<String, Definition>;

/// opencode 1.18.25's built-in commands, which `opencode debug config` does
/// not show: `init` (no agent, no subtask, and a template without shell)
/// and `review` (a subagent). A configured command of the same name
/// replaces one.
fn built_ins() -> Book {
    let built_in = |description: &str, subtask| Definition {
        agent: None,
        subtask,
        template: Some(String::new()),
        description: description.to_owned(),
    };
    Book::from([
        ("init".to_owned(), built_in("guided AGENTS.md setup", None)),
        (
            "review".to_owned(),
            built_in(
                "review changes [commit|branch|pr], defaults to uncommitted",
                Some(true),
            ),
        ),
    ])
}

/// The built-in commands Octet's check knows, each name and the
/// description opencode lists it with: what the protocol gate compares the
/// running opencode's list with.
pub fn built_in_descriptions() -> Vec<(String, String)> {
    let mut built_ins: Vec<(String, String)> = built_ins()
        .into_iter()
        .map(|(name, definition)| (name, definition.description))
        .collect();
    built_ins.sort();
    built_ins
}

/// The book from `opencode debug config`'s `config` and `opencode debug
/// skill`'s `skills`, merged as opencode 1.18.25 merges its commands: the
/// built-ins, replaced by configured commands of the same name; then each
/// skill no command already names. (MCP prompts, which replace configured
/// commands, are not shown, so they stay unknown.)
///
/// # Errors
///
/// Says which output is not the shape opencode prints.
pub(super) fn book_from(config: &Value, skills: &Value) -> Result<Book, String> {
    let mut book = built_ins();
    let commands = match config.get("command") {
        _ if !config.is_object() => return Err("its configuration is not a JSON object".to_owned()),
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(commands)) => commands.clone(),
        Some(_) => return Err("its command map is not a JSON object".to_owned()),
    };
    let Some(skills) = skills.as_array() else {
        return Err("its skill list is not a JSON array".to_owned());
    };
    for (name, command) in commands.into_iter().take(MAX_COMMANDS) {
        book.insert(name, configured(&command));
    }
    for skill in skills {
        if book.len() >= MAX_COMMANDS {
            break;
        }
        let Some(name) = skill["name"].as_str() else {
            continue;
        };
        if book.contains_key(name) {
            continue;
        }
        let template = match (skill["content"].as_str(), skill["location"].as_str()) {
            (Some(content), Some(location)) => Some(skill_template(content, location)),
            _ => None,
        };
        let description = skill["description"].as_str().unwrap_or_default().to_owned();
        book.insert(
            name.to_owned(),
            Definition {
                agent: None,
                subtask: None,
                template,
                description,
            },
        );
    }
    Ok(book)
}

/// A configured command's definition from its `command` entry.
fn configured(command: &Value) -> Definition {
    let agent = match &command["agent"] {
        Value::Null => None,
        Value::String(agent) => Some(agent.clone()),
        other => Some(other.to_string()),
    };
    let subtask = match &command["subtask"] {
        Value::Null => None,
        Value::Bool(subtask) => Some(*subtask),
        _ => Some(true),
    };
    Definition {
        agent,
        subtask,
        template: command["template"].as_str().map(str::to_owned),
        description: command["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// A skill's template as opencode 1.18.25 builds it: its content, then,
/// for one not built in, the folder it lives in.
fn skill_template(content: &str, location: &str) -> String {
    if location == "<built-in>" {
        return content.to_owned();
    }
    let folder = Path::new(location)
        .parent()
        .map(|folder| folder.display().to_string())
        .unwrap_or_default();
    [
        content,
        "",
        &format!("Base directory for this skill: {folder}"),
        "Relative paths in this skill (e.g., scripts/, references/) are relative to this base directory.",
    ]
    .join("\n")
}

/// Why `name` cannot be sent with `args` (as opencode reads them) under
/// `mode`, from `book`; `listed` is the description opencode lists it with,
/// when it does. `None` when it can, or `mode` lets opencode decide. A
/// command named `compact` that `book` lacks is opencode's own summary.
pub(super) fn refusal(
    book: &Book,
    listed: Option<&str>,
    name: &str,
    args: &str,
    mode: Mode,
) -> Option<String> {
    if !matches!(mode, Mode::Ask | Mode::AcceptEdits) {
        return None;
    }
    let shown = short(name);
    let label = mode.label();
    let refuse = |why: String| {
        Some(format!(
            "{why}; in {label} Octet did not send it. In auto, opencode's own permission settings decide"
        ))
    };
    let Some(definition) = book.get(name) else {
        if name == "compact" {
            return None;
        }
        return refuse(format!(
            "Octet found no definition of /{shown} among opencode's commands (an MCP prompt shows none), \
             so it cannot check what /{shown} runs"
        ));
    };
    if listed.is_some_and(|listed| listed != definition.description) {
        return refuse(format!(
            "opencode lists /{shown} with another description than the definition Octet read (an MCP \
             prompt of that name takes a command's place), so Octet cannot check what it runs"
        ));
    }
    let own = agent(mode);
    if let Some(other) = definition.agent.as_deref().filter(|other| *other != own) {
        // Every agent but the mode's own: a primary one's permissions would
        // apply instead of the mode's, and a subagent-mode one runs the
        // command as a subagent.
        return refuse(format!(
            "opencode runs /{shown} under its own agent {}, whose permissions would apply instead of {label}'s",
            short(other)
        ));
    }
    if definition.subtask == Some(true) {
        return subtask_refusal(Some(name), mode, "Octet did not send it");
    }
    let Some(template) = &definition.template else {
        return refuse(format!(
            "The template of /{shown} is not text Octet can check"
        ));
    };
    if template.contains(SHELL_MARK) {
        return refuse(format!(
            "/{shown}: its template runs shell commands (`!`…``) before opencode asks for any permission"
        ));
    }
    let Some(expanded) = expand(template, args) else {
        return refuse(format!(
            "/{shown} with these arguments expands to more than {} MiB of text, too long for Octet to \
             check",
            MAX_EXPANDED >> 20
        ));
    };
    if expanded.contains(SHELL_MARK) {
        return shell_refusal(name, SHELL_MARK, mode);
    }
    None
}

/// Whether `c` is whitespace to JavaScript (`\s`, and what `trim` removes).
pub(super) fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// The text opencode 1.18.25's `command()` would build from `template` and
/// `args` before it runs its `` !`cmd` ``s: each `$N` replaced by the N-th
/// word of `args` (the highest by every word from there on), each
/// `$ARGUMENTS` by `args` with JavaScript's replacement patterns, and
/// `args` appended on a line of its own when the template takes none.
/// `None` when it would be longer than [`MAX_EXPANDED`].
pub fn expand(template: &str, args: &str) -> Option<String> {
    let words = words(args);
    // The template as text between `$N`s, each with the number after it.
    let mut pieces: Vec<(&str, Option<u128>)> = Vec::new();
    let mut rest = template;
    let mut from = 0;
    while let Some(found) = rest[from..].find('$') {
        let at = from + found;
        let digits = rest[at + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len() - at - 1);
        if digits == 0 {
            from = at + 1;
            continue;
        }
        let number = rest[at + 1..at + 1 + digits].parse().unwrap_or(u128::MAX);
        pieces.push((&rest[..at], Some(number)));
        rest = &rest[at + 1 + digits..];
        from = 0;
    }
    pieces.push((rest, None));
    let highest = pieces.iter().filter_map(|(_, number)| *number).max();
    let mut substituted = String::new();
    for (text, number) in &pieces {
        substituted.push_str(text);
        if let (Some(number), Some(highest)) = (number, highest) {
            substituted.push_str(&word(&words, *number, highest));
        }
        if substituted.len() > MAX_EXPANDED {
            return None;
        }
    }
    let takes_all = template.contains("$ARGUMENTS");
    let mut expanded = replace_all(&substituted, "$ARGUMENTS", args)?;
    if highest.is_none() && !takes_all && !args.trim_matches(js_space).is_empty() {
        expanded.push('\n');
        expanded.push_str(args);
    }
    (expanded.len() <= MAX_EXPANDED).then_some(expanded)
}

/// What `$number` becomes, `highest` being the template's highest: as in
/// opencode, past the last word nothing, the highest every word from there,
/// any other its word (`$0` names none: JavaScript's `undefined`).
fn word(words: &[String], number: u128, highest: u128) -> String {
    let Ok(number) = usize::try_from(number) else {
        return String::new();
    };
    if number > words.len() {
        return String::new();
    }
    // `$0` asks for the word before the first: JavaScript's index -1.
    let index = number.checked_sub(1);
    if number == usize::try_from(highest).unwrap_or(usize::MAX) {
        // `slice(index)`: from `$0`, the last word.
        let from = index.unwrap_or(words.len().saturating_sub(1));
        return words[from..].join(" ");
    }
    index.map_or_else(|| "undefined".to_owned(), |index| words[index].clone())
}

/// `args` split into words as opencode 1.18.25 splits a command's
/// arguments: `[Image N]`, a quoted run, or a run of anything but
/// whitespace and quotes, each without a quote at either end.
fn words(args: &str) -> Vec<String> {
    let chars: Vec<char> = args.chars().collect();
    let mut words = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let end = if let Some(end) = image_word(&chars, at) {
            end
        } else if c == '"' || c == '\'' {
            let Some(close) = chars[at + 1..].iter().position(|&d| d == c) else {
                // No alternative matches at an unclosed quote.
                at += 1;
                continue;
            };
            at + close + 2
        } else if js_space(c) {
            at += 1;
            continue;
        } else {
            chars[at..]
                .iter()
                .position(|&d| js_space(d) || d == '"' || d == '\'')
                .map_or(chars.len(), |end| at + end)
        };
        let mut word: Vec<char> = chars[at..end].to_vec();
        if word.last().is_some_and(|&d| d == '"' || d == '\'') {
            word.pop();
        }
        if word.first().is_some_and(|&d| d == '"' || d == '\'') {
            word.remove(0);
        }
        words.push(word.into_iter().collect());
        at = end;
    }
    words
}

/// Where `[Image N]` (any case, any whitespace before the number) that
/// starts at `at` ends, if one does.
fn image_word(chars: &[char], at: usize) -> Option<usize> {
    let prefix: String = chars.get(at..at + 6)?.iter().collect();
    if !prefix.eq_ignore_ascii_case("[image") {
        return None;
    }
    let mut i = at + 6;
    let spaces = chars[i..].iter().take_while(|&&c| js_space(c)).count();
    if spaces == 0 {
        return None;
    }
    i += spaces;
    let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || chars.get(i + digits) != Some(&']') {
        return None;
    }
    Some(i + digits + 1)
}

/// `text.replaceAll(pattern, replacement)` as JavaScript does it with a
/// text pattern: `$$`, `$&`, `` $` `` and `$'` in `replacement` are a
/// dollar, the match, and the text before and after it. `None` as soon as
/// the result would pass [`MAX_EXPANDED`]: every piece is counted before it
/// is added, so the text never grows past the bound.
fn replace_all(text: &str, pattern: &str, replacement: &str) -> Option<String> {
    /// Adds `piece` to `out`, unless that would pass the bound.
    fn add(out: &mut String, piece: &str) -> Option<()> {
        (out.len() + piece.len() <= MAX_EXPANDED).then(|| out.push_str(piece))
    }
    let mut out = String::new();
    let mut last = 0;
    for (at, _) in text.match_indices(pattern) {
        add(&mut out, &text[last..at])?;
        let after = at + pattern.len();
        let mut rest = replacement;
        while let Some(dollar) = rest.find('$') {
            add(&mut out, &rest[..dollar])?;
            let piece = match rest[dollar + 1..].chars().next() {
                Some('$') => "$",
                Some('&') => pattern,
                Some('`') => &text[..at],
                Some('\'') => &text[after..],
                _ => {
                    add(&mut out, "$")?;
                    rest = &rest[dollar + 1..];
                    continue;
                }
            };
            add(&mut out, piece)?;
            rest = &rest[dollar + 2..];
        }
        add(&mut out, rest)?;
        last = after;
    }
    add(&mut out, &text[last..])?;
    Some(out)
}

/// How Octet starts `opencode debug`: the session's binary, workspace and
/// launch environment.
#[derive(Clone)]
pub(super) struct Launch {
    /// The opencode binary.
    binary: PathBuf,
    /// The workspace, opencode's working directory.
    cwd: PathBuf,
    /// The session's launch variables.
    env: Vec<(OsString, OsString)>,
}

impl Launch {
    /// How `config`'s session starts opencode.
    pub(super) fn of(config: &Config) -> Self {
        Self {
            binary: config.binary.clone(),
            cwd: config.cwd.clone(),
            env: super::launch_env(config),
        }
    }
}

/// Reads every command opencode would run, from `opencode debug config`
/// and `opencode debug skill` run side by side.
///
/// # Errors
///
/// Why the definitions could not be read.
pub(super) async fn read(launch: Launch) -> Result<Book, String> {
    let (config, skills) =
        tokio::join!(debug_json(&launch, "config"), debug_json(&launch, "skill"));
    book_from(&config?, &skills?)
        .map_err(|why| format!("opencode debug printed something else: {why}"))
}

/// A file for `opencode debug`'s output, with no name: opencode 1.18.25
/// exits before a pipe takes more than 64 KiB of its output, so it writes to
/// a file instead, and that output (the whole merged configuration,
/// provider keys included) must be neither found by another program nor
/// left behind, even when Octet is killed. The file is made `O_EXCL` and
/// 0600, and its name removed at once; Octet and the child keep it open.
fn unnamed_output(what: &str) -> std::io::Result<std::fs::File> {
    /// Tells this process's output files apart.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "octet-opencode-{what}-{}-{n}.json",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

/// `opencode debug what`'s output, as JSON.
async fn debug_json(launch: &Launch, what: &str) -> Result<Value, String> {
    let mut file = unnamed_output(what).map_err(|error| {
        format!("Octet could not make a file for opencode debug {what}: {error}")
    })?;
    let stdout = file.try_clone().map_err(|error| {
        format!("Octet could not make a file for opencode debug {what}: {error}")
    })?;
    let mut child = tokio::process::Command::new(&launch.binary)
        .args(["debug", what])
        .envs(launch.env.iter().cloned())
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("opencode debug {what} did not start: {error}"))?;
    let too_much = || {
        format!(
            "opencode debug {what} printed more than {} MiB",
            MAX_OUTPUT >> 20
        )
    };
    // The bound holds while it runs: one that goes on printing is stopped
    // (dropping the child kills it).
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            () = tokio::time::sleep(OUTPUT_POLL) => {
                if file.metadata().is_ok_and(|output| output.len() > MAX_OUTPUT) {
                    return Err(too_much());
                }
            }
        }
    }
    .map_err(|error| format!("opencode debug {what} could not be waited for: {error}"))?;
    if !status.success() {
        return Err(format!("opencode debug {what} {status}"));
    }
    let mut text = String::new();
    file.seek(std::io::SeekFrom::Start(0))
        .and_then(|_| file.take(MAX_OUTPUT + 1).read_to_string(&mut text))
        .map_err(|error| format!("opencode debug {what}'s output could not be read: {error}"))?;
    if text.len() as u64 > MAX_OUTPUT {
        return Err(too_much());
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("opencode debug {what} printed no JSON ({error})"))
}

/// What a command's check decided.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// It may be sent.
    Send,
    /// It must not be sent, for this reason.
    Refuse(String),
    /// Octet is still reading the definitions it needs: the command waits
    /// (see [`Commands::finished`]).
    Wait,
}

/// Where Octet's reading of opencode's commands is.
#[derive(Default)]
enum Reading {
    /// None has started.
    #[default]
    None,
    /// One is under way: its task, killed with its `opencode debug`
    /// processes when dropped, and where its result arrives.
    Running {
        /// The task that reads.
        task: JoinHandle<()>,
        /// Its result, once it has one.
        result: oneshot::Receiver<Result<Book, String>>,
    },
    /// It is done.
    Done(Result<Book, String>),
}

/// The commands opencode lists, and what Octet read of them.
#[derive(Default)]
pub(super) struct Commands {
    /// The commands opencode last listed: each name and description,
    /// bounded.
    listed: Vec<(String, String)>,
    /// The reading for the current list.
    reading: Reading,
    /// A reading that failed has been tried again since the list changed.
    retried: bool,
    /// The mode of a turn sent since opencode listed its commands in which
    /// opencode edits files without asking. opencode 1.18.25 loads its
    /// commands once per process and never reloads them, so after such a
    /// turn their files may no longer be what opencode runs: Octet does not
    /// read them again.
    edited: Option<Mode>,
    /// Such a turn was sent while the reading under way ran, so what it
    /// reads may not be what opencode loaded.
    tainted: Option<Mode>,
    /// What Octet says when opencode reported a version not in
    /// [`VERIFIED_VERSIONS`]: then no command is sent while the mode asks.
    unverified: Option<String>,
}

/// Whether opencode edits files without asking in `mode`.
fn edits_unasked(mode: Mode) -> bool {
    !matches!(mode, Mode::Ask)
}

impl Drop for Commands {
    fn drop(&mut self) {
        self.stop_reading();
    }
}

impl Commands {
    /// opencode's `initialize` reply reported `version` (`None`: none). The
    /// notice to show when it is not one Octet's check is verified against.
    pub(super) fn reported(&mut self, version: Option<&str>) -> Option<String> {
        if version.is_some_and(|version| VERIFIED_VERSIONS.contains(&version)) {
            self.unverified = None;
            return None;
        }
        let shown = version.map_or_else(
            || "(version not reported)".to_owned(),
            |version| {
                version
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_VERSION)
                    .collect()
            },
        );
        self.unverified = Some(format!(
            "opencode {shown} is not verified for Octet's command check; commands are off in ask and \
             accept-edits — plain prompts work; /mode auto runs them under opencode's own settings"
        ));
        self.unverified.clone()
    }

    /// opencode listed `list` (`availableCommands`). A list that differs
    /// from the last drops what was read, and a reading of the new list
    /// starts with `launch`, for at most `limit`. `running` is the mode of
    /// the turn running meanwhile, if any: one that edits unasked leaves
    /// the reading untrusted.
    pub(super) fn listed(
        &mut self,
        list: &Value,
        launch: Launch,
        limit: Duration,
        running: Option<Mode>,
    ) {
        let listed: Vec<(String, String)> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|command| {
                let name = command["name"].as_str()?.to_owned();
                Some((
                    name,
                    command["description"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ))
            })
            .take(MAX_COMMANDS)
            .collect();
        if listed == self.listed && !matches!(self.reading, Reading::None) {
            return;
        }
        self.listed = listed;
        self.retried = false;
        self.start(launch, limit, LIST_SETTLE);
        self.edited = running.filter(|mode| edits_unasked(*mode));
        self.tainted = self.edited;
    }

    /// A turn is being sent in `mode` (see `edited` and `tainted`).
    pub(super) fn turn_sent(&mut self, mode: Mode) {
        if !edits_unasked(mode) {
            return;
        }
        self.edited = Some(mode);
        if self.reading() {
            self.tainted = Some(mode);
        }
    }

    /// Whether `name` may be sent with `args` under `mode` (see
    /// [`refusal`]). A command that needs a reading not yet at hand waits;
    /// one starts with `launch`, for at most `limit`, when none is under
    /// way, and a reading that failed is tried once more, unless a turn
    /// may have changed the files since opencode loaded them. An opencode
    /// of a version the check is not verified against sends none (see
    /// [`Commands::reported`]). `compact`, when
    /// opencode lists no command of that name, is its own summary and
    /// needs none.
    pub(super) fn verdict(
        &mut self,
        launch: Launch,
        limit: Duration,
        name: &str,
        args: &str,
        mode: Mode,
    ) -> Verdict {
        if !matches!(mode, Mode::Ask | Mode::AcceptEdits) {
            return Verdict::Send;
        }
        let listed = self
            .listed
            .iter()
            .find(|(listed, _)| listed == name)
            .map(|(_, description)| description.clone());
        if name == "compact" && listed.is_none() {
            return Verdict::Send;
        }
        if let Some(unverified) = &self.unverified {
            return Verdict::Refuse(format!("Octet did not send /{}: {unverified}", short(name)));
        }
        self.poll();
        match &self.reading {
            Reading::Running { .. } => Verdict::Wait,
            Reading::Done(Ok(book)) => refusal(book, listed.as_deref(), name, args, mode)
                .map_or(Verdict::Send, Verdict::Refuse),
            Reading::Done(Err(_)) | Reading::None if !self.retried && self.edited.is_none() => {
                self.retried = true;
                self.start(launch, limit, Duration::ZERO);
                Verdict::Wait
            }
            Reading::Done(Err(why)) => Verdict::Refuse(self.unread(why, name, mode)),
            Reading::None => Verdict::Refuse(self.unread("none was made in time", name, mode)),
        }
    }

    /// Why `name` cannot be sent under `mode` when Octet could not read
    /// opencode's commands, `why`.
    fn unread(&self, why: &str, name: &str, mode: Mode) -> String {
        let again = self.edited.map_or_else(String::new, |edited| {
            format!(
                " A turn has run in {} since opencode loaded its commands, which opencode does not \
                 load again, so Octet does not read them again either; /reconnect starts opencode \
                 afresh.",
                edited.label()
            )
        });
        format!(
            "Octet could not read opencode's commands ({why}), so it cannot check /{}; in {} \
             Octet sends only commands it has checked, and did not send it. In auto, opencode's \
             own permission settings decide.{again}",
            short(name),
            mode.label()
        )
    }

    /// Whether a reading is under way.
    pub(super) fn reading(&mut self) -> bool {
        self.poll();
        matches!(self.reading, Reading::Running { .. })
    }

    /// Resolves once the reading under way is done; never while none is.
    pub(super) async fn finished(&mut self) {
        let Reading::Running { result, .. } = &mut self.reading else {
            return std::future::pending().await;
        };
        let read = result
            .await
            .unwrap_or_else(|_| Err("the reading stopped".to_owned()));
        self.land(read);
    }

    /// Keeps the result `read` of the reading under way, which a turn sent
    /// meanwhile leaves untrusted.
    fn land(&mut self, read: Result<Book, String>) {
        let read = match self.tainted.take() {
            Some(mode) if read.is_ok() => Err(format!(
                "a turn ran in {} while Octet read them, so they may not be what opencode loaded",
                mode.label()
            )),
            _ => read,
        };
        self.reading = Reading::Done(read);
    }

    /// Takes the result of a reading that has one.
    fn poll(&mut self) {
        if let Reading::Running { result, .. } = &mut self.reading {
            match result.try_recv() {
                Ok(read) => self.land(read),
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.land(Err("the reading stopped".to_owned()));
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
    }

    /// Starts a reading with `launch`, for at most `limit`, in place of
    /// any under way, once `settle` has passed without another.
    fn start(&mut self, launch: Launch, limit: Duration, settle: Duration) {
        self.stop_reading();
        let (sender, result) = oneshot::channel();
        let task = tokio::spawn(async move {
            tokio::time::sleep(settle).await;
            let read = tokio::time::timeout(limit, read(launch))
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "opencode debug took longer than {}",
                        crate::live::driver::seconds(limit)
                    ))
                });
            let _ = sender.send(read);
        });
        self.reading = Reading::Running { task, result };
    }

    /// Stops the reading under way, if any; its `opencode debug` processes
    /// are killed with it.
    fn stop_reading(&mut self) {
        if let Reading::Running { task, .. } = std::mem::take(&mut self.reading) {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// How Octet would start the scripted `opencode` `body` (a shell
    /// script), in a folder of its own that lives as long as the guard.
    fn scripted(name: &str, body: &str) -> (octet_testkit::TempDir, Launch) {
        let dir = octet_testkit::TempDir::new(name);
        let binary = octet_testkit::write_script(dir.path(), "opencode", body);
        let launch = Launch {
            binary,
            cwd: dir.path().to_path_buf(),
            env: Vec::new(),
        };
        (dir, launch)
    }

    #[tokio::test]
    async fn opencode_debug_is_stopped_once_it_prints_too_much() {
        // It goes on running after passing the bound; Octet stops it then,
        // not when it exits.
        let (_dir, launch) = scripted(
            "octet-debug-too-much",
            "head -c 17000000 /dev/zero; exec sleep 30",
        );
        let started = std::time::Instant::now();
        let error = debug_json(&launch, "config").await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(error, "opencode debug config printed more than 16 MiB");
    }

    #[tokio::test]
    async fn the_debug_output_has_no_name_while_opencode_writes_it() {
        // It holds the whole merged configuration, provider keys included:
        // nothing else should find it, and nothing should outlive Octet.
        // The script counts the files named as Octet names its output.
        let (_dir, launch) = scripted(
            "octet-debug-unnamed",
            "n=$(ls \"${TMPDIR:-/tmp}\" | grep -c \"^octet-opencode-$2-$PPID-\")\n\
             if [ \"$2\" = config ]; then echo \"{\\\"command\\\":{\\\"probe\\\":{\\\"template\\\":\\\"$n\\\"}}}\"; \
             else echo '[]'; fi",
        );
        let book = read(launch).await.unwrap();
        assert_eq!(book["probe"].template.as_deref(), Some("0"));
    }

    /// A book of `commands` (a `command` map) and `skills`.
    fn book(commands: &Value, skills: &Value) -> Book {
        book_from(&json!({"command": commands}), skills).unwrap()
    }

    #[test]
    fn the_book_holds_built_ins_configured_commands_then_skills() {
        let read = book(
            &json!({
                "review": {"template": "mine", "description": "My review"},
                "x": {"agent": "build", "subtask": false, "template": "t", "description": "X"},
                "odd": {"agent": 3, "subtask": "yes", "template": ["t"]},
            }),
            &json!([
                {"name": "x", "description": "a skill", "location": "/s/x/SKILL.md", "content": "c"},
                {"name": "s", "description": "S", "location": "/s/s/SKILL.md", "content": "c"},
                {"name": "b", "description": "B", "location": "<built-in>", "content": "c"},
            ]),
        );
        assert_eq!(read["init"].description, "guided AGENTS.md setup");
        assert_eq!(
            read["review"].subtask, None,
            "a configured review replaces the built-in"
        );
        assert_eq!(read["x"].agent.as_deref(), Some("build"));
        assert_eq!(read["x"].description, "X", "a command outranks a skill");
        assert_eq!(read["odd"].agent.as_deref(), Some("3"));
        assert_eq!(read["odd"].subtask, Some(true));
        assert_eq!(read["odd"].template, None);
        assert_eq!(
            read["s"].template.as_deref(),
            Some(
                "c\n\nBase directory for this skill: /s/s\nRelative paths in this skill (e.g., scripts/, \
                 references/) are relative to this base directory."
            )
        );
        assert_eq!(read["b"].template.as_deref(), Some("c"));
        assert_eq!(built_ins()["review"].subtask, Some(true));
        for bad in [json!([]), json!({"command": []})] {
            assert!(book_from(&bad, &json!([])).is_err(), "{bad}");
        }
        assert!(book_from(&json!({}), &json!({})).is_err());
    }

    #[test]
    fn expansion_follows_opencodes_command() {
        assert_eq!(expand("Say $ARGUMENTS.", "a  b").unwrap(), "Say a  b.");
        assert_eq!(expand("No args", "a b").unwrap(), "No args\na b");
        assert_eq!(expand("No args", " ").unwrap(), "No args");
        assert_eq!(
            expand("$1 and $2", "one two three").unwrap(),
            "one and two three"
        );
        assert_eq!(expand("$2 $1 $3", "a").unwrap(), " a ");
        assert_eq!(
            expand("\"$1\"", "'quoted word' x").unwrap(),
            "\"quoted word x\""
        );
        assert_eq!(expand("$1-$2", "'quoted word' x").unwrap(), "quoted word-x");
        assert_eq!(expand("$1|$2", "[Image 1] y").unwrap(), "[Image 1]|y");
        assert_eq!(expand("$0", "a b").unwrap(), "b");
        assert_eq!(expand("$0 $1", "a b").unwrap(), "undefined a b");
        assert_eq!(expand("$ $x", "a").unwrap(), "$ $x\na");
        // JavaScript's replacement patterns in the arguments.
        assert_eq!(
            expand("<$ARGUMENTS>", "$$ $& $` $'").unwrap(),
            "<$ $ARGUMENTS < >>"
        );
    }

    #[test]
    fn a_replacement_stops_once_its_text_is_too_long() {
        // Many matches of a long replacement with no pattern in it: plain
        // text alone passes the bound.
        let text = "$ARGUMENTS".repeat(2048);
        let replacement = "a".repeat(4096);
        assert_eq!(replace_all(&text, "$ARGUMENTS", &replacement), None);
        // Text between the matches counts too.
        let text = format!("{}$ARGUMENTS", "t".repeat(MAX_EXPANDED + 1));
        assert_eq!(replace_all(&text, "$ARGUMENTS", "x"), None);
        assert_eq!(
            replace_all("a$ARGUMENTSb", "$ARGUMENTS", "$$").as_deref(),
            Some("a$b")
        );
    }

    #[test]
    fn a_command_too_long_to_expand_is_refused_for_its_length() {
        let read = book(
            &json!({"long": {"template": "$ARGUMENTS".repeat(2048)}}),
            &json!([]),
        );
        let refusal = refusal(&read, None, "long", &"a".repeat(4096), Mode::Ask).unwrap();
        assert!(refusal.contains("too long for Octet to check"), "{refusal}");
        assert!(!refusal.contains("shell"), "{refusal}");
        assert!(refusal.contains("ask"), "{refusal}");
    }

    #[test]
    fn shell_the_arguments_complete_is_found() {
        let shell =
            |template: &str, args: &str| expand(template, args).unwrap().contains(SHELL_MARK);
        assert!(shell("Run !$1 now", "`id`"));
        assert!(shell("!$ARGUMENTS", "`id`"));
        assert!(shell("Go !$ARGUMENTS`id`", "$'"));
        assert!(shell("x", "!`id`"));
        assert!(!shell("Say $ARGUMENTS", "hi `x` !"));
        assert!(!shell("Say $1", "! `x`"));
    }

    #[test]
    fn a_command_is_refused_for_its_agent_subtask_or_shell_while_the_mode_asks() {
        let read = book(
            &json!({
                "plain": {"template": "Say $ARGUMENTS", "description": "P"},
                "build": {"agent": "build", "template": "t"},
                "own": {"agent": "octet-ask", "template": "t"},
                "sub": {"subtask": true, "template": "t"},
                "shell": {"template": "!`date`"},
                "bang": {"template": "!$1"},
                "opaque": {},
            }),
            &json!([]),
        );
        let refused = |name: &str, args: &str, mode| refusal(&read, None, name, args, mode);
        assert_eq!(refused("plain", "hi", Mode::Ask), None);
        assert_eq!(refused("own", "", Mode::Ask), None);
        assert_eq!(refused("init", "", Mode::Ask), None);
        assert_eq!(refused("compact", "", Mode::Ask), None);
        for (name, args, why) in [
            ("build", "", "agent build"),
            ("own", "", "agent octet-ask"),
            ("sub", "", "in a subagent"),
            ("review", "", "in a subagent"),
            ("shell", "", "its template runs"),
            ("bang", "`id`", "in its shell"),
            ("opaque", "", "not text"),
            ("missing", "", "no definition"),
        ] {
            let mode = if name == "own" {
                Mode::AcceptEdits
            } else {
                Mode::Ask
            };
            let refusal =
                refused(name, args, mode).unwrap_or_else(|| panic!("{name} was not refused"));
            assert!(refusal.contains(why), "{name}: {refusal}");
            assert!(refusal.contains(mode.label()), "{refusal}");
        }
        assert!(
            refusal(&read, Some("Q"), "plain", "", Mode::Ask)
                .unwrap()
                .contains("lists /plain")
        );
        assert_eq!(refusal(&read, Some("P"), "plain", "", Mode::Ask), None);
        for mode in [Mode::Auto, Mode::FullAccess] {
            assert_eq!(refused("build", "", mode), None);
            assert_eq!(refused("missing", "", mode), None);
        }
    }
}
