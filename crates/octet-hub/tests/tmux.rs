//! `start` against a fake `tmux` first on `PATH`: a script that records its arguments and exits 0.
//! This is its own test binary with one test, so changing `PATH` before anything else runs
//! cannot race another test.
use octet_core::remote::run::Registration;
use octet_hub::Hub;
use octet_testkit::relay::{Phone, Relay, control, pair};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::UnixListener, sync::oneshot, time::timeout};

const WAIT: Duration = Duration::from_secs(5);
const CAP: &str = "8 sessions started from the phone are running; end one first";

async fn send(phone: &mut Phone, message: Value) {
    phone.send(&message).await;
}

/// The next frame of type `kind`.
async fn recv_kind(phone: &mut Phone, kind: &str) -> Value {
    phone.recv_kind(kind).await
}

fn start_command(req: u64, workspace: &str) -> Value {
    json!({"v": 1, "req": req, "type": "start", "engine": "demo", "workspace": workspace})
}

/// The next `ack` or `refused`.
async fn answer(phone: &mut Phone) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let frame = recv_kind_any(phone).await;
            if frame["type"] == "ack" || frame["type"] == "refused" {
                return frame;
            }
        }
    })
    .await
    .expect("an answer")
}

async fn start(phone: &mut Phone, req: u64, workspace: &str) -> Value {
    send(phone, start_command(req, workspace)).await;
    answer(phone).await
}

async fn recv_kind_any(phone: &mut Phone) -> Value {
    phone.next().await.expect("a frame, not the end")
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one story with one fake tmux and one process-wide PATH"
)]
fn a_started_session_counts_until_it_registers() {
    let root = PathBuf::from(format!("/tmp/oht{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    let run = root.join("run");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(&run).expect("run");
    let make = |name: &str| {
        let path = root.join(name);
        std::fs::create_dir_all(&path).expect("workspace");
        std::fs::canonicalize(path).expect("canonical")
    };
    let workspace = make("w");
    let slow = [make("slow1"), make("slow2")];
    let log = root.join("calls");
    std::fs::create_dir_all(&log).expect("calls");
    let script = bin.join("tmux");
    // Logs its arguments, one file a call. `new-session` in a `slow` workspace never finishes, and only the first
    // `kill-session` works.
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{log}'/$$\n\
         case \"$1$6\" in new-session*/slow*) exec sleep 30;; esac\n\
         if [ \"$1\" = kill-session ]; then\n\
         mkdir '{marker}' 2>/dev/null || exit 1\nfi\n",
        log = log.display(),
        marker = root.join("killed").display(),
    );
    std::fs::write(&script, body).expect("fake tmux");
    std::fs::set_permissions(&script, PermissionsExt::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: this binary has one test and nothing else has started threads that read the
    // environment; the runtime below is built after this call.
    unsafe { std::env::set_var("PATH", path) };
    std::fs::write(
        root.join("hub.json"),
        json!({"workspaces": [&workspace, &slow[0], &slow[1]]}).to_string(),
    )
    .expect("hub.json");
    std::fs::write(
        root.join("session-1-1.jsonl"),
        "{\"format\":\"octet-preview-1\",\"seq\":0,\"type\":\"text\",\"data\":{}}\n",
    )
    .expect("journal");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (stop, stopped) = oneshot::channel::<()>();
        let data = root.clone();
        let relay = Relay::start().await;
        let url = relay.url();
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            let log = Arc::new(|_: &str| {});
            Hub::serve_with(data, url, log, shutdown, oneshot::channel().0).await
        });
        timeout(WAIT, async {
            while control(&run, &json!({"type": "status"})).await.is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
        let (link, key, _) = pair(&run, "Pixel").await;
        let mut phone = Phone::connect(&link, &key).await.expect("connects");
        send(&mut phone, json!({"v": 1, "req": 2, "type": "hello"})).await;
        recv_kind(&mut phone, "welcome").await;
        let place = workspace.to_string_lossy().into_owned();

        // A tmux that never answers is given up on after 5 seconds, and the session it may have
        // made is stopped; when that fails too, the phone is told it may still be starting.
        let (first, second) = (slow[0].to_string_lossy(), slow[1].to_string_lossy());
        send(&mut phone, start_command(30, &first)).await;
        send(&mut phone, start_command(31, &second)).await;
        let mut reasons = Vec::new();
        for _ in 0..2 {
            let answer = answer(&mut phone).await;
            assert_eq!(answer["type"], "refused", "{answer}");
            reasons.push(answer["reason"].as_str().expect("a reason").to_owned());
        }
        reasons.sort();
        assert_eq!(
            reasons,
            [
                "The session may still be starting",
                "tmux didn't answer in 5 seconds"
            ]
        );

        // Seven starts are accepted: the ack, the audit line and the tmux arguments.
        for req in 10..17 {
            let answer = start(&mut phone, req, &place).await;
            assert_eq!(answer["type"], "ack", "{answer}");
            if req == 10 {
                let text = answer["message"].as_str().expect("a message").to_owned();
                let name = text
                    .strip_prefix("Starting ")
                    .and_then(|rest| rest.strip_suffix(&format!(" in {place}")))
                    .unwrap_or_else(|| panic!("{text}"))
                    .to_owned();
                assert!(name.starts_with("octet-") && name.len() == 12, "{name}");
                let calls: Vec<String> = std::fs::read_dir(&log)
                    .expect("tmux was run")
                    .map(|file| {
                        std::fs::read_to_string(file.expect("a call").path()).expect("read")
                    })
                    .collect();
                let lines: Vec<&str> = calls
                    .iter()
                    .map(|call| call.lines().collect::<Vec<_>>())
                    .find(|call| call.get(3) == Some(&name.as_str()))
                    .unwrap_or_else(|| panic!("no call for {name} in {calls:?}"));
                assert_eq!(
                    lines[..6],
                    [
                        "new-session",
                        "-d",
                        "-s",
                        name.as_str(),
                        "-c",
                        place.as_str()
                    ]
                );
                let flag = lines.iter().position(|l| *l == "--remote-started");
                let flag = flag.unwrap_or_else(|| panic!("{lines:?}"));
                assert_eq!(
                    lines[flag + 1..flag + 4],
                    [name.as_str(), "--remote-by", "Pixel"]
                );
                let audit = std::fs::read_to_string(root.join("hub-audit.jsonl")).expect("audit");
                let line: Value =
                    serde_json::from_str(audit.lines().last().expect("a line")).expect("json");
                assert_eq!(line["session"], name.as_str());
                assert_eq!(line["device"], "Pixel");
                assert_eq!(
                    line["what"],
                    format!("start demo {place} resume=no mode=default")
                );
            }
        }

        // One of them registers: it is counted once, so there is room for exactly one more.
        let registration = Registration {
            id: "6001-1790000000".into(),
            pid: 6001,
            started: 1_790_000_000,
            socket: run.join("6001.sock"),
            engine: "demo".into(),
            workspace: workspace.clone(),
            model: String::new(),
            mode: "ask".into(),
            vendor_session: String::new(),
            journals: vec![root.join("session-1-1.jsonl")],
            from_phone: true,
            tmux: Some("octet-000001".into()),
        };
        let _listener = UnixListener::bind(&registration.socket).expect("bridge socket");
        registration.write(&run).await.expect("registration");
        timeout(WAIT, async {
            loop {
                let sessions = recv_kind(&mut phone, "sessions").await;
                if sessions["sessions"][0]["from_phone"] == true {
                    return;
                }
            }
        })
        .await
        .expect("the hub lists the registered session");
        let eighth = start(&mut phone, 20, &place).await;
        assert_eq!(eighth["type"], "ack", "{eighth}");
        let ninth = start(&mut phone, 21, &place).await;
        assert_eq!(ninth["type"], "refused", "{ninth}");
        assert_eq!(ninth["reason"], CAP);
        assert_eq!(ninth["code"], "busy");
        let _ = stop.send(());
    });
    let _ = std::fs::remove_dir_all(&root);
}
