//! opencode's slash commands, checked before one is sent while the mode
//! asks. opencode 1.18.25 runs a command (a configured one, a skill, an MCP
//! prompt or a built-in) in ways no permission check sees: under the agent
//! its definition names instead of the session's, as a subagent whose
//! approval requests never reach Octet, and with every `` !`cmd` `` of its
//! expanded text run in the shell first. Octet reads each definition as
//! that opencode would (`opencode debug config` and `opencode debug skill`,
//! with the session's launch environment, in its workspace), and refuses a
//! command it cannot read or that would run any of those ways.
use super::{SHELL_MARK, agent, shell_refusal, subtask_refusal};
use crate::live::{Config, Mode, short};
use serde_json::Value;
use std::{
    collections::HashMap,
    ffi::OsString,
    io::Read as _,
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::task::JoinHandle;

/// The most commands one reading keeps; the rest are unknown, so refused.
const MAX_COMMANDS: usize = 4096;
/// The most bytes one `opencode debug` output may have.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
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
    let shell = expand(template, args).is_none_or(|text| text.contains(SHELL_MARK));
    if shell {
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
pub(super) fn expand(template: &str, args: &str) -> Option<String> {
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
/// dollar, the match, and the text before and after it. `None` past
/// [`MAX_EXPANDED`].
fn replace_all(text: &str, pattern: &str, replacement: &str) -> Option<String> {
    let mut out = String::new();
    let mut last = 0;
    for (at, _) in text.match_indices(pattern) {
        out.push_str(&text[last..at]);
        let after = at + pattern.len();
        let mut chars = replacement.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            match (c, chars.peek().map(|&(_, next)| next)) {
                ('$', Some('$')) => out.push('$'),
                ('$', Some('&')) => out.push_str(pattern),
                ('$', Some('`')) => out.push_str(&text[..at]),
                ('$', Some('\'')) => out.push_str(&text[after..]),
                _ => {
                    out.push(c);
                    continue;
                }
            }
            chars.next();
            if out.len() > MAX_EXPANDED {
                return None;
            }
        }
        last = after;
    }
    out.push_str(&text[last..]);
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

/// A file `opencode debug` writes its output to, removed when dropped.
/// opencode 1.18.25 exits before a pipe takes more than 64 KiB of its
/// output, so its output goes to a file instead.
struct Output(PathBuf);

impl Drop for Output {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `opencode debug what`'s output, as JSON.
async fn debug_json(launch: &Launch, what: &str) -> Result<Value, String> {
    /// Tells this process's output files apart.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let output = Output(std::env::temp_dir().join(format!(
        "octet-opencode-{what}-{}-{n}.json",
        std::process::id()
    )));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&output.0)
        .map_err(|error| {
            format!("Octet could not make a file for opencode debug {what}: {error}")
        })?;
    let status = tokio::process::Command::new(&launch.binary)
        .args(["debug", what])
        .envs(launch.env.iter().cloned())
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|error| format!("opencode debug {what} did not start: {error}"))?;
    if !status.success() {
        return Err(format!("opencode debug {what} {status}"));
    }
    let mut text = String::new();
    std::fs::File::open(&output.0)
        .and_then(|file| file.take(MAX_OUTPUT + 1).read_to_string(&mut text))
        .map_err(|error| format!("opencode debug {what}'s output could not be read: {error}"))?;
    if text.len() as u64 > MAX_OUTPUT {
        return Err(format!(
            "opencode debug {what} printed more than {} MiB",
            MAX_OUTPUT >> 20
        ));
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("opencode debug {what} printed no JSON ({error})"))
}

/// The commands opencode lists, and what Octet read of them.
#[derive(Default)]
pub(super) struct Commands {
    /// The commands opencode last listed: each name and description,
    /// bounded.
    listed: Vec<(String, String)>,
    /// What Octet read for the current list.
    book: Option<Book>,
    /// The reading under way.
    reading: Option<JoinHandle<Result<Book, String>>>,
}

impl Drop for Commands {
    fn drop(&mut self) {
        self.stop_reading();
    }
}

impl Commands {
    /// opencode listed `list` (`availableCommands`). A list that differs
    /// from the last drops what was read; with `launch`, a reading of the
    /// new list starts.
    pub(super) fn listed(&mut self, list: &Value, launch: Option<Launch>) {
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
        if listed == self.listed && (self.book.is_some() || self.reading.is_some()) {
            return;
        }
        self.listed = listed;
        self.book = None;
        self.stop_reading();
        if let Some(launch) = launch {
            self.reading = Some(tokio::spawn(read(launch)));
        }
    }

    /// Why `name` cannot be sent with `args` under `mode` (see
    /// [`refusal`]), reading the commands first when nothing read is at
    /// hand, for at most `limit`.
    pub(super) async fn check(
        &mut self,
        launch: Launch,
        limit: Duration,
        name: &str,
        args: &str,
        mode: Mode,
    ) -> Option<String> {
        if !matches!(mode, Mode::Ask | Mode::AcceptEdits) {
            return None;
        }
        if self.book.is_none() {
            let mut reading = self
                .reading
                .take()
                .unwrap_or_else(|| tokio::spawn(read(launch)));
            let read = match tokio::time::timeout(limit, &mut reading).await {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => Err(format!("the reading stopped: {error}")),
                Err(_) => {
                    reading.abort();
                    Err(format!(
                        "opencode debug took longer than {}",
                        crate::live::driver::seconds(limit)
                    ))
                }
            };
            match read {
                Ok(book) => self.book = Some(book),
                Err(why) => {
                    return Some(format!(
                        "Octet could not read opencode's commands ({why}), so it cannot check /{}; in {} \
                         Octet sends only commands it has checked, and did not send it. In auto, opencode's \
                         own permission settings decide",
                        short(name),
                        mode.label()
                    ));
                }
            }
        }
        let listed = self
            .listed
            .iter()
            .find(|(listed, _)| listed == name)
            .map(|(_, description)| description.as_str());
        refusal(self.book.as_ref()?, listed, name, args, mode)
    }

    /// Stops the reading under way, if any; its `opencode debug` processes
    /// are killed with it.
    fn stop_reading(&mut self) {
        if let Some(reading) = self.reading.take() {
            reading.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
