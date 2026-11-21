//! `octet-relay`: serves the relay on `PORT` (8080 unset), on every interface, until `SIGTERM` or
//! Ctrl-C. `OCTET_RELAY_CLIENT_ADDRESS_HEADER` names the header the proxy in front writes the
//! client's address to. `octet-relay probe <url>` checks a running relay end to end, and exits 0,
//! or prints why it failed and exits 1.
#![forbid(unsafe_code)]
use octet_relay::{Config, probe, serve};
use std::{env, net::Ipv4Addr, process::ExitCode};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
};

/// The port served when `PORT` is unset.
const DEFAULT_PORT: u16 = 8080;
/// What to type.
const USAGE: &str = "usage: octet-relay | octet-relay probe <url>";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [] => run().await,
        [command, url] if command == "probe" => match probe(url).await {
            Ok(()) => {
                println!("ok");
                ExitCode::SUCCESS
            }
            Err(why) => {
                eprintln!("octet-relay probe: {why}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Serves the relay as the environment says.
async fn run() -> ExitCode {
    let port = env::var("PORT").map_or(Ok(DEFAULT_PORT), |port| {
        port.trim().parse().map_err(|_| port)
    });
    let port = match port {
        Ok(port) => port,
        Err(port) => {
            eprintln!("octet-relay: PORT must be a port number, not {port:?}");
            return ExitCode::from(2);
        }
    };
    let client_address_header = env::var("OCTET_RELAY_CLIENT_ADDRESS_HEADER")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("octet-relay: cannot listen on port {port}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let config = Config {
        client_address_header,
        ..Config::default()
    };
    match serve(listener, config, stopped()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("octet-relay: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves at `SIGTERM` (how a platform stops a container) or Ctrl-C.
async fn stopped() {
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
