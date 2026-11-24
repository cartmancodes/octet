//! `octet`: parses the command line and starts the terminal interface, or
//! runs headless (`--print`, `--rpc`).
#![forbid(unsafe_code)]
use args::{CliError, HubAction, Parsed, Prompt, Run};
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};
mod args;
mod fleet;
mod headless;
mod hub;
mod pair;
mod service;

/// Where journals and goals go under the data directory. Named when the Rust
/// port was a preview; kept so existing journals and goals are still found.
const JOURNAL_DIR: &str = "octet/rust-preview";

/// `octet hub --help`.
fn hub_help() -> &'static str {
    "Octet hub — your terminal sessions on your phone, through a relay, end-to-end encrypted\n\
\n\
Usage: octet hub [ACTION] [--journal-dir PATH] [--print]\n\
\n\
Actions:\n\
  serve          Run the hub until interrupted (the default)\n\
  devices        List the paired phones\n\
  revoke NAME    Unpair the phone called NAME (a running hub drops it within 2 s)\n\
  watch          Print what every session is doing until Ctrl+C\n\
  install        Keep the hub running: a launchd agent (macOS) or systemd user unit (Linux)\n\
  uninstall      Stop that service and remove it\n\
\n\
Options:\n\
  --print        install: show the service file without installing it\n\
  --journal-dir PATH  Where journals and the hub's files live (install keeps it)\n"
}

/// `octet pair --help`.
fn pair_help() -> &'static str {
    "Octet pair — pair a phone with this Mac, starting the hub first if it is not running\n\
\n\
Usage: octet pair [--journal-dir PATH]\n\
\n\
Shows a QR code and a link, valid once and for 10 minutes. Scan it with the Octet app; when\n\
the phone shows a code, check it matches and answer y to pair it. If the hub is not running\n\
it is installed as a per-user service first, as octet hub install does.\n\
\n\
Options:\n\
  --journal-dir PATH  Where journals and the hub's files live\n"
}

/// `--help`: fixed usage and keys, then every command from the registry.
fn help() -> String {
    const HEAD: &[&str] = &[
        "                 [--model MODEL] [--mode MODE] [--resume VENDOR_SESSION_ID]",
        "                 [--binary PATH] [--journal-dir PATH]",
        "                 [--approval-timeout SECONDS] [--effort LEVEL]",
        "                 [--no-plugins] [--no-plugin-hooks] [--trust-workspace-config]",
        "                 [--no-remote]",
        "                 [--print PROMPT|- [--output text|json] | --rpc]",
        "       octet hub [serve|devices|revoke NAME|watch|install|uninstall] (the phone hub; octet hub --help)",
        "       octet pair [--journal-dir PATH] (pair a phone; octet pair --help)",
        "       octet fleet run|resume|stop|status|validate (workflows; octet fleet --help)",
        "       octet --version | --help",
        "",
        "Headless: --print (-p) runs one prompt and writes the reply; - reads it",
        "from stdin. --output json writes every event as a JSON line. --rpc reads",
        "JSON-line commands (prompt, answer, interrupt, mode, effort, quit) from",
        "stdin and writes events. Print mode denies approvals. Exit codes: 0 done,",
        "1 failed, 2 usage error, 130 interrupted (SIGINT), 143 stopped (SIGTERM).",
        "",
        "Defaults: Codex, current directory. Vendor CLI installation and login required.",
        "Modes: ask (default) · accept-edits · auto (vendor auto-review) · full-access",
        "Use --engine demo for an offline interactive preview.",
        "Remote: the interface shares its session with `octet hub` over a private socket;",
        "--no-remote turns that off.",
        "Plugins: the vendor's enabled plugins load (Claude's from its settings'",
        "enabledPlugins); --no-plugins turns them off, --no-plugin-hooks only their hooks.",
        "Antigravity: a workspace's .agents configuration (hooks, MCP servers, plugins)",
        "loads only once you trust it: /trust in the interface, or",
        "--trust-workspace-config for one run (as it is when Octet starts).",
        "",
        "Keys: Enter send · Alt+Enter / Ctrl+J newline · Esc cancel · Ctrl+C twice quit",
        "      PageUp/PageDown scroll · Ctrl+P commands · F1 help · Shift+Tab mode",
        "      @ mention a file · Tab complete · !cmd run (attach) · !!cmd run only",
        "      Ctrl+G edit in $EDITOR · Ctrl+X copy the last reply",
        "",
    ];
    let engines: Vec<&str> = octet_core::Engine::ALL
        .iter()
        .map(|engine| engine.as_str())
        .collect();
    let mut text = format!(
        "Octet — terminal coding workspace (preview)\n\nUsage: octet [--engine {}] [--cwd PATH]\n",
        engines.join("|")
    );
    text.push_str(&HEAD.join("\n"));
    // "Commands: " then the names, wrapped at 76 columns under the first.
    let mut line = String::from("\nCommands: ");
    for (i, name) in octet_tui::command_names().enumerate() {
        let item = if i == 0 {
            name.to_owned()
        } else {
            format!(", {name}")
        };
        if line.chars().count() + item.chars().count() > 77 {
            text.push_str(line.trim_end());
            text.push(',');
            line = format!("\n          {}", &item[2..]);
        } else {
            line.push_str(&item);
        }
    }
    text.push_str(&line);
    text.push_str("\nJournals are separate JSONL files; see --journal-dir.\n");
    text
}
/// Returns the exit code rather than calling `process::exit`: the runtime is
/// dropped first, so every task's vendor process (a session, a model-list
/// probe) is stopped by its drop guard instead of outliving Octet.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let code = match run().await {
        Ok(code) => code,
        Err(error) => {
            headless::warn(&error.to_string());
            error.code()
        }
    };
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}
/// Where journals and the hub's files go: `journal_dir`, else `$XDG_DATA_HOME`, else
/// `~/.local/share`. It is made (owner-only) if missing and resolved to its canonical absolute
/// path, so a terminal UI, the hub and the sessions a phone starts, each with its own working
/// directory, all name the same place.
fn data_dir(journal_dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
    make_data_dir(&data_dir_path(journal_dir)?)
}

/// The data directory [`data_dir`] makes, not made yet and as given.
fn data_dir_path(journal_dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
    journal_dir
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(JOURNAL_DIR))
        })
        .or_else(|| octet_core::home_dir().map(|p| p.join(".local/share").join(JOURNAL_DIR)))
        .ok_or_else(|| {
            CliError::Usage("Set --journal-dir or HOME to choose transcript storage".into())
        })
}

/// Makes the data directory `directory` (owner-only) if missing; its canonical absolute path.
pub(crate) fn make_data_dir(directory: &Path) -> Result<PathBuf, CliError> {
    let resolve = || {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        std::fs::canonicalize(directory)
    };
    resolve().map_err(|e| CliError::Run(format!("Couldn't use {}: {e}", directory.display())))
}

/// Reads the options and runs; the process's exit code.
async fn run() -> Result<i32, CliError> {
    let args = match args::parse_args(std::env::args_os().skip(1))? {
        Parsed::Help => {
            print!("{}", help());
            return Ok(0);
        }
        Parsed::Version => {
            println!("octet {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        Parsed::HubHelp => {
            print!("{}", hub_help());
            return Ok(0);
        }
        Parsed::Hub(args) => {
            let directory = data_dir(args.journal_dir)?;
            return match args.action {
                HubAction::Serve => hub::serve(directory).await,
                HubAction::Devices => hub::devices(&directory).await,
                HubAction::Revoke(name) => hub::revoke(&directory, &name).await,
                HubAction::Watch => hub::watch(&directory).await,
                HubAction::Install { print } => hub::install(directory, print).await,
                HubAction::Uninstall => hub::uninstall(directory).await,
            };
        }
        Parsed::Fleet {
            action,
            journal_dir,
        } => {
            let explicit = journal_dir.is_some();
            // `--help` and `validate` touch no storage, so none is made for them;
            // `run` makes it once it has checked where it is.
            let directory = if matches!(action, args::FleetAction::Run { .. }) {
                data_dir_path(journal_dir)?
            } else if action.stores() {
                data_dir(journal_dir)?
            } else {
                PathBuf::new()
            };
            return fleet::fleet(action, directory, explicit).await;
        }
        Parsed::PairHelp => {
            print!("{}", pair_help());
            return Ok(0);
        }
        Parsed::Pair { journal_dir } => {
            let directory = data_dir(journal_dir)?;
            let input = pair::stdin_reader()
                .map_err(|e| CliError::Run(format!("Couldn't read the terminal: {e}")))?;
            return pair::pair(directory, input, std::io::stdout()).await;
        }
        Parsed::Run(args) => args,
    };
    let stdin_prompt = match &args.run {
        Run::Print {
            prompt: Prompt::Stdin,
            ..
        } => {
            let prompt = headless::read_prompt().await?;
            args::check_prompt(&prompt)?;
            Some(prompt)
        }
        _ => None,
    };
    if args.run == Run::Interface
        && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        return Err(CliError::Usage(
            "The terminal UI requires an interactive terminal. Use --help for options.".into(),
        ));
    }
    let cwd = match &args.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().map_err(|e| CliError::Run(e.to_string()))?,
    };
    let mut config = args.config(args::workspace(&cwd)?);
    let directory = data_dir(args.journal_dir.clone())?;
    config.data_dir = Some(directory.clone());
    let result = match args.run {
        Run::Interface => octet_tui::run(
            config,
            directory,
            octet_tui::Options {
                remote: args.remote,
                tmux: args.remote_started.clone(),
                started_by: args.remote_by.clone(),
            },
        )
        .await
        .map(|()| 0)
        .map_err(|e| e.to_string()),
        Run::Print { prompt, json } => {
            let prompt = match prompt {
                Prompt::Text(text) => text,
                Prompt::Stdin => stdin_prompt.unwrap_or_default(),
            };
            headless::print(config, directory, prompt, json).await
        }
        Run::Rpc => headless::rpc(config, directory).await,
    };
    result.map_err(CliError::Run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_data_directory_becomes_absolute_and_exists() {
        // Tests run in the crate's directory; `target` is scratch space there.
        let relative = PathBuf::from(format!(
            "target/../target/octet-data-dir-{}",
            std::process::id()
        ));
        let resolved = data_dir(Some(relative.clone())).expect("a data directory");
        let made = std::fs::canonicalize(&relative);
        let _ = std::fs::remove_dir_all(&relative);
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert_eq!(
            Some(resolved),
            made.ok(),
            "the same directory, made and resolved"
        );
    }
}
