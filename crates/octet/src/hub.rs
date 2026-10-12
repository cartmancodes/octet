//! `octet hub`: serving the phone hub, listing and revoking its devices, and watching sessions.
use crate::args::CliError;
use octet_hub::{Device, Hub, Store, plain};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::oneshot,
};

/// Longest device name or platform printed, in characters.
const NAME_CHARS: usize = 64;

fn failed(message: impl Into<String>) -> CliError {
    CliError::Run(message.into())
}

/// Resolves on SIGINT or SIGTERM.
async fn interrupted() -> Result<(), CliError> {
    let mut term = signal(SignalKind::terminate()).map_err(|e| failed(e.to_string()))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| failed(e.to_string()))?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    Ok(())
}

/// Runs the hub until SIGINT or SIGTERM, saying where it listens and whether it is published.
pub(crate) async fn serve(
    directory: PathBuf,
    port: Option<u16>,
    owner: Option<String>,
) -> Result<i32, CliError> {
    let port = match port {
        Some(port) => port,
        None => open(&directory).await?.config().port,
    };
    let owner = octet_hub::owner_login(owner).await.map_err(failed)?;
    // Signals are caught before the hub starts, so none is lost in between.
    let stop = interrupted();
    let (ready, bound) = oneshot::channel();
    let shutdown = async {
        let _ = stop.await;
    };
    let serving = Hub::serve_notify(directory, port, owner, shutdown, ready);
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result.map(|()| 0).map_err(failed),
        Ok(port) = bound => {
            let announce = announce(port);
            let (result, ()) = tokio::join!(serving, announce);
            result.map(|()| 0).map_err(failed)
        }
    }
}

/// Prints where the hub listens, then where it is published or how to publish it.
async fn announce(port: u16) {
    println!("Octet hub on 127.0.0.1:{port}");
    match octet_hub::published_url(port).await {
        Some(url) => println!("Published at {url}"),
        None => println!("Publish it on your tailnet: tailscale serve --bg {port}"),
    }
    let _ = std::io::stdout().flush();
}

async fn open(directory: &Path) -> Result<Store, CliError> {
    Store::open(directory).await.map_err(|e| {
        failed(format!(
            "Couldn't open the hub's files in {}: {e}",
            directory.display()
        ))
    })
}

/// Prints the paired devices, one per line.
pub(crate) async fn devices(directory: &Path) -> Result<i32, CliError> {
    let store = open(directory).await?;
    let now = SystemTime::now();
    if store.devices().is_empty() {
        println!("No paired devices");
    }
    for device in store.devices() {
        println!("{}", device_line(device, now));
    }
    Ok(0)
}

/// Unpairs the device called `name`; a running hub notices within 2 seconds.
pub(crate) async fn revoke(directory: &Path, name: &str) -> Result<i32, CliError> {
    let mut store = open(directory).await?;
    let shown = plain(name, NAME_CHARS);
    if !store.revoke(name).await {
        return Err(failed(format!("No device named {shown}")));
    }
    println!("Revoked {shown}");
    Ok(0)
}

/// Prints every session's activity until Ctrl+C.
pub(crate) async fn watch(directory: &Path) -> Result<i32, CliError> {
    let printing = octet_hub::watch(directory, |line| {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
    });
    tokio::select! {
        result = printing => result.map(|()| 0).map_err(failed),
        _ = tokio::signal::ctrl_c() => Ok(0),
    }
}

/// One device as a line: name, platform, when it paired and when it was last seen.
fn device_line(device: &Device, now: SystemTime) -> String {
    let at = |seconds: u64| UNIX_EPOCH + Duration::from_secs(seconds);
    format!(
        "{} · {} · paired {} · last seen {}",
        plain(&device.name, NAME_CHARS),
        plain(&device.platform, NAME_CHARS),
        date(device.created),
        ago(now, at(device.last_seen)),
    )
}

/// How long ago `then` was, in the largest whole unit; a time ahead of the clock is "just now".
fn ago(now: SystemTime, then: SystemTime) -> String {
    let seconds = now.duration_since(then).map_or(0, |age| age.as_secs());
    match seconds {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// `YYYY-MM-DD` (UTC) of `seconds` since the epoch.
fn date(seconds: u64) -> String {
    // Civil-from-days, for days since 1970-01-01 (Howard Hinnant's algorithm).
    let days = i64::try_from(seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_calendar_days_in_utc() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_790_000_000), "2026-09-21");
        assert_eq!(date(1_789_948_800 + 86_399), "2026-09-21");
        assert_eq!(date(1_789_948_800 + 86_400), "2026-09-22");
    }

    #[test]
    fn a_device_line_says_who_when_and_how_long_ago() {
        let device = Device {
            name: "My\u{1b}[2J phone".into(),
            platform: "ios".into(),
            login: "me@example.com".into(),
            created: 1_790_000_000,
            last_seen: 1_790_000_000 - 1,
            token_sha256: String::new(),
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_790_000_000 + 7_200);
        assert_eq!(
            device_line(&device, now),
            "My[2J phone · ios · paired 2026-09-21 · last seen 2 h ago"
        );
    }
}
