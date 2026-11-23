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
    for word in ["serve", "devices", "revoke", "watch", "install"] {
        assert!(help.contains(word), "{word} missing from: {help}");
    }
    // The hub listens on no port and knows nothing of a tailnet.
    for gone in ["--port", "--owner", "tailscale", "tailnet"] {
        assert!(!help.contains(gone), "{gone} in: {help}");
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
fn hub_port_and_owner_are_usage_errors() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    for (flag, value) in [("--port", "47823"), ("--owner", "me@example.com")] {
        let output = hub(&[flag, value], dir.path());
        assert_eq!(output.status.code(), Some(2), "{flag}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("Unknown argument {flag}")),
            "{flag}"
        );
    }
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
        .pair(
            &secret,
            "My phone",
            "ios",
            &"ab".repeat(32),
            "wss://relay.example",
            now,
        )
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

#[tokio::test]
async fn hub_devices_and_revoke_work_with_a_broken_hub_json_and_revoke_is_audited() {
    let dir = octet_testkit::TempDir::new("octet-hub-cli");
    let mut store = octet_hub::Store::open(dir.path()).await.unwrap();
    let now = std::time::SystemTime::now();
    let secret = store.pairing_secret(now);
    let device = store
        .pair(
            &secret,
            "Lost phone",
            "ios",
            &"cd".repeat(32),
            "wss://relay.example",
            now,
        )
        .await
        .unwrap();
    // A typo in the hub's settings must not stand between a stolen phone and its revoke.
    std::fs::write(dir.path().join("hub.json"), "{\"workspaces\": [").unwrap();
    let path = dir.path().to_owned();
    let (listed, revoked) = tokio::task::spawn_blocking(move || {
        (
            hub(&["devices"], &path),
            hub(&["revoke", "Lost phone"], &path),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        listed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(String::from_utf8_lossy(&listed.stdout).contains("Lost phone · ios"));
    assert_eq!(
        revoked.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    let audit = std::fs::read_to_string(dir.path().join("hub-audit.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
    assert_eq!(last["device"], "Lost phone");
    assert_eq!(last["what"], "revoked");
    assert!(!audit.contains(&device.key), "{audit}");
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
    let mut serve = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["hub", "serve", "--journal-dir"])
        .arg(&dir.0)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = octet_testkit::line_reader(serve.stdout.take().unwrap());
    let first = lines.recv_timeout(octet_testkit::QUICK).unwrap();
    assert_eq!(
        first,
        "Octet hub running; phones reach it through the relay once one is paired"
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

#[test]
fn pair_help_and_usage_errors_reach_the_shell() {
    let help = Command::new(env!("CARGO_BIN_EXE_octet"))
        .args(["pair", "--help"])
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("Usage: octet pair"), "{text}");
    for gone in ["--port", "--owner", "tailscale", "tailnet"] {
        assert!(!text.contains(gone), "{gone} in: {text}");
    }
    let main = Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&main.stdout).contains("octet pair"));
    // Refused before anything starts: no hub, no service.
    let dir = octet_testkit::TempDir::new("octet-pair-cli");
    for flag in ["--port", "--owner"] {
        let output = Command::new(env!("CARGO_BIN_EXE_octet"))
            .args(["pair", flag, "x", "--journal-dir"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{flag}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("Unknown argument {flag}")),
            "{flag}"
        );
        assert!(!dir.path().exists(), "{flag} touched the data directory");
    }
}

/// A small workflow: one document and its review, a change, its tests and
/// its review. Every profile runs `binary`; the coder may write.
fn fleet_workflow(binary: &std::path::Path) -> String {
    format!(
        "version: 1\n\
name: small\n\
limits: {{ max_rounds: 1, transient_retries: 1 }}\n\
profiles:\n\
\x20 doc: {{ engine: codex, binary: '{binary}' }}\n\
\x20 code: {{ engine: codex, binary: '{binary}', mode: full-access }}\n\
roles:\n\
\x20 researcher: {{ kind: document, instructions: Research the task. }}\n\
\x20 reviewer: {{ kind: verdict, instructions: Review the artifact. }}\n\
\x20 coder: {{ kind: change, instructions: Implement the research. }}\n\
\x20 tester: {{ kind: tests, instructions: Test the change. }}\n\
steps:\n\
\x20 - {{ id: research, role: researcher, profile: doc }}\n\
\x20 - {{ id: review_research, role: reviewer, profile: doc, reviews: research }}\n\
\x20 - id: implement\n\
\x20   role: coder\n\
\x20   profile: code\n\
\x20   inputs: [{{ from: research, approval_from: review_research }}]\n\
\x20 - {{ id: test, role: tester, profile: code, change_from: implement, commands: [\"true\"] }}\n\
\x20 - {{ id: review_change, role: reviewer, profile: doc, reviews: implement, evidence_from: test }}\n",
        binary = binary.display()
    )
}

/// A git work tree with one commit.
fn fleet_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.name", "Tester"],
        &["config", "user.email", "tester@example.com"],
        &["config", "commit.gpgsign", "false"],
    ] {
        git_in(dir, args);
    }
    std::fs::write(dir.join("README"), "a repository\n").unwrap();
    git_in(dir, &["add", "-A"]);
    git_in(dir, &["commit", "--quiet", "-m", "base"]);
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `octet fleet ARGS` with stdin closed and `data` as its data directory.
fn fleet_cli(args: &[&str], data: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_octet"))
        .arg("fleet")
        .args(args)
        .arg("--journal-dir")
        .arg(data)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn fleet_validate_reports_every_error() {
    let dir = octet_testkit::TempDir::new("octet-fleet-validate");
    std::fs::create_dir_all(dir.path()).unwrap();
    let workflow = dir.path().join("broken.yaml");
    std::fs::write(
        &workflow,
        "version: 2\n\
name: broken\n\
profiles:\n\
\x20 p: { engine: codex, binary: /bin/sh }\n\
roles:\n\
\x20 r: { kind: document, instructions: x }\n\
steps:\n\
\x20 - { id: a, role: nope, profile: p }\n\
\x20 - { id: b, role: r, profile: missing }\n",
    )
    .unwrap();
    let output = fleet_cli(
        &["validate", workflow.to_str().unwrap()],
        &dir.path().join("data"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    for error in [
        "version: must be 1, found 2",
        "step a: role nope is not defined",
        "step b: profile missing is not defined",
    ] {
        assert!(stderr.contains(error), "{error} missing from: {stderr}");
    }
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    // A workflow with no errors validates, and starts nothing.
    let fine = dir.path().join("fine.yaml");
    std::fs::write(&fine, fleet_workflow(std::path::Path::new("/bin/sh"))).unwrap();
    let output = fleet_cli(
        &["validate", fine.to_str().unwrap()],
        &dir.path().join("data"),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("small"));
}

#[test]
fn fleet_run_without_trust_off_a_terminal_is_refused() {
    let dir = octet_testkit::TempDir::new("octet-fleet-trust");
    let work = dir.path().join("work");
    fleet_repo(&work);
    let workflow = dir.path().join("small.yaml");
    std::fs::write(&workflow, fleet_workflow(std::path::Path::new("/bin/sh"))).unwrap();
    let data = dir.path().join("data");
    let output = fleet_cli(
        &[
            "run",
            workflow.to_str().unwrap(),
            "--task",
            "Add the feature.",
            "--cwd",
            work.to_str().unwrap(),
        ],
        &data,
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--trust"), "{stderr}");
    // What would run is shown, so the refusal can be judged.
    assert!(
        stderr.contains("/bin/sh") && stderr.contains("true"),
        "{stderr}"
    );
    // Nothing started: no run directory, no trust record.
    assert!(
        !data.join("fleet").exists(),
        "{:?}",
        std::fs::read_dir(&data)
    );
}

/// A verdict reply approving, with no findings.
fn fleet_approve() -> String {
    "Looks right.\n\n```json\n{\"verdict\": \"approved\", \"new_findings\": [], \"previous\": []}\n```\n".into()
}

#[test]
fn vendor_text_is_plain() {
    let dir = octet_testkit::TempDir::new("octet-fleet-plain");
    let work = dir.path().join("work");
    fleet_repo(&work);
    let script = dir.path().join("script.json");
    let turns = serde_json::json!({
        "research#1": {"reply": "Findings \u{1b}]0;x\u{7} end of findings"},
        "review_research#1": {"reply": fleet_approve()},
        "implement#1": {"write": {"feature.txt": "the feature\n"}, "reply": "Added feature.txt."},
        "test#1": {"write": {"feature_test.sh": "test -f feature.txt\n"}, "reply": "Added a test."},
        "review_change#1": {"reply": fleet_approve()},
    });
    std::fs::write(
        &script,
        serde_json::json!({"log": dir.path().join("turns.log"), "turns": turns}).to_string(),
    )
    .unwrap();
    let vendor = octet_testkit::write_script(
        &dir.path().join("bin"),
        "vendor",
        &format!(
            "OCTET_FLEET_SCRIPT='{}' exec '{}' \"$@\"",
            script.display(),
            octet_testkit::protocol_child().display()
        ),
    );
    let workflow = dir.path().join("small.yaml");
    std::fs::write(&workflow, fleet_workflow(&vendor)).unwrap();
    let output = fleet_cli(
        &[
            "run",
            workflow.to_str().unwrap(),
            "--task",
            "Add the feature.",
            "--cwd",
            work.to_str().unwrap(),
            "--trust",
        ],
        &dir.path().join("data"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(!stdout.contains("\u{1b}]0;x\u{7}"), "{stdout:?}");
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}'),
        "{stdout:?}"
    );
    assert!(stdout.contains("Findings ]0;x end of findings"), "{stdout}");
    assert!(stdout.contains("▸ research · round 1 · codex"), "{stdout}");
    assert!(stdout.contains("git diff "), "{stdout}");
    // The run is on record.
    let second = fleet_cli(&["status"], &dir.path().join("data"));
    assert!(String::from_utf8_lossy(&second.stdout).contains("completed"));
}
