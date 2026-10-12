//! `octet`: parses the command line and starts the terminal interface, or
//! runs headless (`--print`, `--rpc`).
#![forbid(unsafe_code)]
use args::{CliError, HubAction, Parsed, Prompt, Run};
use std::{io::IsTerminal, path::PathBuf};
mod args;
mod headless;
mod hub;

/// Where journals and goals go under the data directory. Named when the Rust
/// port was a preview; kept so existing journals and goals are still found.
const JOURNAL_DIR: &str = "octet/rust-preview";

/// `octet hub --help`.
fn hub_help() -> &'static str {
    "Octet hub — your terminal sessions on your phone, over your tailnet\n\
\n\
Usage: octet hub [ACTION] [--journal-dir PATH] [--port N] [--owner LOGIN]\n\
\n\
Actions:\n\
  serve          Run the hub on 127.0.0.1 until interrupted (the default)\n\
  devices        List the paired phones\n\
  revoke NAME    Unpair the phone called NAME (a running hub drops it within 2 s)\n\
  watch          Print what every session is doing until Ctrl+C\n\
\n\
Options:\n\
  --port N       serve: the loopback port (default: hub.json's, else 47823)\n\
  --owner LOGIN  serve: the tailnet login that may connect (default: this Mac's, from Tailscale)\n\
  --journal-dir PATH  Where journals and the hub's files live\n\
\n\
Publish the hub to your tailnet with: tailscale serve --bg <port>\n"
}

/// `--help`: fixed usage and keys, then every command from the registry.
fn help() -> String {
    const HEAD: &[&str] = &[
        "                 [--model MODEL] [--mode MODE] [--resume VENDOR_SESSION_ID]",
        "                 [--binary PATH] [--journal-dir PATH]",
        "                 [--approval-timeout SECONDS] [--effort LEVEL]",
        "                 [--no-plugins] [--no-plugin-hooks] [--no-remote]",
        "                 [--print PROMPT|- [--output text|json] | --rpc]",
        "       octet hub [serve|devices|revoke NAME|watch] (the phone hub; octet hub --help)",
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
/// Where journals and the hub's files go: `journal_dir`, else `$XDG_DATA_HOME`, else `~/.local/share`.
fn data_dir(journal_dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
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
                HubAction::Serve => hub::serve(directory, args.port, args.owner).await,
                HubAction::Devices => hub::devices(&directory).await,
                HubAction::Revoke(name) => hub::revoke(&directory, &name).await,
                HubAction::Watch => hub::watch(&directory).await,
            };
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
    let config = args.config(args::workspace(&cwd)?);
    let directory = data_dir(args.journal_dir.clone())?;
    let result = match args.run {
        Run::Interface => octet_tui::run(
            config,
            directory,
            octet_tui::Options {
                remote: args.remote,
                from_phone: args.from_phone,
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
