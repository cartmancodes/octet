//! Starting terminal UIs in tmux for a phone, and listing the sessions it may resume.
//!
//! Everything a phone chooses (engine, workspace, resume id, model, mode) reaches tmux as one
//! literal argument each: no shell, no joined strings. Workspaces are checked against an
//! allow-list of canonical directories.
use octet_core::{
    Engine, Mode, recent_sessions_in,
    remote::{
        known_workspaces,
        protocol::{Code, full_access_confirmed},
    },
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    process::Command,
    time::{Instant, timeout},
};

/// Sessions started from the phone that may run at once.
pub(crate) const PHONE_SESSIONS: usize = 8;
/// How long tmux gets to start a session.
const LAUNCH_WITHIN: Duration = Duration::from_secs(5);
/// How long removing a session that took too long to start may take.
const KILL_WITHIN: Duration = Duration::from_secs(2);
/// The longest tmux error kept, in characters.
const TMUX_ERROR: usize = 500;
/// The longest phone-chosen path echoed back, in characters.
const ECHOED_PATH: usize = 200;
/// The longest title of a recent session, in characters.
const TITLE_CHARS: usize = 60;
/// Recent sessions listed for one workspace.
const PER_WORKSPACE: usize = 20;
/// Recent sessions listed across all workspaces.
const IN_TOTAL: usize = 40;

/// Why a phone's request was refused: its code, and the words to show.
pub(crate) type Refusal = (Code, String);

/// The longest device name passed to a started interface, in characters.
const DEVICE_CHARS: usize = 40;

/// What a phone asked `start` for.
#[derive(Debug, Clone)]
pub(crate) struct StartRequest {
    /// The provider's name.
    pub engine: String,
    /// The workspace directory.
    pub workspace: PathBuf,
    /// A vendor session to resume.
    pub resume: Option<String>,
    /// A model to start with.
    pub model: Option<String>,
    /// A mode to start in.
    pub mode: Option<String>,
    /// The confirmation `full-access` needs.
    pub confirm: Option<String>,
}

/// At most `chars` characters of `text`.
pub(crate) fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// `value` as one plain argument: not empty, no whitespace or control characters, no `;` or `#`
/// (tmux's separator and format character), and not shaped like an option.
fn token(what: &str, value: &str) -> Result<(), Refusal> {
    let plain = !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ';' || c == '#');
    if plain {
        Ok(())
    } else {
        Err((Code::Invalid, format!("Invalid {what}")))
    }
}

/// The canonical form of `requested` if it is one of the `allowed` directories.
///
/// # Errors
///
/// "No such workspace" when it cannot be resolved, and "Workspace not allowed" when it is not
/// in the list.
pub(crate) fn check_workspace(requested: &Path, allowed: &[PathBuf]) -> Result<PathBuf, Refusal> {
    let real = std::fs::canonicalize(requested)
        .map_err(|_| (Code::Invalid, "No such workspace".to_owned()))?;
    if allowed.contains(&real) {
        return Ok(real);
    }
    let shown = requested.to_string_lossy();
    Err((
        Code::NotAllowed,
        format!("Workspace not allowed: {}", clip(&shown, ECHOED_PATH)),
    ))
}

/// A device's name as the started interface's `--remote-by`: one line without control
/// characters, not shaped like an option, at most [`DEVICE_CHARS`] characters; `None` if
/// nothing is left.
fn device_argument(name: &str) -> Option<String> {
    let line = name
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let line = clip(line.trim_start_matches('-').trim_start(), DEVICE_CHARS);
    (!line.is_empty()).then_some(line)
}

/// `arg` as tmux takes a command's argument: tmux ends a command at any argument that ends in
/// `;`, unless that is escaped as `\;`.
fn tmux_argument(arg: &OsStr) -> OsString {
    let mut bytes = arg.as_bytes().to_vec();
    if bytes.ends_with(b";") {
        bytes.insert(bytes.len() - 1, b'\\');
    }
    OsString::from_vec(bytes)
}

/// `path` as tmux's `-c` takes it: `#` starts a format there, so it is doubled.
fn tmux_directory(path: &str) -> OsString {
    tmux_argument(OsStr::new(&path.replace('#', "##")))
}

/// The tmux arguments for a phone-started interface; Err when the request is not allowed.
///
/// `allowed` holds canonical directories, `running` is how many phone-started sessions exist,
/// `name` is the tmux session's name and `by` the device asking. Each choice of the phone is
/// one argument.
pub(crate) fn start_args(
    octet: &Path,
    directory: &Path,
    request: &StartRequest,
    allowed: &[PathBuf],
    running: usize,
    (name, by): (&str, &str),
) -> Result<Vec<OsString>, Refusal> {
    if running >= PHONE_SESSIONS {
        return Err((
            Code::Busy,
            format!("{PHONE_SESSIONS} sessions started from the phone are running; end one first"),
        ));
    }
    let engine = Engine::parse(&request.engine).ok_or_else(|| {
        (
            Code::Invalid,
            format!("Unknown engine {}", clip(&request.engine, 32)),
        )
    })?;
    for (what, value) in [
        ("session to resume", &request.resume),
        ("model", &request.model),
        ("mode", &request.mode),
    ] {
        if let Some(value) = value {
            token(what, value)?;
        }
    }
    let mode = match &request.mode {
        Some(text) => Some(
            Mode::parse(text)
                .ok_or_else(|| (Code::Invalid, format!("Unknown mode {}", clip(text, 32))))?,
        ),
        None => None,
    };
    if let Some(mode) = mode
        && !full_access_confirmed(mode.label(), request.confirm.as_deref())
    {
        return Err((
            Code::NotAllowed,
            "Full access needs confirm: \"full access\"".into(),
        ));
    }
    let workspace = check_workspace(&request.workspace, allowed)?;
    let text = workspace
        .to_str()
        .ok_or((Code::Invalid, "The workspace path must be UTF-8".to_owned()))?;
    let mut args: Vec<OsString> = ["new-session", "-d", "-s", name, "-c"]
        .map(OsString::from)
        .into();
    args.push(tmux_directory(text));
    args.push(tmux_argument(octet.as_os_str()));
    args.push("--journal-dir".into());
    args.push(tmux_argument(directory.as_os_str()));
    // Octet's own `--cwd` is literal, and refuses what is not a directory, so a path tmux
    // misread could never leave the session in `$HOME`.
    args.push("--cwd".into());
    args.push(tmux_argument(workspace.as_os_str()));
    args.push("--engine".into());
    args.push(engine.as_str().into());
    args.push("--remote-started".into());
    args.push(name.into());
    if let Some(device) = device_argument(by) {
        args.push("--remote-by".into());
        args.push(tmux_argument(OsStr::new(&device)));
    }
    for (flag, value) in [
        ("--resume", request.resume.as_deref()),
        ("--model", request.model.as_deref()),
        ("--mode", mode.map(Mode::label)),
    ] {
        if let Some(value) = value {
            args.push(flag.into());
            args.push(value.into());
        }
    }
    Ok(args)
}

/// A tmux session name for a new interface: `octet-` and six hex digits.
///
/// # Errors
///
/// When the operating system gave no random bytes.
pub(crate) fn session_name() -> Result<String, Refusal> {
    let mut bytes = [0_u8; 3];
    getrandom::fill(&mut bytes)
        .map_err(|_| (Code::Unavailable, "Couldn't make a session name".to_owned()))?;
    Ok(format!(
        "octet-{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2]
    ))
}

/// Runs tmux with `args`, which start the session `name`, giving it 5 seconds. A tmux that runs
/// out of time is stopped, and so is the session, if it got that far.
///
/// # Errors
///
/// Why it did not start a session: tmux is missing, it ran out of time, or what it said on
/// stderr (clipped).
pub(crate) async fn launch(args: Vec<OsString>, name: &str) -> Result<(), Refusal> {
    let child = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => (Code::Unavailable, "tmux isn't installed".to_owned()),
            _ => (Code::Unavailable, format!("Couldn't run tmux: {e}")),
        })?;
    let Ok(output) = timeout(LAUNCH_WITHIN, child.wait_with_output()).await else {
        return Err((Code::Timeout, abandon(name).await));
    };
    let output = output.map_err(|e| (Code::Unavailable, format!("Couldn't run tmux: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = said.trim();
    let reason = if said.is_empty() {
        "tmux couldn't start the session".to_owned()
    } else {
        clip(said, TMUX_ERROR)
    };
    Err((Code::Unavailable, reason))
}

/// How long an accepted launch counts toward the cap while its interface has not registered.
const REGISTER_WITHIN: Duration = Duration::from_secs(30);

/// Launches the cap must count beyond the registrations: those tmux is still starting, and those
/// it started whose interface has not registered yet (up to [`REGISTER_WITHIN`]).
#[derive(Debug, Default)]
pub(crate) struct Launches {
    /// Launches tmux has not answered yet.
    in_flight: usize,
    /// When each accepted launch stops counting, oldest first.
    awaiting: VecDeque<Instant>,
    /// The phone-started registrations seen last time.
    seen: usize,
}

impl Launches {
    /// A launch begins.
    pub(crate) fn begin(&mut self) {
        self.in_flight += 1;
    }

    /// The launch that began was accepted by tmux at `now`.
    pub(crate) fn accepted(&mut self, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.awaiting.push_back(now + REGISTER_WITHIN);
    }

    /// The launch that began did not start a session.
    pub(crate) fn failed(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// How many phone-started sessions exist or are on their way, given the `registered` ones: each
    /// newly seen registration settles the oldest awaited launch, and a launch not settled within
    /// [`REGISTER_WITHIN`] stops counting.
    pub(crate) fn running(&mut self, registered: usize, now: Instant) -> usize {
        for _ in self.seen..registered {
            self.awaiting.pop_front();
        }
        self.seen = registered;
        self.awaiting.retain(|deadline| *deadline > now);
        registered + self.in_flight + self.awaiting.len()
    }
}

/// What to tell the phone about a tmux that ran out of time, after trying to remove the session
/// it may have created.
async fn abandon(name: &str) -> String {
    let kill = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    match timeout(KILL_WITHIN, kill).await {
        Ok(Ok(status)) if status.success() => "tmux didn't answer in 5 seconds".to_owned(),
        _ => "The session may still be starting".to_owned(),
    }
}

/// A session tmux started.
#[derive(Debug)]
pub(crate) struct Started {
    /// The tmux session's name.
    pub name: String,
    /// The canonical workspace it runs in.
    pub workspace: PathBuf,
}

/// Where the literal canonical workspace (the value of `--cwd`) sits in [`start_args`]'s result.
const WORKSPACE_ARG: usize = 10;

/// Checks `request` against the allowed workspaces and starts it in a new tmux session, on
/// behalf of the device `by`.
///
/// # Errors
///
/// Why the session was not started: the request is not allowed, or tmux failed.
pub(crate) async fn start(
    directory: &Path,
    configured: &[PathBuf],
    request: &StartRequest,
    (running, by): (usize, &str),
) -> Result<Started, Refusal> {
    let allowed = allowed_workspaces(directory, configured).await;
    let octet = std::env::current_exe()
        .map_err(|e| (Code::Unavailable, format!("Couldn't find octet: {e}")))?;
    let name = session_name()?;
    let args = start_args(&octet, directory, request, &allowed, running, (&name, by))?;
    let workspace = args
        .get(WORKSPACE_ARG)
        .map(PathBuf::from)
        .unwrap_or_default();
    launch(args, &name).await?;
    Ok(Started { name, workspace })
}

/// The directories a phone may start sessions in: the configured ones and the workspaces of
/// the newest journals, canonical, without repeats. Entries that are not directories are left
/// out.
pub(crate) async fn allowed_workspaces(directory: &Path, configured: &[PathBuf]) -> Vec<PathBuf> {
    let known = known_workspaces(directory).await;
    let mut allowed: Vec<PathBuf> = Vec::new();
    for path in configured.iter().chain(&known) {
        let Ok(real) = tokio::fs::canonicalize(path).await else {
            continue;
        };
        let is_dir = tokio::fs::metadata(&real).await.is_ok_and(|m| m.is_dir());
        if is_dir && !allowed.contains(&real) {
            allowed.push(real);
        }
    }
    allowed
}

/// The `RecentInfo` rows for `workspace`, or for every allowed workspace, newest first.
///
/// # Errors
///
/// Why `workspace` cannot be listed: it does not exist or is not allowed.
pub(crate) async fn recent(
    directory: &Path,
    allowed: &[PathBuf],
    workspace: Option<&Path>,
) -> Result<Vec<Value>, Refusal> {
    let workspaces = match workspace {
        Some(requested) => vec![check_workspace(requested, allowed)?],
        None => allowed.to_vec(),
    };
    let limit = if workspace.is_some() {
        PER_WORKSPACE
    } else {
        IN_TOTAL
    };
    let found = recent_sessions_in(directory, &workspaces, PER_WORKSPACE, limit).await;
    Ok(found
        .into_iter()
        .map(|session| {
            let summary = session.summary;
            let started = summary
                .started
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let title = summary
                .first_prompt
                .as_deref()
                .map_or_else(|| "(no prompt)".to_owned(), |p| clip(p, TITLE_CHARS));
            let mut row = json!({
                "engine": session.engine.as_str(),
                "workspace": summary.cwd.to_string_lossy(),
                "session": summary.session,
                "title": title,
                "started": started,
            });
            if let Some(model) = summary.model.filter(|m| !m.is_empty()) {
                row["model"] = json!(model);
            }
            row
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;

    /// A directory called `name` under a fresh temp dir, canonical, and the temp dir.
    fn workspace(name: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new("octet-hub-launch");
        let path = temp.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        (temp, path)
    }

    fn request(workspace: &Path) -> StartRequest {
        StartRequest {
            engine: "demo".into(),
            workspace: workspace.to_path_buf(),
            resume: None,
            model: None,
            mode: None,
            confirm: None,
        }
    }

    fn args(
        request: &StartRequest,
        allowed: &[PathBuf],
        running: usize,
    ) -> Result<Vec<String>, String> {
        start_args(
            Path::new("/bin/octet"),
            Path::new("/data dir"),
            request,
            allowed,
            running,
            ("octet-abc123", "Gemini 10"),
        )
        .map(|args| {
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        })
        .map_err(|(_, reason)| reason)
    }

    #[test]
    fn start_arguments_are_literal_and_workspaces_checked() {
        let (_temp, hostile) = workspace("a b; rm -rf ~");
        let allowed = vec![hostile.clone()];
        let hostile_text = hostile.to_string_lossy().into_owned();
        let got = args(&request(&hostile), &allowed, 0).unwrap();
        assert_eq!(
            got,
            [
                "new-session",
                "-d",
                "-s",
                "octet-abc123",
                "-c",
                &hostile_text,
                "/bin/octet",
                "--journal-dir",
                "/data dir",
                "--cwd",
                &hostile_text,
                "--engine",
                "demo",
                "--remote-started",
                "octet-abc123",
                "--remote-by",
                "Gemini 10",
            ]
        );

        // Not in the allow-list.
        let (_other_temp, other) = workspace("other");
        let refused = args(&request(&other), &allowed, 0).unwrap_err();
        assert!(refused.starts_with("Workspace not allowed: "), "{refused}");

        // `..` is resolved before the check: leaving an allowed directory is refused, and
        // returning into it is allowed.
        let mut sneaky = request(&hostile.join(".."));
        assert!(
            args(&sneaky, &allowed, 0)
                .unwrap_err()
                .starts_with("Workspace not allowed")
        );
        sneaky.workspace = hostile.join("..").join(hostile.file_name().unwrap());
        let got = args(&sneaky, &allowed, 0).unwrap();
        assert_eq!(got[5], hostile_text);

        // A missing directory.
        let missing = request(&hostile.join("missing"));
        assert_eq!(
            args(&missing, &allowed, 0).unwrap_err(),
            "No such workspace"
        );

        // Echoed paths are clipped.
        let (_t, deep) = workspace(&format!("{0}/{0}", "x".repeat(100)));
        let message = args(&request(&deep), &allowed, 0).unwrap_err();
        assert_eq!(
            message.chars().count(),
            "Workspace not allowed: ".len() + 200,
            "{message}"
        );
    }

    #[test]
    fn the_device_name_reaches_the_interface_as_one_short_plain_argument() {
        assert_eq!(device_argument("Gemini 10").as_deref(), Some("Gemini 10"));
        assert_eq!(
            device_argument("  My\n\tiPhone \u{1b}[2J  ").as_deref(),
            Some("My iPhone [2J")
        );
        assert_eq!(
            device_argument("--mode full-access").as_deref(),
            Some("mode full-access")
        );
        assert_eq!(
            device_argument(&"é".repeat(90)).map(|d| d.chars().count()),
            Some(40)
        );
        assert_eq!(device_argument(" - \u{7}"), None);
        // Nothing left to name: the flag is left out.
        let (_temp, ws) = workspace("w");
        let got = start_args(
            Path::new("/bin/octet"),
            Path::new("/data"),
            &request(&ws),
            std::slice::from_ref(&ws),
            0,
            ("octet-abc123", "--"),
        )
        .unwrap();
        assert!(!got.iter().any(|a| a == "--remote-by"), "{got:?}");
    }

    #[test]
    fn tmux_formats_and_separators_in_workspaces_are_escaped_in_the_cwd_flag_only() {
        for (name, escaped) in [
            ("C#", "C##"),
            ("notes#S", "notes##S"),
            ("a#{b}", "a##{b}"),
            ("semi;", "semi\\;"),
            ("odd#;", "odd##\\;"),
        ] {
            let (_temp, ws) = workspace(name);
            let text = ws.to_string_lossy().into_owned();
            let got = args(&request(&ws), std::slice::from_ref(&ws), 0).unwrap();
            let want = format!("{}{escaped}", text.strip_suffix(name).unwrap());
            assert_eq!(got[4], "-c", "{name}");
            assert_eq!(got[5], want, "{name}");
            // The flag's value is the path, but a trailing `;` is escaped for tmux there too.
            let literal = match text.strip_suffix(';') {
                Some(rest) => format!("{rest}\\;"),
                None => text.clone(),
            };
            assert_eq!(got[9..11], ["--cwd", literal.as_str()], "{name}");
        }
        // Any other path argument ending in `;` is escaped the same way.
        let (_temp, ws) = workspace("w");
        let got = start_args(
            Path::new("/bin/octet;"),
            Path::new("/data;"),
            &request(&ws),
            std::slice::from_ref(&ws),
            0,
            ("octet-abc123", "phone;"),
        )
        .unwrap();
        assert_eq!(got[16], "phone\\;");
        assert_eq!(got[6], "/bin/octet\\;");
        assert_eq!(got[8], "/data\\;");
    }

    #[test]
    fn launches_count_until_their_interface_registers_or_time_runs_out() {
        let start = Instant::now();
        let mut launches = Launches::default();
        launches.begin();
        // In flight counts, and so does an accepted launch that has not registered.
        assert_eq!(launches.running(0, start), 1);
        launches.accepted(start);
        assert_eq!(launches.running(0, start), 1);
        launches.begin();
        launches.failed();
        assert_eq!(launches.running(0, start), 1);
        // Its registration settles it: counted once, not twice.
        assert_eq!(launches.running(1, start), 1);
        assert_eq!(launches.running(1, start), 1);
        // Two more accepted: three in all; one registers, still three.
        launches.begin();
        launches.accepted(start);
        launches.begin();
        launches.accepted(start);
        assert_eq!(launches.running(1, start), 3);
        assert_eq!(launches.running(2, start), 3);
        // A session that ended does not settle the next launch.
        assert_eq!(launches.running(1, start), 2);
        // One that never registers stops counting after 30 seconds.
        let later = start + REGISTER_WITHIN;
        assert_eq!(launches.running(1, later), 1);
    }

    #[test]
    fn engines_tokens_modes_and_the_cap_are_checked() {
        let (_temp, ws) = workspace("w");
        let allowed = vec![ws.clone()];
        let mut r = request(&ws);

        r.engine = "gemini".into();
        assert!(args(&r, &allowed, 0).is_err());
        r.engine = "claude".into();
        assert!(args(&r, &allowed, 0).is_ok());

        r.resume = Some("abc-123".into());
        r.model = Some("opus".into());
        r.mode = Some("accept-edits".into());
        let got = args(&r, &allowed, 0).unwrap();
        assert_eq!(
            got[17..],
            [
                "--resume",
                "abc-123",
                "--model",
                "opus",
                "--mode",
                "accept-edits"
            ]
        );
        assert_eq!(got[12], "claude");

        for bad in [
            "", "a b", "a\tb", "a\nb", "a\u{7}b", "--mode", "-x", "a;b", "a#b",
        ] {
            for field in 0..2 {
                let mut r = request(&ws);
                match field {
                    0 => r.resume = Some(bad.into()),
                    _ => r.model = Some(bad.into()),
                }
                assert!(args(&r, &allowed, 0).is_err(), "{bad:?}");
            }
        }

        let mut r = request(&ws);
        r.mode = Some("yolo".into());
        assert!(args(&r, &allowed, 0).is_err());
        r.mode = Some("full-access".into());
        assert!(args(&r, &allowed, 0).is_err());
        r.confirm = Some("yes".into());
        assert!(args(&r, &allowed, 0).is_err());
        r.confirm = Some("full access".into());
        assert!(args(&r, &allowed, 0).is_ok());

        let r = request(&ws);
        assert!(args(&r, &allowed, PHONE_SESSIONS - 1).is_ok());
        assert_eq!(
            args(&r, &allowed, PHONE_SESSIONS).unwrap_err(),
            "8 sessions started from the phone are running; end one first"
        );
    }

    /// Starts a demo interface in real tmux and waits for its phone-started registration.
    /// Run it with `--ignored` after `cargo build -p octet`.
    #[tokio::test]
    #[ignore = "starts a real tmux session; needs tmux and a built octet"]
    async fn starts_a_real_demo_session() {
        let exe = std::env::current_exe().unwrap();
        let octet = exe.parent().and_then(Path::parent).unwrap().join("octet");
        assert!(octet.exists(), "build octet first: cargo build -p octet");
        let root = PathBuf::from(format!("/tmp/ohl{}", std::process::id()));
        let data = root.join("data");
        let work = root.join("w#S{x};");
        std::fs::create_dir_all(&work).unwrap();
        let work = std::fs::canonicalize(work).unwrap();
        let name = session_name().unwrap();
        let args = start_args(
            &octet,
            &data,
            &request(&work),
            std::slice::from_ref(&work),
            0,
            (&name, "Test phone"),
        )
        .unwrap();
        launch(args, &name).await.unwrap();
        let found = timeout(Duration::from_secs(20), async {
            loop {
                let registered = std::fs::read_dir(data.join("run")).ok().and_then(|dir| {
                    dir.flatten().find_map(|entry| {
                        let text = std::fs::read_to_string(entry.path()).ok()?;
                        text.contains("\"from_phone\":true").then_some(text)
                    })
                });
                if let Some(text) = registered {
                    return text;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        let cwd = std::process::Command::new("tmux")
            .args(["display-message", "-p", "-t", &name, "#{pane_current_path}"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &name])
            .status();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            cwd.unwrap(),
            work.to_string_lossy(),
            "the pane starts in the workspace"
        );
        let text = found.expect("the interface registers");
        assert!(text.contains("\"engine\":\"demo\""), "{text}");
    }
}
