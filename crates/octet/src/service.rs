//! `octet hub install` and `uninstall`: the hub as a per-user launchd agent (macOS) or systemd
//! user unit (Linux), so it starts at login and comes back if it stops.
//!
//! Every process runs with literal argv, never a shell. The hidden `OCTET_SERVICE_CTL` replaces
//! `launchctl` / `systemctl` (receiving the same argv), so tests never touch the user's session.
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
/// Directories a bare launchd or systemd `PATH` lacks, after the one `tmux` was found in.
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
    journal_dir: Option<PathBuf>,
    log: PathBuf,
    path: String,
}

impl ServicePlan {
    /// A plan to run `program hub [--journal-dir DIR]`, logging to `<data_dir>/hub.log` where the
    /// manager has a log file, with `tmux_dir` (where `tmux` was found) first on its `PATH`.
    ///
    /// # Errors
    ///
    /// A path with a control character: no unit file can say it.
    pub(crate) fn new(
        manager: Manager,
        home: &Path,
        program: &Path,
        journal_dir: Option<&Path>,
        data_dir: &Path,
        tmux_dir: Option<&Path>,
    ) -> Result<Self, CliError> {
        let mut dirs: Vec<String> = Vec::new();
        for dir in tmux_dir
            .map(|dir| dir.to_string_lossy().into_owned())
            .into_iter()
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
            journal_dir: journal_dir.map(Path::to_owned),
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
        if let Some(dir) = &self.journal_dir {
            argv.push("--journal-dir".to_owned());
            argv.push(dir.to_string_lossy().into_owned());
        }
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

    fn unit(&self) -> String {
        let exec = self
            .arguments()
            .iter()
            .map(|argument| systemd_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let environment = systemd_quote(&format!("PATH={}", self.path));
        format!(
            r"# {UNIT}: written by `octet hub install`; `octet hub uninstall` removes it.
[Unit]
Description=Octet hub, your terminal sessions on your phone

[Service]
ExecStart={exec}
Restart=on-failure
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

/// One systemd word: quoted if it holds anything systemd would split on or expand. Inside
/// quotes `\` and `"` are escaped; `%` (a specifier) and `$` (a variable) are doubled.
fn systemd_quote(word: &str) -> String {
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
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The directory holding an executable `tmux` on `search` (a `PATH` value), if any. Relative
/// entries are skipped: the service would resolve them against its own directory.
pub(crate) fn find_tmux(search: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search)
        .filter(|dir| dir.is_absolute())
        .find(|dir| {
            std::fs::metadata(dir.join("tmux"))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The program that loads and unloads the unit: `launchctl` or `systemctl`, or the test seam.
#[derive(Debug)]
pub(crate) struct Control {
    program: PathBuf,
    /// The user id for `launchctl gui/<uid>`; asked of `id -u` when first needed if `None`.
    uid: Option<String>,
}

impl Control {
    /// A controller that runs `program`, for the user id `uid`.
    #[cfg(test)]
    pub(crate) fn new(program: PathBuf, uid: &str) -> Self {
        Self {
            program,
            uid: Some(uid.into()),
        }
    }

    /// The real tool for `manager`, or `$OCTET_SERVICE_CTL` when set.
    pub(crate) fn from_env(manager: Manager) -> Self {
        let program = std::env::var_os("OCTET_SERVICE_CTL")
            .filter(|path| !path.is_empty())
            .map_or_else(
                || {
                    PathBuf::from(match manager {
                        Manager::Launchd => "launchctl",
                        Manager::Systemd => "systemctl",
                    })
                },
                PathBuf::from,
            );
        Self { program, uid: None }
    }

    fn user_id(&self) -> Result<String, CliError> {
        if let Some(uid) = &self.uid {
            return Ok(uid.clone());
        }
        let output = Command::new("id")
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
    if let Some(parent) = plan.log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| failed(format!("Couldn't create {}: {e}", parent.display())))?;
    }
    write_unit(&path, &plan.text())
        .map_err(|e| failed(format!("Couldn't write {}: {e}", path.display())))?;
    match plan.manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", control.user_id()?);
            let (domain, plist) = (OsString::from(domain), path.clone().into_os_string());
            // A copy that is already loaded has to go first; it may not be.
            let _ = control.run(&args([OsStr::new("bootout"), &domain, &plist]));
            let mut tries = 1;
            while let Err(error) = control.run(&args([OsStr::new("bootstrap"), &domain, &plist])) {
                if tries >= BOOTSTRAP_TRIES {
                    return Err(error);
                }
                tries += 1;
                std::thread::sleep(BOOTSTRAP_WAIT);
            }
        }
        Manager::Systemd => {
            control.run(&args([OsStr::new("--user"), OsStr::new("daemon-reload")]))?;
            control.run(&args([
                OsStr::new("--user"),
                OsStr::new("enable"),
                OsStr::new("--now"),
                OsStr::new(UNIT),
            ]))?;
        }
    }
    let log = match plan.manager {
        Manager::Launchd => plan.log.display().to_string(),
        Manager::Systemd => format!("journalctl --user -u {UNIT}"),
    };
    writeln!(
        out,
        "Installed {}\nThe hub starts at login and restarts if it stops.\nLog: {log}\nRemove it with: octet hub uninstall",
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
    if plan.manager == Manager::Systemd {
        control.run(&args([OsStr::new("--user"), OsStr::new("daemon-reload")]))?;
    }
    writeln!(out, "Removed {}", path.display()).map_err(say)
}

/// `octet hub install` for this user: the running `octet`, the hub's data directory for the log.
///
/// # Errors
///
/// No home directory, or what [`install`] reports.
pub(crate) fn install_here(
    data_dir: &Path,
    journal_dir: Option<&Path>,
    print: bool,
) -> Result<(), CliError> {
    let manager = Manager::native();
    let plan = here(manager, data_dir, journal_dir)?;
    install(
        &plan,
        &Control::from_env(manager),
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
    let plan = here(manager, data_dir, None)?;
    uninstall(&plan, &Control::from_env(manager), &mut std::io::stdout())
}

fn here(
    manager: Manager,
    data_dir: &Path,
    journal_dir: Option<&Path>,
) -> Result<ServicePlan, CliError> {
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
    let journal_dir = journal_dir.map(absolute).transpose()?;
    let data_dir = absolute(data_dir)?;
    let tmux = std::env::var_os("PATH").and_then(|search| find_tmux(&search));
    ServicePlan::new(
        manager,
        &home,
        &program,
        journal_dir.as_deref(),
        &data_dir,
        tmux.as_deref(),
    )
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
        fn plan(&self, manager: Manager, journal_dir: Option<&Path>) -> ServicePlan {
            ServicePlan::new(
                manager,
                &self.home,
                Path::new("/opt/octet/bin/octet"),
                journal_dir,
                &self.data,
                None,
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
        let found = find_tmux(&search);
        assert_eq!(found.as_deref(), Some(bin.as_path()));
        let rig = Rig::new("");
        let plan = ServicePlan::new(
            Manager::Launchd,
            &rig.home,
            Path::new("/opt/octet/bin/octet"),
            None,
            &rig.data,
            found.as_deref(),
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
        assert_eq!(find_tmux(&search).as_deref(), Some(real.as_path()));
        assert_eq!(find_tmux(OsStr::new("")), None);
    }

    #[test]
    fn the_launchd_plist_runs_the_hub_and_logs_to_hub_log() {
        let rig = Rig::new("");
        let plan = rig.plan(Manager::Launchd, Some(Path::new("/srv/a & <b>")));
        assert_eq!(
            plan.unit_path(),
            rig.home.join("Library/LaunchAgents/dev.octet.hub.plist")
        );
        let text = plan.text();
        let log = rig.data.join("hub.log");
        for needle in [
            "<key>Label</key>\n  <string>dev.octet.hub</string>",
            "<key>ProgramArguments</key>",
            "<string>/opt/octet/bin/octet</string>\n    <string>hub</string>",
            "<string>--journal-dir</string>\n    <string>/srv/a &amp; &lt;b&gt;</string>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>KeepAlive</key>\n  <true/>",
            &format!(
                "<key>StandardOutPath</key>\n  <string>{}</string>",
                log.display()
            ),
            &format!(
                "<key>StandardErrorPath</key>\n  <string>{}</string>",
                log.display()
            ),
            &format!("<string>{DEFAULTS}</string>"),
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        assert!(
            !rig.plan(Manager::Launchd, None)
                .text()
                .contains("--journal-dir")
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
            "ExecStart=/opt/octet/bin/octet hub\n",
            "Restart=on-failure\n",
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
            None,
            &rig.data,
            Some(Path::new("/tmux dir")),
        )
        .unwrap();
        let text = odd.text();
        assert!(
            text.contains("ExecStart=\"/Users/a b/octet\" hub\n"),
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
            None,
            &rig.data,
            Some(Path::new("/usr/local/bin")),
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
            None,
            &rig.data,
            None,
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
                "--user enable --now octet-hub.service"
            ]
        );
        let (result, _) = run_uninstall(&rig, &plan);
        result.unwrap();
        assert!(!unit.exists());
        assert_eq!(
            rig.calls()[2..],
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
        let again = rig.plan(Manager::Systemd, Some(Path::new("/j")));
        run_install(&rig, &again, false).0.unwrap();
        let text = std::fs::read_to_string(again.unit_path()).unwrap();
        assert!(text.contains("--journal-dir /j"), "{text}");
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
}
