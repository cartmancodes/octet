//! `octet hub install` and `uninstall`: the hub as a per-user launchd agent (macOS) or systemd
//! user unit (Linux), so it starts at login and comes back if it stops.
//!
//! Every process runs with literal argv, never a shell. Tests hand `Control::new` a stand-in for
//! `launchctl` / `systemctl` (receiving the same argv), so they never touch the user's session.
use crate::args::CliError;
use octet_hub::plain;
use std::{
    ffi::{OsStr, OsString},
    io::{Read as _, Write},
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

/// The launchd label, and the file's name without `.plist`.
const LABEL: &str = "dev.octet.hub";
/// The systemd unit's name.
const UNIT: &str = "octet-hub.service";
/// The tools the hub starts, in the order their directories lead the service's `PATH`.
const TOOLS: [&str; 4] = ["tmux", "claude", "codex", "opencode"];
/// Directories a bare launchd or systemd `PATH` lacks, after the ones the [`TOOLS`] were found in.
const DEFAULT_PATH: [&str; 4] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
/// How many times `launchctl bootstrap` is tried: it can fail just after a `bootout`.
const BOOTSTRAP_TRIES: u32 = 3;
const BOOTSTRAP_WAIT: Duration = Duration::from_millis(500);
/// Longest tool output quoted in an error, in characters.
const TOOL_WORDS: usize = 300;
/// How much of an existing file is read to tell whether it is ours.
const OWN_CHECK_BYTES: u64 = 64 * 1024;

fn failed(message: impl Into<String>) -> CliError {
    CliError::Run(message.into())
}

/// Which init system owns the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    /// The one this OS uses.
    pub(crate) fn native() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else {
            Self::Systemd
        }
    }

    /// What marks a file as Octet's.
    fn marker(self) -> &'static str {
        match self {
            Self::Launchd => LABEL,
            Self::Systemd => "octet-hub",
        }
    }
}

/// Everything the unit file says, from explicit inputs.
#[derive(Debug)]
pub(crate) struct ServicePlan {
    manager: Manager,
    home: PathBuf,
    program: PathBuf,
    data_dir: PathBuf,
    log: PathBuf,
    path: String,
}

impl ServicePlan {
    /// A plan to run `program hub --journal-dir DATA_DIR`, logging to `<data_dir>/hub.log` where the
    /// manager has a log file, with `lead` (where the [`TOOLS`] were found) first on its `PATH`.
    ///
    /// # Errors
    ///
    /// A path with a control character: no unit file can say it.
    pub(crate) fn new(
        manager: Manager,
        home: &Path,
        program: &Path,
        data_dir: &Path,
        lead: &[PathBuf],
    ) -> Result<Self, CliError> {
        let mut dirs: Vec<String> = Vec::new();
        for dir in lead
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .chain(DEFAULT_PATH.iter().map(|dir| (*dir).to_owned()))
        {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        let plan = Self {
            manager,
            home: home.to_owned(),
            program: program.to_owned(),
            data_dir: data_dir.to_owned(),
            log: data_dir.join("hub.log"),
            path: dirs.join(":"),
        };
        let unsayable = |text: &str| text.chars().any(char::is_control);
        let texts = plan.arguments().into_iter().chain([
            plan.log.to_string_lossy().into_owned(),
            plan.path.clone(),
            plan.home.to_string_lossy().into_owned(),
        ]);
        if texts.into_iter().any(|text| unsayable(&text)) {
            return Err(failed(
                "A path with a control character can't go in a service file",
            ));
        }
        Ok(plan)
    }

    /// The program and its arguments, as the service runs them.
    fn arguments(&self) -> Vec<String> {
        let mut argv = vec![
            self.program.to_string_lossy().into_owned(),
            "hub".to_owned(),
        ];
        // Always the directory this install resolved: the service has another environment, so
        // left to itself it could choose another directory than the one its log is in.
        argv.push("--journal-dir".to_owned());
        argv.push(self.data_dir.to_string_lossy().into_owned());
        argv
    }

    /// Where the unit file lives.
    pub(crate) fn unit_path(&self) -> PathBuf {
        match self.manager {
            Manager::Launchd => self
                .home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            Manager::Systemd => self.home.join(".config/systemd/user").join(UNIT),
        }
    }

    /// The unit file's text.
    pub(crate) fn text(&self) -> String {
        match self.manager {
            Manager::Launchd => self.plist(),
            Manager::Systemd => self.unit(),
        }
    }

    /// The launchd agent. `AbandonProcessGroup` keeps launchd from killing what the hub started
    /// (a tmux server a phone's `start` forked, and its sessions) when the agent stops.
    fn plist(&self) -> String {
        let mut arguments = String::new();
        for argument in self.arguments() {
            arguments.push_str("    <string>");
            arguments.push_str(&xml(&argument));
            arguments.push_str("</string>\n");
        }
        let log = xml(&self.log.to_string_lossy());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>AbandonProcessGroup</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
</dict>
</plist>
"#,
            path = xml(&self.path),
        )
    }

    /// The systemd user unit. `KillMode=process` stops the hub alone: the tmux server a phone's
    /// `start` forked lives in the unit's cgroup, and a stop or restart would otherwise kill it
    /// with every session in it.
    fn unit(&self) -> String {
        let exec = self
            .arguments()
            .iter()
            .map(|argument| systemd_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let environment = systemd_assignment(&format!("PATH={}", self.path));
        format!(
            r"# {UNIT}: written by `octet hub install`; `octet hub uninstall` removes it.
[Unit]
Description=Octet hub, your terminal sessions on your phone

[Service]
ExecStart={exec}
Restart=on-failure
RestartSec=5
KillMode=process
Environment={environment}

[Install]
WantedBy=default.target
"
        )
    }
}

/// `text` as XML character data.
fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One systemd command-line word: quoted if it holds anything systemd would split on or expand.
/// Inside quotes `\` and `"` are escaped; `%` (a specifier) and `$` (a variable) are doubled.
fn systemd_quote(word: &str) -> String {
    quote(word, true)
}

/// An `Environment=` assignment: as `systemd_quote`, except `$` stays, for systemd doesn't expand
/// variables there.
fn systemd_assignment(assignment: &str) -> String {
    quote(assignment, false)
}

fn quote(word: &str, expands_dollar: bool) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@=+,-".contains(c));
    if plain {
        return word.to_owned();
    }
    let mut out = String::from("\"");
    for c in word.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' if expands_dollar => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The directory holding an executable `name` on `search` (a `PATH` value), if any. Relative
/// entries are skipped: the service would resolve them against its own directory.
pub(crate) fn find_on_path(search: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(search)
        .filter(|dir| dir.is_absolute())
        .find(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The directories on `search` where `tmux`, `claude`, `codex` and `opencode` were found, in that
/// order, each once; a tool that isn't there adds nothing. The service's own `PATH` lacks the
/// directories a shell adds (`claude` is often in `~/.local/bin`, `opencode` in `~/.opencode/bin`),
/// so a session started from the phone needs these.
pub(crate) fn tool_dirs(search: &OsStr) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in TOOLS.iter().filter_map(|tool| find_on_path(search, tool)) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The program that loads and unloads the unit: `launchctl` or `systemctl`, or a test's stand-in.
#[derive(Debug)]
pub(crate) struct Control {
    program: PathBuf,
    /// The user id for `launchctl gui/<uid>`; asked of `id -u` when first needed if `None`.
    uid: Option<String>,
    /// What runs `id -u`.
    id_program: PathBuf,
    /// How long to wait between `launchctl bootstrap` tries.
    retry_wait: Duration,
}

impl Control {
    /// A controller that runs `program`, for the user id `uid`.
    #[cfg(test)]
    pub(crate) fn new(program: PathBuf, uid: &str) -> Self {
        Self {
            program,
            uid: Some(uid.into()),
            id_program: PathBuf::from("id"),
            retry_wait: Duration::ZERO,
        }
    }

    /// The same controller, finding its user id with `id_program` instead of `id`.
    #[cfg(test)]
    pub(crate) fn finding_the_uid_with(mut self, id_program: PathBuf) -> Self {
        self.uid = None;
        self.id_program = id_program;
        self
    }

    /// The real tool for `manager`. `launchctl` is always `/bin/launchctl`; where `systemctl` is
    /// varies by distribution, so it is looked up on `PATH`. Tests build a [`Control`] with
    /// `Control::new` instead: nothing in the environment can swap the tool.
    pub(crate) fn native(manager: Manager) -> Self {
        Self {
            program: PathBuf::from(match manager {
                Manager::Launchd => "/bin/launchctl",
                Manager::Systemd => "systemctl",
            }),
            uid: None,
            id_program: PathBuf::from("id"),
            retry_wait: BOOTSTRAP_WAIT,
        }
    }

    fn user_id(&self) -> Result<String, CliError> {
        if let Some(uid) = &self.uid {
            return Ok(uid.clone());
        }
        let output = Command::new(&self.id_program)
            .arg("-u")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| failed(format!("Couldn't run id -u: {e}")))?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() && !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
            Ok(uid)
        } else {
            Err(failed("Couldn't find your user id with id -u"))
        }
    }

    /// Runs the tool with `args`; its own words on failure.
    fn run(&self, args: &[OsString]) -> Result<(), CliError> {
        let shown = || {
            let name = self
                .program
                .file_name()
                .unwrap_or(self.program.as_os_str())
                .to_string_lossy()
                .into_owned();
            let words: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
            plain(&format!("{name} {}", words.join(" ")), TOOL_WORDS)
        };
        let output = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| failed(format!("Couldn't run {}: {e}", self.program.display())))?;
        if output.status.success() {
            return Ok(());
        }
        let status = output
            .status
            .code()
            .map_or_else(|| "a signal".to_owned(), |code| format!("exit {code}"));
        let said = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        let said = plain(said.trim(), TOOL_WORDS);
        Err(failed(if said.is_empty() {
            format!("{} failed ({status})", shown())
        } else {
            format!("{} failed ({status}): {said}", shown())
        }))
    }
}

fn args<const N: usize>(list: [&OsStr; N]) -> Vec<OsString> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Whether the file at `path` is ours.
enum Existing {
    Absent,
    Ours,
    Foreign,
}

fn existing(plan: &ServicePlan) -> Existing {
    let path = plan.unit_path();
    let mut head = Vec::new();
    let read = std::fs::File::open(&path)
        .and_then(|file| file.take(OWN_CHECK_BYTES).read_to_end(&mut head));
    match read {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Existing::Absent,
        Ok(_) if String::from_utf8_lossy(&head).contains(plan.manager.marker()) => Existing::Ours,
        Ok(_) | Err(_) => Existing::Foreign,
    }
}

fn foreign(plan: &ServicePlan) -> CliError {
    failed(format!(
        "{} exists and isn't Octet's, so it was left alone",
        plan.unit_path().display()
    ))
}

/// Writes `text` to `path` (mode 0644, creating its directory) by renaming a sibling into place.
fn write_unit(path: &Path, text: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().unwrap_or(OsStr::new("unit"));
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(".tmp");
    let temporary = parent.join(temporary);
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temporary)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.set_permissions(std::fs::Permissions::from_mode(0o644)))
        .and_then(|()| file.sync_all());
    drop(file);
    written
        .and_then(|()| std::fs::rename(&temporary, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
}

/// Installs the service and starts it; with `print`, only writes the unit to `out`.
///
/// # Errors
///
/// A file at the unit's path that isn't Octet's, a write that fails, or the tool refusing.
pub(crate) fn install(
    plan: &ServicePlan,
    control: &Control,
    print: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let say = |e: std::io::Error| failed(format!("Couldn't write to the terminal: {e}"));
    if print {
        return out.write_all(plan.text().as_bytes()).map_err(say);
    }
    if matches!(existing(plan), Existing::Foreign) {
        return Err(foreign(plan));
    }
    let path = plan.unit_path();
    // Everything that can fail before a file exists comes first, so a failure leaves nothing.
    let domain = match plan.manager {
        Manager::Launchd => Some(OsString::from(format!("gui/{}", control.user_id()?))),
        Manager::Systemd => None,
    };
    if let Some(parent) = plan.log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| failed(format!("Couldn't create {}: {e}", parent.display())))?;
    }
    write_unit(&path, &plan.text())
        .map_err(|e| failed(format!("Couldn't write {}: {e}", path.display())))?;
    if let Some(domain) = domain {
        let plist = path.clone().into_os_string();
        // A copy that is already loaded has to go first; it may not be.
        let _ = control.run(&args([OsStr::new("bootout"), &domain, &plist]));
        let mut tries = 1;
        while let Err(error) = control.run(&args([OsStr::new("bootstrap"), &domain, &plist])) {
            if tries >= BOOTSTRAP_TRIES {
                return Err(error);
            }
            tries += 1;
            std::thread::sleep(control.retry_wait);
        }
    } else {
        control.run(&args([OsStr::new("--user"), OsStr::new("daemon-reload")]))?;
        control.run(&args([
            OsStr::new("--user"),
            OsStr::new("enable"),
            OsStr::new("--now"),
            OsStr::new(UNIT),
        ]))?;
        // `enable --now` leaves a running unit alone; a changed program needs a restart.
        control.run(&args([
            OsStr::new("--user"),
            OsStr::new("restart"),
            OsStr::new(UNIT),
        ]))?;
    }
    let (log, restarts) = match plan.manager {
        Manager::Launchd => (plan.log.display().to_string(), "stops"),
        Manager::Systemd => (format!("journalctl --user -u {UNIT}"), "fails"),
    };
    writeln!(
        out,
        "Installed {}\nThe hub starts at login and restarts if it {restarts}.\nLog: {log}\nRemove it with: octet hub uninstall",
        path.display()
    )
    .map_err(say)
}

/// Stops the service and removes its file; "Not installed" when there is none.
///
/// # Errors
///
/// A file at the unit's path that isn't Octet's, or a file that can't be removed.
pub(crate) fn uninstall(
    plan: &ServicePlan,
    control: &Control,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let say = |e: std::io::Error| failed(format!("Couldn't write to the terminal: {e}"));
    match existing(plan) {
        Existing::Absent => return writeln!(out, "Not installed").map_err(say),
        Existing::Foreign => return Err(foreign(plan)),
        Existing::Ours => {}
    }
    let path = plan.unit_path();
    // Stopping a service that isn't running is not an error here.
    match plan.manager {
        Manager::Launchd => {
            let domain = OsString::from(format!("gui/{}", control.user_id()?));
            let _ = control.run(&args([OsStr::new("bootout"), &domain, path.as_os_str()]));
        }
        Manager::Systemd => {
            let _ = control.run(&args([
                OsStr::new("--user"),
                OsStr::new("disable"),
                OsStr::new("--now"),
                OsStr::new(UNIT),
            ]));
        }
    }
    std::fs::remove_file(&path)
        .map_err(|e| failed(format!("Couldn't remove {}: {e}", path.display())))?;
    writeln!(out, "Removed {}", path.display()).map_err(say)?;
    if plan.manager == Manager::Systemd {
        // The file is already gone, so a failure here is the user's to know about, not a failure.
        if let Err(error) = control.run(&args([OsStr::new("--user"), OsStr::new("daemon-reload")]))
        {
            writeln!(out, "Warning: {error}").map_err(say)?;
        }
    }
    Ok(())
}

/// `octet hub install` for this user: the running `octet`, serving from `data_dir`.
///
/// # Errors
///
/// No home directory, or what [`install`] reports.
pub(crate) fn install_here(data_dir: &Path, print: bool) -> Result<(), CliError> {
    let manager = Manager::native();
    let plan = here(manager, data_dir)?;
    install(
        &plan,
        &Control::native(manager),
        print,
        &mut std::io::stdout(),
    )
}

/// `octet hub uninstall` for this user.
///
/// # Errors
///
/// No home directory, or what [`uninstall`] reports.
pub(crate) fn uninstall_here(data_dir: &Path) -> Result<(), CliError> {
    let manager = Manager::native();
    let plan = here(manager, data_dir)?;
    uninstall(&plan, &Control::native(manager), &mut std::io::stdout())
}

fn here(manager: Manager, data_dir: &Path) -> Result<ServicePlan, CliError> {
    let home =
        octet_core::home_dir().ok_or_else(|| failed("Set HOME to find your home directory"))?;
    let program = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| failed(format!("Couldn't find the octet program: {e}")))?;
    // The service starts with another working directory, so a relative path would point elsewhere.
    let absolute = |path: &Path| {
        std::path::absolute(path)
            .map_err(|e| failed(format!("Couldn't resolve {}: {e}", path.display())))
    };
    let data_dir = absolute(data_dir)?;
    let lead = std::env::var_os("PATH").map_or_else(Vec::new, |search| tool_dirs(&search));
    ServicePlan::new(manager, &home, &program, &data_dir, &lead)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::{TempDir, write_script};

    const DEFAULTS: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

    /// A temp HOME, a stand-in `launchctl`/`systemctl` that records its argv, and a plan on it.
    struct Rig {
        _dir: TempDir,
        home: PathBuf,
        data: PathBuf,
        calls: PathBuf,
        control: Control,
    }

    impl Rig {
        fn new(stand_in: &str) -> Self {
            let dir = TempDir::new("octet-service");
            let home = dir.path().join("home");
            let data = dir.path().join("data");
            std::fs::create_dir_all(&home).unwrap();
            let calls = dir.path().join("calls.txt");
            let script = write_script(
                &dir.path().join("bin"),
                "ctl",
                &format!("printf '%s\\n' \"$*\" >> '{}'\n{stand_in}", calls.display()),
            );
            Self {
                control: Control::new(script, "501"),
                _dir: dir,
                home,
                data,
                calls,
            }
        }
        /// A plan for `data_dir`, or the rig's own.
        fn plan(&self, manager: Manager, data_dir: Option<&Path>) -> ServicePlan {
            ServicePlan::new(
                manager,
                &self.home,
                Path::new("/opt/octet/bin/octet"),
                data_dir.unwrap_or(&self.data),
                &[],
            )
            .unwrap()
        }
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.calls)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn run_install(rig: &Rig, plan: &ServicePlan, print: bool) -> (Result<(), CliError>, String) {
        let mut out = Vec::new();
        let result = install(plan, &rig.control, print, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    fn run_uninstall(rig: &Rig, plan: &ServicePlan) -> (Result<(), CliError>, String) {
        let mut out = Vec::new();
        let result = uninstall(plan, &rig.control, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_launchd_agent_finds_tmux() {
        let dir = TempDir::new("octet-service-tmux");
        let bin = dir.path().join("bin");
        write_script(&bin, "tmux", "exit 0");
        let search = std::env::join_paths([bin.as_path(), Path::new("/usr/bin")]).unwrap();
        let found = find_on_path(&search, "tmux");
        assert_eq!(found.as_deref(), Some(bin.as_path()));
        let rig = Rig::new("");
        let plan = ServicePlan::new(
            Manager::Launchd,
            &rig.home,
            Path::new("/opt/octet/bin/octet"),
            &rig.data,
            &found.into_iter().collect::<Vec<_>>(),
        )
        .unwrap();
        let expected = format!("{}:{DEFAULTS}", bin.display());
        let text = plan.text();
        assert!(
            text.contains(&format!("<key>PATH</key>\n    <string>{expected}</string>")),
            "{text}"
        );
    }

    #[test]
    fn the_service_finds_the_vendor_clis() {
        let dir = TempDir::new("octet-service-clis");
        let brew = dir.path().join("brew");
        let local = dir.path().join("local");
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        write_script(&brew, "tmux", "exit 0");
        write_script(&local, "claude", "exit 0");
        write_script(&brew, "codex", "exit 0");
        // opencode's installer puts it in its own `~/.opencode/bin`.
        let opencode = dir.path().join(".opencode").join("bin");
        write_script(&opencode, "opencode", "exit 0");
        let search = std::env::join_paths([
            empty.as_path(),
            opencode.as_path(),
            local.as_path(),
            brew.as_path(),
        ])
        .unwrap();
        // tmux, claude, codex, opencode in that order, each directory once.
        let found = tool_dirs(&search);
        assert_eq!(found, [brew.clone(), local.clone(), opencode.clone()]);
        let rig = Rig::new("");
        let plan = ServicePlan::new(
            Manager::Launchd,
            &rig.home,
            Path::new("/opt/octet/bin/octet"),
            &rig.data,
            &found,
        )
        .unwrap();
        let expected = format!(
            "{}:{}:{}:{DEFAULTS}",
            brew.display(),
            local.display(),
            opencode.display()
        );
        let text = plan.text();
        assert!(
            text.contains(&format!("<key>PATH</key>\n    <string>{expected}</string>")),
            "{text}"
        );
        // A tool that isn't installed adds nothing.
        let only_claude = std::env::join_paths([local.as_path()]).unwrap();
        assert_eq!(tool_dirs(&only_claude), [local]);
        assert!(tool_dirs(OsStr::new("")).is_empty());
    }

    #[test]
    fn the_real_tools_are_fixed_and_not_taken_from_the_environment() {
        assert_eq!(
            Control::native(Manager::Launchd).program,
            Path::new("/bin/launchctl")
        );
        // Where systemctl lives varies by distribution, so it is looked up on PATH.
        assert_eq!(
            Control::native(Manager::Systemd).program,
            Path::new("systemctl")
        );
    }

    #[test]
    fn tmux_is_found_only_as_an_absolute_executable_file() {
        let dir = TempDir::new("octet-service-tmux");
        let plain = dir.path().join("plain");
        let real = dir.path().join("real");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("tmux"), "not executable").unwrap();
        std::fs::set_permissions(plain.join("tmux"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        write_script(&real, "tmux", "exit 0");
        std::fs::create_dir_all(real.join("sub").join("tmux")).unwrap();
        let search = std::env::join_paths([
            Path::new(""),
            Path::new("relative"),
            plain.as_path(),
            real.join("sub").as_path(),
            real.as_path(),
        ])
        .unwrap();
        assert_eq!(
            find_on_path(&search, "tmux").as_deref(),
            Some(real.as_path())
        );
        assert_eq!(find_on_path(OsStr::new(""), "tmux"), None);
    }

    #[test]
    fn the_launchd_plist_runs_the_hub_and_logs_to_hub_log() {
        let rig = Rig::new("");
        let data = Path::new("/srv/a & <b>");
        let plan = rig.plan(Manager::Launchd, Some(data));
        assert_eq!(
            plan.unit_path(),
            rig.home.join("Library/LaunchAgents/dev.octet.hub.plist")
        );
        let text = plan.text();
        let log = xml(&data.join("hub.log").to_string_lossy());
        for needle in [
            "<key>Label</key>\n  <string>dev.octet.hub</string>",
            "<key>ProgramArguments</key>",
            "<string>/opt/octet/bin/octet</string>\n    <string>hub</string>",
            // The data directory the installing shell resolved, so the hub and its log agree.
            "<string>--journal-dir</string>\n    <string>/srv/a &amp; &lt;b&gt;</string>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>KeepAlive</key>\n  <true/>",
            // Stopping the agent leaves the tmux server a phone's `start` forked, and its sessions.
            "<key>AbandonProcessGroup</key>\n  <true/>",
            &format!("<key>StandardOutPath</key>\n  <string>{log}</string>"),
            &format!("<key>StandardErrorPath</key>\n  <string>{log}</string>"),
            &format!("<string>{DEFAULTS}</string>"),
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
    }

    #[test]
    fn the_data_directory_is_always_carried_into_the_service() {
        let rig = Rig::new("");
        let data = rig.data.display();
        let launchd = rig.plan(Manager::Launchd, None).text();
        assert!(
            launchd.contains(&format!(
                "<string>--journal-dir</string>\n    <string>{data}</string>"
            )),
            "{launchd}"
        );
        let systemd = rig.plan(Manager::Systemd, None).text();
        assert!(
            systemd.contains(&format!("octet hub --journal-dir {data}\n")),
            "{systemd}"
        );
    }

    #[test]
    fn the_systemd_unit_quotes_what_systemd_would_split_or_expand() {
        let rig = Rig::new("");
        let plain = rig.plan(Manager::Systemd, None);
        assert_eq!(
            plain.unit_path(),
            rig.home.join(".config/systemd/user/octet-hub.service")
        );
        let text = plain.text();
        for needle in [
            "ExecStart=/opt/octet/bin/octet hub --journal-dir ",
            // Without a wait, a hub failing at boot exhausts systemd's start limit and stays down.
            "Restart=on-failure\nRestartSec=5\n",
            // A stop or restart ends the hub only, never the tmux server a phone's `start` forked
            // into the unit's cgroup, with every session in it.
            "KillMode=process\n",
            &format!("Environment=PATH={DEFAULTS}\n"),
            "WantedBy=default.target\n",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        let spaced = rig.plan(Manager::Systemd, Some(Path::new("/my dir/50%/a\"b\\c$d")));
        assert!(
            spaced.text().contains(
                "ExecStart=/opt/octet/bin/octet hub --journal-dir \"/my dir/50%%/a\\\"b\\\\c$$d\"\n"
            ),
            "{}",
            spaced.text()
        );
        let odd = ServicePlan::new(
            Manager::Systemd,
            &rig.home,
            Path::new("/Users/a b/octet"),
            Path::new("/d"),
            &[PathBuf::from("/tmux dir")],
        )
        .unwrap();
        let text = odd.text();
        assert!(
            text.contains("ExecStart=\"/Users/a b/octet\" hub --journal-dir /d\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!("Environment=\"PATH=/tmux dir:{DEFAULTS}\"\n")),
            "{text}"
        );
    }

    #[test]
    fn the_path_has_no_duplicates() {
        let rig = Rig::new("");
        let plan = ServicePlan::new(
            Manager::Systemd,
            &rig.home,
            Path::new("/opt/octet/bin/octet"),
            &rig.data,
            &[PathBuf::from("/usr/local/bin")],
        )
        .unwrap();
        assert!(
            plan.text()
                .contains("Environment=PATH=/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin\n"),
            "{}",
            plan.text()
        );
    }

    #[test]
    fn a_control_character_in_a_path_is_refused() {
        let rig = Rig::new("");
        let bad = ServicePlan::new(
            Manager::Launchd,
            &rig.home,
            Path::new("/opt/octet\n/octet"),
            &rig.data,
            &[],
        );
        assert!(matches!(bad, Err(CliError::Run(_))));
    }

    #[test]
    fn launchd_install_then_uninstall_leaves_no_file() {
        let rig = Rig::new("");
        let plan = rig.plan(Manager::Launchd, None);
        let plist = plan.unit_path();
        let (result, out) = run_install(&rig, &plan, false);
        result.unwrap();
        assert!(out.contains("dev.octet.hub.plist"), "{out}");
        assert!(out.contains("hub.log"), "{out}");
        assert_eq!(std::fs::read_to_string(&plist).unwrap(), plan.text());
        assert_eq!(
            std::fs::metadata(&plist).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(rig.data.is_dir(), "the log's directory exists");
        let target = format!("gui/501 {}", plist.display());
        assert_eq!(
            rig.calls(),
            [format!("bootout {target}"), format!("bootstrap {target}")]
        );
        let (result, out) = run_uninstall(&rig, &plan);
        result.unwrap();
        assert!(!plist.exists());
        assert!(out.contains("Removed"), "{out}");
        assert_eq!(rig.calls()[2..], [format!("bootout {target}")]);
    }

    #[test]
    fn systemd_install_then_uninstall_leaves_no_file() {
        let rig = Rig::new("");
        let plan = rig.plan(Manager::Systemd, None);
        let unit = plan.unit_path();
        let (result, out) = run_install(&rig, &plan, false);
        result.unwrap();
        assert!(out.contains("journalctl --user -u octet-hub"), "{out}");
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), plan.text());
        assert_eq!(
            std::fs::metadata(&unit).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            rig.calls(),
            [
                "--user daemon-reload",
                "--user enable --now octet-hub.service",
                "--user restart octet-hub.service"
            ]
        );
        let (result, _) = run_uninstall(&rig, &plan);
        result.unwrap();
        assert!(!unit.exists());
        assert_eq!(
            rig.calls()[3..],
            [
                "--user disable --now octet-hub.service",
                "--user daemon-reload"
            ]
        );
    }

    #[test]
    fn a_second_uninstall_says_not_installed() {
        for manager in [Manager::Launchd, Manager::Systemd] {
            let rig = Rig::new("");
            let plan = rig.plan(manager, None);
            run_install(&rig, &plan, false).0.unwrap();
            run_uninstall(&rig, &plan).0.unwrap();
            let before = rig.calls().len();
            let (result, out) = run_uninstall(&rig, &plan);
            result.unwrap();
            assert_eq!(out, "Not installed\n");
            assert_eq!(rig.calls().len(), before, "nothing was run");
        }
    }

    #[test]
    fn print_shows_the_unit_and_changes_nothing() {
        for manager in [Manager::Launchd, Manager::Systemd] {
            let rig = Rig::new("");
            let plan = rig.plan(manager, None);
            let (result, out) = run_install(&rig, &plan, true);
            result.unwrap();
            assert_eq!(out, plan.text());
            assert!(!plan.unit_path().exists());
            assert!(!rig.home.join("Library").exists());
            assert!(!rig.home.join(".config").exists());
            assert!(!rig.data.exists());
            assert!(rig.calls().is_empty());
        }
    }

    #[test]
    fn a_foreign_file_is_refused() {
        for manager in [Manager::Launchd, Manager::Systemd] {
            let rig = Rig::new("");
            let plan = rig.plan(manager, None);
            let path = plan.unit_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "somebody else's unit\n").unwrap();
            let (result, _) = run_install(&rig, &plan, false);
            assert!(
                matches!(&result, Err(CliError::Run(m)) if m.contains(&path.display().to_string())),
                "{result:?}"
            );
            let (result, _) = run_uninstall(&rig, &plan);
            assert!(matches!(result, Err(CliError::Run(_))), "{result:?}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "somebody else's unit\n"
            );
            assert!(rig.calls().is_empty());
        }
    }

    #[test]
    fn installing_again_replaces_our_own_file() {
        let rig = Rig::new("");
        run_install(&rig, &rig.plan(Manager::Systemd, None), false)
            .0
            .unwrap();
        let elsewhere = rig.home.join("elsewhere");
        let again = rig.plan(Manager::Systemd, Some(&elsewhere));
        run_install(&rig, &again, false).0.unwrap();
        let text = std::fs::read_to_string(again.unit_path()).unwrap();
        assert!(
            text.contains(&format!("--journal-dir {}", elsewhere.display())),
            "{text}"
        );
    }

    #[test]
    fn a_failing_load_is_reported_with_the_tools_words() {
        let rig = Rig::new("echo 'no such domain' >&2\nexit 5");
        let plan = rig.plan(Manager::Systemd, None);
        let (result, _) = run_install(&rig, &plan, false);
        let Err(CliError::Run(message)) = result else {
            panic!("expected a failure");
        };
        assert!(message.contains("daemon-reload"), "{message}");
        assert!(message.contains("no such domain"), "{message}");
        assert!(message.contains('5'), "{message}");
    }

    #[test]
    fn a_missing_tool_is_reported() {
        let rig = Rig::new("");
        let control = Control::new(rig.home.join("nonexistent"), "501");
        let plan = rig.plan(Manager::Systemd, None);
        let result = install(&plan, &control, false, &mut Vec::new());
        assert!(matches!(&result, Err(CliError::Run(m)) if m.contains("nonexistent")));
    }

    /// A stand-in that fails the way `fail` (a shell `case` pattern on its first argument) says.
    fn failing(rig: &Rig, fail: &str, code: i32) -> Control {
        let script = write_script(
            &rig.home.join("bin2"),
            "ctl",
            &format!(
                "printf '%s\\n' \"$*\" >> '{calls}'\ncase \"$*\" in {fail}) echo 'refused' >&2; exit {code};; esac",
                calls = rig.calls.display(),
            ),
        );
        Control::new(script, "501")
    }

    #[test]
    fn a_failing_bootout_is_ignored() {
        let rig = Rig::new("");
        let plan = rig.plan(Manager::Launchd, None);
        let control = failing(&rig, "bootout*", 3);
        install(&plan, &control, false, &mut Vec::new()).unwrap();
        assert_eq!(rig.calls().len(), 2, "{:?}", rig.calls());
        uninstall(&plan, &control, &mut Vec::new()).unwrap();
        assert!(!plan.unit_path().exists());
    }

    #[test]
    fn bootstrap_is_tried_again_and_can_succeed() {
        let count = "n=$(cat \"$0.count\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$0.count\"\n\
            if [ \"$1\" = bootstrap ] && [ \"$n\" -lt 3 ]; then echo 'Input/output error' >&2; exit 5; fi";
        let rig = Rig::new(count);
        let plan = rig.plan(Manager::Launchd, None);
        run_install(&rig, &plan, false).0.unwrap();
        let verbs: Vec<_> = rig
            .calls()
            .iter()
            .map(|call| call.split(' ').next().unwrap().to_owned())
            .collect();
        assert_eq!(verbs, ["bootout", "bootstrap", "bootstrap"]);
    }

    #[test]
    fn bootstrap_gives_up_after_three_tries() {
        let rig = Rig::new("[ \"$1\" = bootstrap ] && { echo 'Input/output error' >&2; exit 5; }");
        let plan = rig.plan(Manager::Launchd, None);
        let (result, _) = run_install(&rig, &plan, false);
        let Err(CliError::Run(message)) = result else {
            panic!("expected a failure");
        };
        assert!(message.contains("Input/output error"), "{message}");
        assert_eq!(rig.calls().len(), 4, "a bootout and three bootstraps");
    }

    #[test]
    fn no_uid_means_nothing_is_written() {
        let rig = Rig::new("");
        let control = Control::new(rig.home.join("ctl"), "501")
            .finding_the_uid_with(rig.home.join("no-such-id"));
        let plan = rig.plan(Manager::Launchd, None);
        let result = install(&plan, &control, false, &mut Vec::new());
        assert!(
            matches!(&result, Err(CliError::Run(m)) if m.contains("id -u")),
            "{result:?}"
        );
        assert!(!rig.home.join("Library").exists());
        assert!(!rig.data.exists());
        assert!(rig.calls().is_empty());
    }

    #[test]
    fn a_failing_daemon_reload_after_removal_is_a_warning() {
        let rig = Rig::new("");
        let plan = rig.plan(Manager::Systemd, None);
        run_install(&rig, &plan, false).0.unwrap();
        let control = failing(&rig, "--user daemon-reload", 1);
        let mut out = Vec::new();
        uninstall(&plan, &control, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!plan.unit_path().exists());
        assert!(out.contains("Removed"), "{out}");
        assert!(
            out.contains("Warning") && out.contains("daemon-reload"),
            "{out}"
        );
    }

    #[test]
    fn the_install_message_says_what_restarts_it() {
        let rig = Rig::new("");
        let (_, launchd) = run_install(&rig, &rig.plan(Manager::Launchd, None), false);
        assert!(launchd.contains("restarts if it stops"), "{launchd}");
        let (_, systemd) = run_install(&rig, &rig.plan(Manager::Systemd, None), false);
        assert!(systemd.contains("restarts if it fails"), "{systemd}");
    }

    #[test]
    fn environment_is_quoted_without_doubling_dollars() {
        let rig = Rig::new("");
        let plan = ServicePlan::new(
            Manager::Systemd,
            &rig.home,
            Path::new("/opt/octet/bin/octet"),
            &rig.data,
            &[PathBuf::from("/t$x/50%/a\"b")],
        )
        .unwrap();
        let text = plan.text();
        assert!(
            text.contains(&format!(
                "Environment=\"PATH=/t$x/50%%/a\\\"b:{DEFAULTS}\"\n"
            )),
            "{text}"
        );
    }
}
