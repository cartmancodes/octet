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
}

/// The options of `octet hub`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HubArgs {
    pub(crate) action: HubAction,
    pub(crate) journal_dir: Option<PathBuf>,
    /// The loopback port to serve on; the hub's configured one when absent.
    pub(crate) port: Option<u16>,
    /// The tailnet owner's login; Tailscale's own answer when absent.
    pub(crate) owner: Option<String>,
}

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
        remote,
        remote_started,
        remote_by,
        run,
    })))
}

/// Parses the arguments after `hub`: an action (`serve` unless named) and its options.
fn parse_hub(args: impl Iterator<Item = OsString>) -> Result<Parsed, CliError> {
    let mut args = args;
    let (mut action, mut journal_dir, mut port, mut owner) = (None, None, None, None);
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
            "--port" => {
                port = Some(
                    text(&mut args, &arg)?
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port > 0)
                        .ok_or_else(|| usage("Port must be a whole number from 1 to 65535"))?,
                );
                continue;
            }
            "--owner" => {
                owner = Some(text(&mut args, &arg)?);
                continue;
            }
            "serve" => HubAction::Serve,
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
    let action = action.unwrap_or(HubAction::Serve);
    if action != HubAction::Serve && (port.is_some() || owner.is_some()) {
        return Err(usage("--port and --owner apply to serve"));
    }
    Ok(Parsed::Hub(HubArgs {
        action,
        journal_dir,
        port,
        owner,
    }))
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
        assert_eq!(
            (plain.port, plain.owner, plain.journal_dir),
            (None, None, None)
        );
        let full = hub(&[
            "--journal-dir",
            "/d",
            "--port",
            "65535",
            "--owner",
            "me@x",
            "serve",
        ]);
        assert_eq!(full.port, Some(65535));
        assert_eq!(full.owner.as_deref(), Some("me@x"));
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
    fn hub_usage_errors_name_the_problem() {
        for (list, message) in [
            (
                &["hub", "--port", "0"][..],
                "Port must be a whole number from 1 to 65535",
            ),
            (
                &["hub", "--port", "65536"][..],
                "Port must be a whole number from 1 to 65535",
            ),
            (
                &["hub", "--port", "x"][..],
                "Port must be a whole number from 1 to 65535",
            ),
            (
                &["hub", "--port"][..],
                "--port requires a value. Use --help.",
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
                &["hub", "watch", "--port", "5"][..],
                "--port and --owner apply to serve",
            ),
            (
                &["hub", "devices", "--owner", "x"][..],
                "--port and --owner apply to serve",
            ),
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
}
