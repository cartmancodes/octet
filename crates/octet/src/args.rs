//! The command line, parsed once into `Args`. Usage errors (exit 2) are kept
//! apart from failures while running (exit 1), so a script driving
//! `--print` can tell a bad flag from a failed turn.
use octet_core::{Config, Engine, Mode, Plugins};
use std::{ffi::OsString, path::PathBuf, time::Duration};
use thiserror::Error;

/// Why `octet` stopped with an error.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum CliError {
    /// The command line is wrong.
    #[error("{0}")]
    Usage(String),
    /// Running failed.
    #[error("{0}")]
    Run(String),
}

impl CliError {
    /// The process's exit code: 2 for usage, as shells expect, 1 otherwise.
    pub(crate) fn code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            CliError::Run(_) => 1,
        }
    }
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    Help,
    Version,
    /// `octet hub --help`.
    HubHelp,
    /// `octet hub ...`.
    Hub(HubArgs),
    /// `octet fleet ...`.
    Fleet {
        action: FleetAction,
        /// Where the fleet's runs live (`--journal-dir`).
        journal_dir: Option<PathBuf>,
    },
    /// `octet pair --help`.
    PairHelp,
    /// `octet pair`: pair a phone, starting the hub first if it is not running.
    Pair {
        /// Where the hub's files live (`--journal-dir`).
        journal_dir: Option<PathBuf>,
    },
    Run(Box<Args>),
}

/// What `octet hub` does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HubAction {
    /// Run the hub until interrupted (the default).
    Serve,
    /// List the paired devices.
    Devices,
    /// Unpair the device with this name.
    Revoke(String),
    /// Print every session's activity until interrupted.
    Watch,
    /// Keep the hub running as a per-user service; with `print`, only show the unit.
    Install { print: bool },
    /// Remove that service.
    Uninstall,
}

/// The options of `octet hub`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HubArgs {
    pub(crate) action: HubAction,
    pub(crate) journal_dir: Option<PathBuf>,
}

/// Where a fleet run's task comes from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskSource {
    Text(String),
    /// `--task-file PATH`.
    File(PathBuf),
    /// `--task -`: all of stdin.
    Stdin,
}

/// What `octet fleet` does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FleetAction {
    /// Start a run of a workflow.
    Run {
        /// A name under `.octet/workflows`, or a path.
        workflow: String,
        task: TaskSource,
        cwd: Option<PathBuf>,
        /// `--trust`: the resolved workflow may run without asking.
        trust: bool,
        /// `--output json`.
        json: bool,
    },
    /// Continue a run, taking a decision first if one is given.
    Resume {
        run: String,
        /// One of [`DECISIONS`].
        decision: Option<String>,
        /// The new cap, for the `rounds` decision.
        rounds: Option<u32>,
        json: bool,
    },
    /// `resume --decision stop`.
    Stop { run: String },
    /// Every run, or one in detail.
    Status { run: Option<String> },
    /// Check a workflow without running anything.
    Validate {
        workflow: String,
        cwd: Option<PathBuf>,
    },
    /// `octet fleet --help`.
    Help,
}

impl FleetAction {
    /// Whether the command reads or writes the fleet's runs.
    pub(crate) fn stores(&self) -> bool {
        !matches!(self, FleetAction::Help | FleetAction::Validate { .. })
    }
}

/// The decisions `--decision` names (spec, "Recovery and decisions").
pub(crate) const DECISIONS: [&str; 9] = [
    "keep", "adopt", "continue", "rounds", "retry", "kill", "wait", "release", "stop",
];

/// What to run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Run {
    /// The terminal interface.
    Interface,
    /// One prompt; the reply (or, with `json`, every event) on stdout.
    Print { prompt: Prompt, json: bool },
    /// JSON-line commands in, events out.
    Rpc,
}

/// Where a `--print` prompt comes from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Prompt {
    Text(String),
    /// `-`: all of stdin.
    Stdin,
}

/// The options, checked. The workspace and journal directory, which need
/// the filesystem, are resolved by the caller.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Args {
    pub(crate) engine: Engine,
    pub(crate) binary: Option<PathBuf>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) model: Option<String>,
    pub(crate) resume: Option<String>,
    pub(crate) journal_dir: Option<PathBuf>,
    pub(crate) mode: Mode,
    pub(crate) approval_timeout: Duration,
    pub(crate) effort: Option<String>,
    pub(crate) plugins: Plugins,
    /// Whether to start Antigravity in a workspace whose `.agents` configuration is not trusted
    /// (`--trust-workspace-config`), as long as it is what was there when Octet started; nothing is
    /// recorded.
    pub(crate) trust_workspace: bool,
    /// Whether the terminal interface shares its session with a hub.
    pub(crate) remote: bool,
    /// The tmux session a hub's `start` launched this interface in (the hidden `--remote-started`).
    pub(crate) remote_started: Option<String>,
    /// The device that asked for it (the hidden `--remote-by`).
    pub(crate) remote_by: Option<String>,
    pub(crate) run: Run,
}

impl Args {
    /// The session's configuration, in workspace `cwd`.
    pub(crate) fn config(&self, cwd: PathBuf) -> Config {
        let binary = self
            .binary
            .clone()
            .unwrap_or_else(|| PathBuf::from(self.engine.provider().default_binary));
        Config {
            model: self.model.clone(),
            resume: self.resume.clone(),
            mode: self.mode,
            approval_timeout: self.approval_timeout,
            effort: self.effort.clone(),
            plugins: self.plugins,
            // What the workspace holds now is all the flag ever trusts.
            trust_workspace: self
                .trust_workspace
                .then(|| octet_core::TrustedAtStart::capture(&cwd)),
            ..Config::new(self.engine, binary, cwd)
        }
    }
}

fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}

/// The next argument as `name`'s value. A value never starts with "--", so
/// a forgotten value cannot swallow the next option.
fn value(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<OsString, CliError> {
    args.next()
        .filter(|value| !value.to_string_lossy().starts_with("--"))
        .ok_or_else(|| usage(format!("{name} requires a value. Use --help.")))
}

/// The next argument as `name`'s value, whatever it starts with: a prompt
/// is free text, and may begin with dashes.
fn verbatim(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<String, CliError> {
    args.next()
        .ok_or_else(|| usage(format!("{name} requires a value. Use --help.")))?
        .into_string()
        .map_err(|_| usage(format!("{name} must be UTF-8 text")))
}

/// `name`'s value as text.
fn text(args: &mut impl Iterator<Item = OsString>, name: &str) -> Result<String, CliError> {
    value(args, name)?
        .into_string()
        .map_err(|_| usage(format!("{name} must be UTF-8 text")))
}

/// Parses the arguments after the program name.
///
/// # Errors
///
/// A usage error naming the first problem: an unknown option, a missing or
/// invalid value, or options that cannot be combined.
#[expect(
    clippy::too_many_lines,
    reason = "a flat match over the options, one short arm each"
)]
pub(crate) fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, CliError> {
    let mut args = args.into_iter().peekable();
    if args.next_if(|first| first == "hub").is_some() {
        return parse_hub(args);
    }
    if args.next_if(|first| first == "pair").is_some() {
        return parse_pair(args);
    }
    if args.next_if(|first| first == "fleet").is_some() {
        return parse_fleet(args);
    }
    let mut engine = "codex".to_owned();
    let mut binary = None;
    let mut cwd = None;
    let mut model = None;
    let mut resume = None;
    let mut journal_dir = None;
    let mut mode = Mode::Ask;
    let mut approval_timeout = octet_core::DEFAULT_APPROVAL_TIMEOUT;
    let mut effort = None;
    let mut plugins = Plugins::default();
    let mut trust_workspace = false;
    let mut remote = true;
    let mut remote_started = None;
    let mut remote_by = None;
    let mut print = None;
    let mut output = None;
    let mut rpc = false;
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|arg| {
            usage(format!(
                "Unknown option {}. Use --help.",
                arg.to_string_lossy()
            ))
        })?;
        if let Some(prompt) = arg.strip_prefix("--print=") {
            print = Some(prompt.to_owned());
            continue;
        }
        let name = if arg == "-p" { "--print" } else { arg.as_str() };
        match name {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            "--rpc" => rpc = true,
            "--no-plugins" => plugins.load = false,
            "--no-plugin-hooks" => plugins.hooks = false,
            "--trust-workspace-config" => trust_workspace = true,
            "--no-remote" => remote = false,
            "--remote-started" => remote_started = Some(text(&mut args, name)?),
            "--remote-by" => remote_by = Some(text(&mut args, name)?),
            "--engine" => engine = text(&mut args, name)?,
            "--binary" => binary = Some(PathBuf::from(value(&mut args, name)?)),
            "--cwd" => cwd = Some(PathBuf::from(value(&mut args, name)?)),
            "--model" => model = Some(text(&mut args, name)?),
            "--resume" => resume = Some(text(&mut args, name)?),
            "--journal-dir" => journal_dir = Some(PathBuf::from(value(&mut args, name)?)),
            "--mode" => {
                let value = text(&mut args, name)?;
                mode = Mode::parse(&value).ok_or_else(|| {
                    usage(format!(
                        "Unknown mode {value}. Use ask, accept-edits, auto or full-access."
                    ))
                })?;
            }
            "--approval-timeout" => {
                approval_timeout = text(&mut args, name)?
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (10..=3600).contains(seconds))
                    .map(Duration::from_secs)
                    .ok_or_else(|| {
                        usage("Approval timeout must be a whole number of seconds from 10 to 3600")
                    })?;
            }
            "--effort" => {
                let value = text(&mut args, name)?;
                if !octet_core::valid_effort(&value) {
                    return Err(usage("Effort must be one word, at most 64 bytes"));
                }
                effort = Some(value);
            }
            "--print" => print = Some(verbatim(&mut args, name)?),
            "--output" => {
                output = Some(match text(&mut args, name)?.as_str() {
                    "text" => false,
                    "json" => true,
                    _ => return Err(usage("Output must be text or json")),
                });
            }
            other => return Err(usage(format!("Unknown option {other}. Use --help."))),
        }
    }
    let engine = Engine::parse(&engine).ok_or_else(|| {
        usage(format!(
            "Engine must be {}",
            octet_core::model::engine_choices()
        ))
    })?;
    if let Some(level) = &effort {
        engine.check_effort(level).map_err(usage)?;
    }
    let run = match (print, rpc) {
        (Some(_), true) => return Err(usage("--print and --rpc cannot be combined")),
        (Some(prompt), false) => Run::Print {
            prompt: if prompt == "-" {
                Prompt::Stdin
            } else {
                check_prompt(&prompt)?;
                Prompt::Text(prompt)
            },
            json: output.unwrap_or(false),
        },
        (None, _) if output.is_some() => return Err(usage("--output applies to --print")),
        (None, true) => Run::Rpc,
        (None, false) => Run::Interface,
    };
    Ok(Parsed::Run(Box::new(Args {
        engine,
        binary,
        cwd,
        model,
        resume,
        journal_dir,
        mode,
        approval_timeout,
        effort,
        plugins,
        trust_workspace,
        remote,
        remote_started,
        remote_by,
        run,
    })))
}

/// Parses the arguments after `hub`: an action (`serve` unless named) and its options.
fn parse_hub(args: impl Iterator<Item = OsString>) -> Result<Parsed, CliError> {
    let mut args = args;
    let (mut action, mut journal_dir) = (None, None);
    let mut print = false;
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|arg| {
            usage(format!(
                "Unknown argument {}. Use hub --help.",
                arg.to_string_lossy()
            ))
        })?;
        let named = match arg.as_str() {
            "--help" | "-h" => return Ok(Parsed::HubHelp),
            "--journal-dir" => {
                journal_dir = Some(PathBuf::from(value(&mut args, &arg)?));
                continue;
            }
            "--print" => {
                print = true;
                continue;
            }
            "serve" => HubAction::Serve,
            "install" => HubAction::Install { print: false },
            "uninstall" => HubAction::Uninstall,
            "devices" => HubAction::Devices,
            "watch" => HubAction::Watch,
            "revoke" => {
                let name = verbatim(&mut args, "revoke")?;
                if name.is_empty() {
                    return Err(usage("revoke requires a device name. Use hub --help."));
                }
                HubAction::Revoke(name)
            }
            other => {
                return Err(usage(format!("Unknown argument {other}. Use hub --help.")));
            }
        };
        if action.replace(named).is_some() {
            return Err(usage("Name one hub action. Use hub --help."));
        }
    }
    let mut action = action.unwrap_or(HubAction::Serve);
    if print {
        match &mut action {
            HubAction::Install { print } => *print = true,
            _ => return Err(usage("--print applies to install")),
        }
    }
    Ok(Parsed::Hub(HubArgs {
        action,
        journal_dir,
    }))
}

/// Parses the arguments after `pair`: only where the hub's files live.
fn parse_pair(args: impl Iterator<Item = OsString>) -> Result<Parsed, CliError> {
    let mut args = args;
    let mut journal_dir = None;
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|arg| {
            usage(format!(
                "Unknown argument {}. Use pair --help.",
                arg.to_string_lossy()
            ))
        })?;
        match arg.as_str() {
            "--help" | "-h" => return Ok(Parsed::PairHelp),
            "--journal-dir" => journal_dir = Some(PathBuf::from(value(&mut args, &arg)?)),
            other => return Err(usage(format!("Unknown argument {other}. Use pair --help."))),
        }
    }
    Ok(Parsed::Pair { journal_dir })
}

/// The options of one `octet fleet` command, before they are matched to it.
#[derive(Default)]
struct FleetOptions {
    positional: Vec<String>,
    task: Option<TaskSource>,
    cwd: Option<PathBuf>,
    journal_dir: Option<PathBuf>,
    trust: bool,
    json: bool,
    decision: Option<String>,
    rounds: Option<u32>,
}

/// The fleet commands each option applies to.
fn fleet_option_applies(option: &str) -> Option<&'static [&'static str]> {
    Some(match option {
        "--task" | "--task-file" | "--trust" => &["run"],
        "--cwd" => &["run", "validate"],
        "--output" => &["run", "resume"],
        "--decision" | "--rounds" => &["resume"],
        "--journal-dir" => &["run", "resume", "stop", "status", "validate"],
        _ => return None,
    })
}

/// Parses the arguments after `fleet`: a command, its operand and its options.
fn parse_fleet(args: impl Iterator<Item = OsString>) -> Result<Parsed, CliError> {
    let mut args = args;
    let hint = "Use fleet --help.";
    let first = args
        .next()
        .ok_or_else(|| usage(format!("Name a fleet command. {hint}")))?
        .into_string()
        .map_err(|_| usage(format!("Unknown fleet command. {hint}")))?;
    let help = |journal_dir| {
        Ok(Parsed::Fleet {
            action: FleetAction::Help,
            journal_dir,
        })
    };
    let command: &'static str = match first.as_str() {
        "--help" | "-h" => return help(None),
        "run" => "run",
        "resume" => "resume",
        "stop" => "stop",
        "status" => "status",
        "validate" => "validate",
        other => return Err(usage(format!("Unknown fleet command {other}. {hint}"))),
    };
    let mut options = FleetOptions::default();
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|arg| {
            usage(format!(
                "Unknown argument {}. {hint}",
                arg.to_string_lossy()
            ))
        })?;
        match arg.as_str() {
            "--help" | "-h" => return help(options.journal_dir),
            "--trust" | "--task" | "--task-file" | "--cwd" | "--output" | "--decision"
            | "--rounds" | "--journal-dir" => {}
            positional if !positional.starts_with("--") => {
                options.positional.push(arg);
                continue;
            }
            other => return Err(usage(format!("Unknown option {other}. {hint}"))),
        }
        let name = fleet_option_applies(&arg).filter(|commands| commands.contains(&command));
        if name.is_none() {
            let applies = fleet_option_applies(&arg).map_or_else(String::new, |c| c.join(" and "));
            return Err(usage(format!("{arg} applies to {applies}. {hint}")));
        }
        match arg.as_str() {
            "--trust" => options.trust = true,
            "--task" => {
                let task = verbatim(&mut args, &arg)?;
                let source = if task == "-" {
                    TaskSource::Stdin
                } else {
                    check_prompt(&task)?;
                    TaskSource::Text(task)
                };
                set_task(&mut options, source)?;
            }
            "--task-file" => {
                let path = PathBuf::from(value(&mut args, &arg)?);
                set_task(&mut options, TaskSource::File(path))?;
            }
            "--cwd" => options.cwd = Some(PathBuf::from(value(&mut args, &arg)?)),
            "--journal-dir" => options.journal_dir = Some(PathBuf::from(value(&mut args, &arg)?)),
            "--output" => {
                options.json = match text(&mut args, &arg)?.as_str() {
                    "text" => false,
                    "json" => true,
                    _ => return Err(usage("Output must be text or json")),
                };
            }
            "--decision" => {
                let decision = text(&mut args, &arg)?;
                if !DECISIONS.contains(&decision.as_str()) {
                    return Err(usage(format!(
                        "Unknown decision {decision}. Use {}.",
                        DECISIONS.join(", ")
                    )));
                }
                options.decision = Some(decision);
            }
            _ => {
                options.rounds = Some(
                    text(&mut args, &arg)?
                        .parse::<u32>()
                        .ok()
                        .filter(|rounds| *rounds >= 1)
                        .ok_or_else(|| usage("Rounds must be a whole number of at least 1"))?,
                );
            }
        }
    }
    fleet_action(command, options, hint)
}

fn set_task(options: &mut FleetOptions, source: TaskSource) -> Result<(), CliError> {
    if options.task.replace(source).is_some() {
        return Err(usage("Give the task once: --task, --task-file or --task -"));
    }
    Ok(())
}

/// The command `command` with its operand and options, checked.
fn fleet_action(command: &str, options: FleetOptions, hint: &str) -> Result<Parsed, CliError> {
    let FleetOptions {
        mut positional,
        task,
        cwd,
        journal_dir,
        trust,
        json,
        decision,
        rounds,
    } = options;
    let what = if matches!(command, "run" | "validate") {
        "workflow"
    } else {
        "run"
    };
    if let Some(extra) = positional.get(1) {
        return Err(usage(format!("Unexpected argument {extra}. {hint}")));
    }
    let operand = positional.pop();
    let required =
        |operand: Option<String>| operand.ok_or_else(|| usage(format!("Name the {what}. {hint}")));
    let action = match command {
        "run" => FleetAction::Run {
            workflow: required(operand)?,
            task: task.ok_or_else(|| {
                usage(format!(
                    "Give the task with --task, --task-file or --task -. {hint}"
                ))
            })?,
            cwd,
            trust,
            json,
        },
        "resume" => {
            match (decision.as_deref(), rounds) {
                (Some("rounds"), None) => {
                    return Err(usage("--decision rounds requires --rounds N"));
                }
                (decision, Some(_)) if decision != Some("rounds") => {
                    return Err(usage("--rounds applies to --decision rounds"));
                }
                _ => {}
            }
            FleetAction::Resume {
                run: required(operand)?,
                decision,
                rounds,
                json,
            }
        }
        "stop" => FleetAction::Stop {
            run: required(operand)?,
        },
        "status" => FleetAction::Status { run: operand },
        _ => FleetAction::Validate {
            workflow: required(operand)?,
            cwd,
        },
    };
    Ok(Parsed::Fleet {
        action,
        journal_dir,
    })
}

/// Refuses a prompt the session would refuse, before one opens.
///
/// # Errors
///
/// A usage error for an empty prompt or one over the prompt limit.
pub(crate) fn check_prompt(prompt: &str) -> Result<(), CliError> {
    if prompt.trim().is_empty() {
        return Err(usage("The prompt is empty"));
    }
    check_prompt_size(prompt.len())
}

/// Refuses a prompt of `bytes` over the prompt limit.
///
/// # Errors
///
/// A usage error naming the limit.
pub(crate) fn check_prompt_size(bytes: usize) -> Result<(), CliError> {
    if bytes > octet_core::PROMPT_LIMIT {
        return Err(usage(format!(
            "The prompt is over {} KiB",
            octet_core::PROMPT_LIMIT / 1024
        )));
    }
    Ok(())
}

/// The workspace: `path` as an existing directory with a UTF-8 path, which
/// the journal records.
///
/// # Errors
///
/// A usage error for a path that is not UTF-8, cannot be resolved, or is not
/// a directory.
pub(crate) fn workspace(path: &std::path::Path) -> Result<PathBuf, CliError> {
    let utf8 = |path: &std::path::Path| {
        path.to_str()
            .map(|_| ())
            .ok_or_else(|| usage("Workspace path must be UTF-8"))
    };
    utf8(path)?;
    let path = path
        .canonicalize()
        .map_err(|e| usage(format!("Invalid workspace: {e}")))?;
    utf8(&path)?;
    if !path.is_dir() {
        return Err(usage("Workspace must be a directory"));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, CliError> {
        parse_args(args.iter().map(OsString::from))
    }
    fn args(list: &[&str]) -> Args {
        match parse(list) {
            Ok(Parsed::Run(args)) => *args,
            other => panic!("{other:?}"),
        }
    }
    fn error(list: &[&str]) -> String {
        match parse(list) {
            Err(CliError::Usage(message)) => message,
            other => panic!("{list:?}: {other:?}"),
        }
    }

    #[test]
    fn plugin_switches_parse() {
        assert_eq!(
            args(&[]).plugins,
            octet_core::Plugins {
                load: true,
                hooks: true
            }
        );
        assert!(!args(&["--no-plugins"]).plugins.load);
        let no_hooks = args(&["--no-plugin-hooks"]).plugins;
        assert!(no_hooks.load && !no_hooks.hooks);
        assert!(!args(&["--no-plugins"]).config("/tmp".into()).plugins.load);
    }
    #[test]
    fn the_trust_flag_reaches_the_configuration() {
        assert!(!args(&[]).trust_workspace);
        assert!(args(&[]).config("/tmp".into()).trust_workspace.is_none());
        assert!(args(&["--trust-workspace-config"]).trust_workspace);
        let print = args(&["--trust-workspace-config", "--print", "hi"]);
        assert!(print.config("/tmp".into()).trust_workspace.is_some());
        // A plain flag: the next option is not its value.
        assert_eq!(
            args(&["--trust-workspace-config", "--mode", "auto"]).mode,
            Mode::Auto
        );
    }
    fn hub(list: &[&str]) -> HubArgs {
        let mut full = vec!["hub"];
        full.extend_from_slice(list);
        match parse(&full) {
            Ok(Parsed::Hub(args)) => args,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hub_serves_unless_told_otherwise() {
        let plain = hub(&[]);
        assert_eq!(plain.action, HubAction::Serve);
        assert_eq!(plain.journal_dir, None);
        let full = hub(&["--journal-dir", "/d", "serve"]);
        assert_eq!(full.action, HubAction::Serve);
        assert_eq!(full.journal_dir, Some(PathBuf::from("/d")));
        assert_eq!(hub(&["devices"]).action, HubAction::Devices);
        assert_eq!(
            hub(&["--journal-dir", "/d", "watch"]).action,
            HubAction::Watch
        );
        assert_eq!(
            hub(&["revoke", "My phone", "--journal-dir", "/d"]).action,
            HubAction::Revoke("My phone".into())
        );
        assert_eq!(parse(&["hub", "--help"]), Ok(Parsed::HubHelp));
        assert_eq!(parse(&["hub", "devices", "-h"]), Ok(Parsed::HubHelp));
        // Only the first argument makes it a hub command.
        assert!(matches!(
            parse(&["--no-remote", "hub"]),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn hub_installs_and_uninstalls_the_service() {
        assert_eq!(
            hub(&["install"]).action,
            HubAction::Install { print: false }
        );
        assert_eq!(
            hub(&["install", "--print"]).action,
            HubAction::Install { print: true }
        );
        assert_eq!(
            hub(&["--print", "install"]).action,
            HubAction::Install { print: true }
        );
        assert_eq!(hub(&["uninstall"]).action, HubAction::Uninstall);
        let carried = hub(&["--journal-dir", "/d", "install"]);
        assert_eq!(carried.journal_dir, Some(PathBuf::from("/d")));
    }

    #[test]
    fn hub_usage_errors_name_the_problem() {
        for (list, message) in [
            // The hub listens on no port, and has no tailnet owner.
            (
                &["hub", "--port", "47823"][..],
                "Unknown argument --port. Use hub --help.",
            ),
            (
                &["hub", "--owner", "me@x"][..],
                "Unknown argument --owner. Use hub --help.",
            ),
            (
                &["hub", "revoke"][..],
                "revoke requires a value. Use --help.",
            ),
            (
                &["hub", "bogus"][..],
                "Unknown argument bogus. Use hub --help.",
            ),
            (
                &["hub", "--engine", "demo"][..],
                "Unknown argument --engine. Use hub --help.",
            ),
            (
                &["hub", "devices", "watch"][..],
                "Name one hub action. Use hub --help.",
            ),
            (
                &["hub", "uninstall", "--print"][..],
                "--print applies to install",
            ),
            (
                &["hub", "serve", "--print"][..],
                "--print applies to install",
            ),
            (&["hub", "--print"][..], "--print applies to install"),
        ] {
            assert_eq!(error(list), message, "{list:?}");
        }
    }

    #[test]
    fn remote_control_is_on_unless_turned_off() {
        assert!(args(&[]).remote);
        assert!(!args(&["--no-remote"]).remote);
    }
    #[test]
    fn the_hidden_flags_name_the_tmux_session_and_device_a_phone_started() {
        let desk = args(&[]);
        assert_eq!((desk.remote_started, desk.remote_by), (None, None));
        let phone = args(&[
            "--remote-started",
            "octet-abc123",
            "--remote-by",
            "Gemini 10",
        ]);
        assert_eq!(phone.remote_started.as_deref(), Some("octet-abc123"));
        assert_eq!(phone.remote_by.as_deref(), Some("Gemini 10"));
        assert!(matches!(
            parse_args(["--remote-started".into()]),
            Err(CliError::Usage(_))
        ));
    }
    #[test]
    fn defaults_are_codex_in_ask_mode_with_the_interface() {
        let parsed = args(&[]);
        assert_eq!(parsed.engine, Engine::CODEX);
        assert_eq!(parsed.mode, Mode::Ask);
        assert_eq!(
            parsed.approval_timeout,
            octet_core::DEFAULT_APPROVAL_TIMEOUT
        );
        assert_eq!(parsed.run, Run::Interface);
    }

    #[test]
    fn help_and_version_win_wherever_they_are() {
        assert_eq!(parse(&["--engine", "demo", "-h"]), Ok(Parsed::Help));
        assert_eq!(parse(&["--version"]), Ok(Parsed::Version));
    }

    #[test]
    fn every_option_reaches_the_configuration() {
        let parsed = args(&[
            "--engine",
            "claude",
            "--binary",
            "/bin/claude",
            "--model",
            "opus",
            "--resume",
            "s-1",
            "--mode",
            "auto",
            "--approval-timeout",
            "10",
            "--effort",
            "max",
        ]);
        let config = parsed.config(PathBuf::from("/work"));
        assert_eq!(config.engine, Engine::CLAUDE);
        assert_eq!(config.binary, PathBuf::from("/bin/claude"));
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.resume.as_deref(), Some("s-1"));
        assert_eq!(config.mode, Mode::Auto);
        assert_eq!(config.approval_timeout, Duration::from_secs(10));
        assert_eq!(config.effort.as_deref(), Some("max"));
        assert!(!config.fork);
    }

    #[test]
    fn the_binary_defaults_to_the_providers() {
        let config = args(&["--engine", "claude"]).config(PathBuf::from("/w"));
        assert_eq!(config.binary, PathBuf::from("claude"));
    }

    #[test]
    fn a_print_prompt_may_start_with_dashes() {
        for list in [&["-p", "--fix the bug"][..], &["--print=--fix the bug"][..]] {
            assert_eq!(
                args(list).run,
                Run::Print {
                    prompt: Prompt::Text("--fix the bug".into()),
                    json: false
                },
                "{list:?}"
            );
        }
    }

    #[test]
    fn print_and_rpc_runs() {
        assert_eq!(
            args(&["-p", "hi", "--output", "json"]).run,
            Run::Print {
                prompt: Prompt::Text("hi".into()),
                json: true
            }
        );
        assert_eq!(
            args(&["--print", "-"]).run,
            Run::Print {
                prompt: Prompt::Stdin,
                json: false
            }
        );
        assert_eq!(args(&["--rpc"]).run, Run::Rpc);
    }

    #[test]
    fn every_usage_error_names_the_problem() {
        let engine_error = format!("Engine must be {}", octet_core::model::engine_choices());
        let long = "x".repeat(octet_core::PROMPT_LIMIT + 1);
        for (list, message) in [
            (&["--bogus"][..], "Unknown option --bogus. Use --help."),
            (&["stray"][..], "Unknown option stray. Use --help."),
            (&["--cwd"][..], "--cwd requires a value. Use --help."),
            (
                &["--cwd", "--journal-dir", "/tmp"][..],
                "--cwd requires a value. Use --help.",
            ),
            (&["--engine", "gemini"][..], &engine_error),
            (
                &["--mode", "bogus"][..],
                "Unknown mode bogus. Use ask, accept-edits, auto or full-access.",
            ),
            (
                &["--approval-timeout", "5"][..],
                "Approval timeout must be a whole number of seconds from 10 to 3600",
            ),
            (
                &["--approval-timeout", "abc"][..],
                "Approval timeout must be a whole number of seconds from 10 to 3600",
            ),
            (
                &["--approval-timeout", "3601"][..],
                "Approval timeout must be a whole number of seconds from 10 to 3600",
            ),
            (
                &["--effort", ""][..],
                "Effort must be one word, at most 64 bytes",
            ),
            (
                &["--effort", "two words"][..],
                "Effort must be one word, at most 64 bytes",
            ),
            (
                &["--engine", "claude", "--effort", "bogus"][..],
                "Claude takes effort low, medium, high, xhigh or max",
            ),
            (
                &["--output", "xml", "-p", "x"][..],
                "Output must be text or json",
            ),
            (&["--output", "json"][..], "--output applies to --print"),
            (
                &["-p", "x", "--rpc"][..],
                "--print and --rpc cannot be combined",
            ),
            (&["-p", "  "][..], "The prompt is empty"),
            (&["-p", &long][..], "The prompt is over 64 KiB"),
        ] {
            assert_eq!(error(list), message, "{list:?}");
        }
    }

    #[test]
    fn usage_errors_exit_2_and_failures_1() {
        assert_eq!(usage("x").code(), 2);
        assert_eq!(CliError::Run("x".into()).code(), 1);
    }

    #[test]
    fn a_non_utf8_cwd_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(b"/tmp/octet-\xff".to_vec());
        let parsed = parse_args([OsString::from("--cwd"), bad]).unwrap();
        let Parsed::Run(args) = parsed else {
            panic!("{parsed:?}")
        };
        let error = workspace(&args.cwd.unwrap()).unwrap_err();
        assert_eq!(error, usage("Workspace path must be UTF-8"));
    }

    #[test]
    fn a_workspace_must_be_an_existing_directory() {
        assert!(matches!(
            workspace(std::path::Path::new("/nonexistent/octet")),
            Err(CliError::Usage(message)) if message.starts_with("Invalid workspace")
        ));
        assert_eq!(
            workspace(std::path::Path::new("/etc/hosts")),
            Err(usage("Workspace must be a directory"))
        );
    }

    fn fleet(list: &[&str]) -> (FleetAction, Option<PathBuf>) {
        let mut full = vec!["fleet"];
        full.extend_from_slice(list);
        match parse(&full) {
            Ok(Parsed::Fleet {
                action,
                journal_dir,
            }) => (action, journal_dir),
            other => panic!("{list:?}: {other:?}"),
        }
    }

    #[test]
    fn fleet_run_takes_a_workflow_and_a_task() {
        let (action, journal_dir) = fleet(&["run", "wf", "--task", "Add a flag"]);
        assert_eq!(
            action,
            FleetAction::Run {
                workflow: "wf".into(),
                task: TaskSource::Text("Add a flag".into()),
                cwd: None,
                trust: false,
                json: false,
            }
        );
        assert_eq!(journal_dir, None);
        let (action, journal_dir) = fleet(&[
            "run",
            "--task-file",
            "t.md",
            "plans/wf.yaml",
            "--cwd",
            "/w",
            "--trust",
            "--output",
            "json",
            "--journal-dir",
            "/d",
        ]);
        assert_eq!(
            action,
            FleetAction::Run {
                workflow: "plans/wf.yaml".into(),
                task: TaskSource::File("t.md".into()),
                cwd: Some("/w".into()),
                trust: true,
                json: true,
            }
        );
        assert_eq!(journal_dir, Some(PathBuf::from("/d")));
        // `-` reads the task from stdin, and a task may start with dashes.
        assert!(matches!(
            fleet(&["run", "wf", "--task", "-"]).0,
            FleetAction::Run {
                task: TaskSource::Stdin,
                ..
            }
        ));
        assert!(matches!(
            fleet(&["run", "wf", "--task", "--fix it"]).0,
            FleetAction::Run { task: TaskSource::Text(text), .. } if text == "--fix it"
        ));
    }

    #[test]
    fn fleet_resume_stop_status_and_validate_parse() {
        assert_eq!(
            fleet(&["resume", "f1-0001"]).0,
            FleetAction::Resume {
                run: "f1-0001".into(),
                decision: None,
                rounds: None,
                json: false,
            }
        );
        assert_eq!(
            fleet(&[
                "resume",
                "f1-0001",
                "--decision",
                "keep",
                "--output",
                "json"
            ])
            .0,
            FleetAction::Resume {
                run: "f1-0001".into(),
                decision: Some("keep".into()),
                rounds: None,
                json: true,
            }
        );
        assert_eq!(
            fleet(&["resume", "f1-0001", "--decision", "rounds", "--rounds", "3"]).0,
            FleetAction::Resume {
                run: "f1-0001".into(),
                decision: Some("rounds".into()),
                rounds: Some(3),
                json: false,
            }
        );
        assert_eq!(
            fleet(&["stop", "f1-0001", "--journal-dir", "/d"]),
            (
                FleetAction::Stop {
                    run: "f1-0001".into()
                },
                Some(PathBuf::from("/d"))
            )
        );
        assert_eq!(fleet(&["status"]).0, FleetAction::Status { run: None });
        assert_eq!(
            fleet(&["status", "f1-0001"]).0,
            FleetAction::Status {
                run: Some("f1-0001".into())
            }
        );
        assert_eq!(
            fleet(&["validate", "wf", "--cwd", "/w"]).0,
            FleetAction::Validate {
                workflow: "wf".into(),
                cwd: Some("/w".into()),
            }
        );
        for decision in DECISIONS {
            let rounds: &[&str] = if decision == "rounds" {
                &["--rounds", "2"]
            } else {
                &[]
            };
            let mut list = vec!["resume", "r", "--decision", decision];
            list.extend_from_slice(rounds);
            assert!(
                matches!(fleet(&list).0, FleetAction::Resume { decision: Some(d), .. } if d == decision),
                "{decision}"
            );
        }
        assert_eq!(
            parse(&["fleet", "--help"]),
            Ok(Parsed::Fleet {
                action: FleetAction::Help,
                journal_dir: None,
            })
        );
        assert!(matches!(
            parse(&["fleet", "run", "-h"]),
            Ok(Parsed::Fleet {
                action: FleetAction::Help,
                ..
            })
        ));
    }

    #[test]
    fn fleet_usage_errors_name_the_problem() {
        let cases: &[(&[&str], &str)] = &[
            (&["fleet"], "Name a fleet command"),
            (&["fleet", "launch"], "Unknown fleet command launch"),
            (
                &["fleet", "run", "wf"],
                "Give the task with --task, --task-file or --task -",
            ),
            (&["fleet", "run", "--task", "x"], "Name the workflow"),
            (
                &["fleet", "run", "a", "b", "--task", "x"],
                "Unexpected argument b",
            ),
            (
                &["fleet", "run", "wf", "--task", "x", "--task-file", "f"],
                "Give the task once",
            ),
            (
                &["fleet", "run", "wf", "--task", "x", "--task", "y"],
                "Give the task once",
            ),
            (
                &["fleet", "run", "wf", "--task", "  "],
                "The prompt is empty",
            ),
            (&["fleet", "run", "wf", "--task"], "--task requires a value"),
            (
                &["fleet", "run", "wf", "--task", "x", "--output", "xml"],
                "Output must be text or json",
            ),
            (
                &["fleet", "run", "wf", "--task", "x", "--decision", "keep"],
                "--decision applies to resume",
            ),
            (
                &["fleet", "run", "wf", "--task", "x", "--bogus"],
                "Unknown option --bogus",
            ),
            (&["fleet", "resume"], "Name the run"),
            (
                &["fleet", "resume", "r", "--decision", "bogus"],
                "Unknown decision bogus. Use keep, adopt",
            ),
            (
                &["fleet", "resume", "r", "--rounds", "0"],
                "Rounds must be a whole number of at least 1",
            ),
            (
                &["fleet", "resume", "r", "--rounds", "x"],
                "Rounds must be a whole number of at least 1",
            ),
            (
                &["fleet", "resume", "r", "--decision", "rounds"],
                "--decision rounds requires --rounds",
            ),
            (
                &[
                    "fleet",
                    "resume",
                    "r",
                    "--decision",
                    "keep",
                    "--rounds",
                    "2",
                ],
                "--rounds applies to --decision rounds",
            ),
            (
                &["fleet", "resume", "r", "--task", "x"],
                "--task applies to run",
            ),
            (&["fleet", "stop"], "Name the run"),
            (
                &["fleet", "stop", "r", "--decision", "keep"],
                "--decision applies to resume",
            ),
            (&["fleet", "status", "a", "b"], "Unexpected argument b"),
            (&["fleet", "validate"], "Name the workflow"),
            (
                &["fleet", "validate", "wf", "--trust"],
                "--trust applies to run",
            ),
        ];
        for (list, expected) in cases {
            let message = error(list);
            assert!(message.contains(expected), "{list:?}: {message}");
        }
    }
}
