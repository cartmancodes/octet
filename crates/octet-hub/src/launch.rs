//! Starting terminal UIs in tmux for a phone, and listing the sessions it may resume.
//!
//! Everything a phone chooses (engine, workspace, resume id, model, mode) reaches tmux as one
//! literal argument each: no shell, no joined strings. Workspaces are checked against an
//! allow-list of canonical directories.
use octet_core::{
    Engine, Mode, recent_sessions,
    remote::{known_workspaces, protocol::full_access_confirmed},
};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

/// Sessions started from the phone that may run at once.
pub(crate) const PHONE_SESSIONS: usize = 8;
/// How long tmux gets to start a session.
const LAUNCH_WITHIN: Duration = Duration::from_secs(5);
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

/// `value` as one plain argument: not empty, no whitespace or control characters, and not
/// shaped like an option.
fn token(what: &str, value: &str) -> Result<(), String> {
    let plain = !value.is_empty()
        && !value.starts_with('-')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control());
    if plain {
        Ok(())
    } else {
        Err(format!("Invalid {what}"))
    }
}

/// The canonical form of `requested` if it is one of the `allowed` directories.
///
/// # Errors
///
/// "No such workspace" when it cannot be resolved, and "Workspace not allowed" when it is not
/// in the list.
pub(crate) fn check_workspace(requested: &Path, allowed: &[PathBuf]) -> Result<PathBuf, String> {
    let real = std::fs::canonicalize(requested).map_err(|_| "No such workspace".to_owned())?;
    if allowed.contains(&real) {
        return Ok(real);
    }
    let shown = requested.to_string_lossy();
    Err(format!(
        "Workspace not allowed: {}",
        clip(&shown, ECHOED_PATH)
    ))
}

/// The tmux arguments for a phone-started interface; Err when the request is not allowed.
///
/// `allowed` holds canonical directories, `running` is how many phone-started sessions exist,
/// and `name` is the tmux session's name. Each choice of the phone is one argument.
pub(crate) fn start_args(
    octet: &Path,
    directory: &Path,
    request: &StartRequest,
    allowed: &[PathBuf],
    running: usize,
    name: &str,
) -> Result<Vec<OsString>, String> {
    if running >= PHONE_SESSIONS {
        return Err(format!(
            "{PHONE_SESSIONS} sessions started from the phone are running; end one first"
        ));
    }
    let engine = Engine::parse(&request.engine)
        .ok_or_else(|| format!("Unknown engine {}", clip(&request.engine, 32)))?;
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
        Some(text) => {
            Some(Mode::parse(text).ok_or_else(|| format!("Unknown mode {}", clip(text, 32)))?)
        }
        None => None,
    };
    if let Some(mode) = mode
        && !full_access_confirmed(mode.label(), request.confirm.as_deref())
    {
        return Err("Full access needs confirm: \"full access\"".into());
    }
    let workspace = check_workspace(&request.workspace, allowed)?;
    let mut args: Vec<OsString> = ["new-session", "-d", "-s", name, "-c"]
        .map(OsString::from)
        .into();
    args.push(workspace.into_os_string());
    args.push(octet.as_os_str().to_owned());
    args.push("--journal-dir".into());
    args.push(directory.as_os_str().to_owned());
    args.push("--engine".into());
    args.push(engine.as_str().into());
    args.push("--remote-started".into());
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
pub(crate) fn session_name() -> String {
    let mut bytes = [0_u8; 3];
    getrandom::fill(&mut bytes).expect("the operating system gave no random bytes");
    format!("octet-{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

/// Runs tmux with `args`, giving it 5 seconds.
///
/// # Errors
///
/// Why it did not start a session: tmux is missing, it ran out of time, or what it said on
/// stderr (clipped).
pub(crate) async fn launch(args: Vec<OsString>) -> Result<(), String> {
    let child = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "tmux isn't installed".to_owned(),
            _ => format!("Couldn't run tmux: {e}"),
        })?;
    let output = timeout(LAUNCH_WITHIN, child.wait_with_output())
        .await
        .map_err(|_| "tmux didn't answer in 5 seconds".to_owned())?
        .map_err(|e| format!("Couldn't run tmux: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = said.trim();
    Err(if said.is_empty() {
        "tmux couldn't start the session".to_owned()
    } else {
        clip(said, TMUX_ERROR)
    })
}

/// A session tmux started.
#[derive(Debug)]
pub(crate) struct Started {
    /// The tmux session's name.
    pub name: String,
    /// The canonical workspace it runs in.
    pub workspace: PathBuf,
}

/// Where the canonical workspace sits in [`start_args`]'s result.
const WORKSPACE_ARG: usize = 5;

/// Checks `request` against the allowed workspaces and starts it in a new tmux session.
///
/// # Errors
///
/// Why the session was not started: the request is not allowed, or tmux failed.
pub(crate) async fn start(
    directory: &Path,
    configured: &[PathBuf],
    request: &StartRequest,
    running: usize,
) -> Result<Started, String> {
    let allowed = allowed_workspaces(directory, configured).await;
    let octet = std::env::current_exe().map_err(|e| format!("Couldn't find octet: {e}"))?;
    let name = session_name();
    let args = start_args(&octet, directory, request, &allowed, running, &name)?;
    let workspace = args
        .get(WORKSPACE_ARG)
        .map(PathBuf::from)
        .unwrap_or_default();
    launch(args).await?;
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
) -> Result<Vec<Value>, String> {
    let workspaces = match workspace {
        Some(requested) => vec![check_workspace(requested, allowed)?],
        None => allowed.to_vec(),
    };
    let mut found = Vec::new();
    for workspace in &workspaces {
        for session in recent_sessions(directory, workspace, PER_WORKSPACE).await {
            found.push((workspace, session));
        }
    }
    found.sort_by_key(|(_, session)| std::cmp::Reverse(session.summary.started));
    found.truncate(if workspace.is_some() {
        PER_WORKSPACE
    } else {
        IN_TOTAL
    });
    Ok(found
        .into_iter()
        .map(|(workspace, session)| {
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
                "workspace": workspace.to_string_lossy(),
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
            "octet-abc123",
        )
        .map(|args| {
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        })
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
                "--engine",
                "demo",
                "--remote-started",
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
            got[12..],
            [
                "--resume",
                "abc-123",
                "--model",
                "opus",
                "--mode",
                "accept-edits"
            ]
        );
        assert_eq!(got[10], "claude");

        for bad in ["", "a b", "a\tb", "a\nb", "a\u{7}b", "--mode", "-x"] {
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
        let work = root.join("w");
        std::fs::create_dir_all(&work).unwrap();
        let work = std::fs::canonicalize(work).unwrap();
        let name = session_name();
        let args = start_args(
            &octet,
            &data,
            &request(&work),
            std::slice::from_ref(&work),
            0,
            &name,
        )
        .unwrap();
        launch(args).await.unwrap();
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
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &name])
            .status();
        let _ = std::fs::remove_dir_all(&root);
        let text = found.expect("the interface registers");
        assert!(text.contains("\"engine\":\"demo\""), "{text}");
    }
}
