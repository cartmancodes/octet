//! The command line as a process: help and usage errors. Each message is
//! pinned by the parser's unit tests; these check what reaches the shell.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]
use std::process::Command;

/// The invalid-engine error, from the provider table.
fn engine_error() -> String {
    format!("Engine must be {}", octet_core::model::engine_choices())
}

#[test]
fn help_and_errors_name_exactly_the_providers() {
    let names: Vec<&str> = octet_core::Engine::ALL.iter().map(|e| e.as_str()).collect();
    let help = Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains(&format!("--engine {}", names.join("|"))),
        "{help}"
    );
    let bad = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["--engine", "nope"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bad.stderr).contains(&engine_error()));
}

#[test]
fn help_lists_every_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    for command in octet_tui::command_names() {
        assert!(help.contains(command), "{command} missing from --help");
    }
}

#[test]
fn usage_errors_exit_2() {
    // Each message is pinned by the parser's unit tests; here, the process.
    let output = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["--engine", "demo", "--mode", "bogus"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("octet: Unknown mode bogus"));
}

#[test]
fn help_lists_version() {
    let help = Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--version"), "{help}");
}

#[test]
fn help_lists_the_remote_switch() {
    let help = Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--no-remote"), "{help}");
}

/// Runs `octet hub ARGS --journal-dir DIR`.
fn hub(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("hub")
        .args(args)
        .arg("--journal-dir")
        .arg(dir)
        .output()
        .unwrap()
}

#[test]
fn hub_help_lists_the_actions() {
    let output = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["hub", "--help"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    for word in ["serve", "devices", "revoke", "watch", "--port", "--owner"] {
        assert!(help.contains(word), "{word} missing from: {help}");
    }
    let main = Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&main.stdout).contains("octet hub"));
}

#[test]
fn hub_devices_on_an_empty_store_says_so() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    let output = hub(&["devices"], dir.path());
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "No paired devices\n"
    );
}

#[test]
fn hub_revoke_of_an_unknown_device_fails() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    let output = hub(&["revoke", "nobody"], dir.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No device named nobody"));
}

#[test]
fn hub_port_zero_is_a_usage_error() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    let output = hub(&["--port", "0"], dir.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Port must be"));
}

#[test]
fn hub_watch_without_a_hub_says_how_to_start_one() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    let output = hub(&["watch"], dir.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("The hub isn't running; start it with: octet hub")
    );
}

#[tokio::test]
async fn hub_devices_lists_and_revokes_a_paired_device() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    let mut store = octet_hub::Store::open(dir.path()).await.unwrap();
    let now = std::time::SystemTime::now();
    let secret = store.pairing_secret(now);
    store
        .pair(&secret, "My phone", "ios", "me@example.com", now)
        .await
        .unwrap();
    let path = dir.path().to_owned();
    let (listed, revoked, after) = tokio::task::spawn_blocking(move || {
        (
            hub(&["devices"], &path),
            hub(&["revoke", "My phone"], &path),
            hub(&["devices"], &path),
        )
    })
    .await
    .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("My phone · ios · paired ") && listed.contains("last seen just now"),
        "{listed:?}"
    );
    assert_eq!(revoked.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&revoked.stdout).starts_with("Revoked "));
    assert_eq!(
        String::from_utf8_lossy(&after.stdout),
        "No paired devices\n"
    );
}

/// A short data directory under `/tmp` (socket paths are limited), removed on drop.
struct ShortDir(std::path::PathBuf);

impl Drop for ShortDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signal(child: &std::process::Child, signal: i32) {
    let pid = i32::try_from(child.id()).unwrap();
    // SAFETY: a plain kill(2) of a child this test spawned and has not yet waited for.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

#[test]
fn hub_serve_announces_itself_and_watch_follows_until_interrupted() {
    use std::process::Stdio;
    let dir = ShortDir(std::path::PathBuf::from(format!(
        "/tmp/ohcli{}",
        std::process::id()
    )));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut serve = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["hub", "serve", "--owner", "me@example.com", "--port"])
        .arg(port.to_string())
        .arg("--journal-dir")
        .arg(&dir.0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = octet_testkit::line_reader(serve.stdout.take().unwrap());
    let first = lines.recv_timeout(octet_testkit::QUICK).unwrap();
    assert_eq!(first, format!("Octet hub on 127.0.0.1:{port}"));
    let second = lines.recv_timeout(octet_testkit::QUICK).unwrap();
    assert!(
        second == format!("Publish it on your tailnet: tailscale serve --bg {port}")
            || second.starts_with("Published at https://"),
        "{second}"
    );

    let mut watch = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["hub", "watch", "--journal-dir"])
        .arg(&dir.0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let seen = octet_testkit::line_reader(watch.stdout.take().unwrap());
    assert_eq!(
        seen.recv_timeout(octet_testkit::QUICK).unwrap(),
        "No sessions"
    );
    signal(&watch, libc::SIGINT);
    assert_eq!(watch.wait().unwrap().code(), Some(0));

    signal(&serve, libc::SIGTERM);
    assert_eq!(serve.wait().unwrap().code(), Some(0));
}
