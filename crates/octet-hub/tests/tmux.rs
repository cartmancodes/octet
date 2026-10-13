//! `start` against a fake `tmux` first on `PATH`: a script that records its arguments and exits 0.
//! This is its own test binary with one test, so changing `PATH` before anything else runs
//! cannot race another test.
use futures_util::{SinkExt, StreamExt};
use octet_core::remote::run::Registration;
use octet_hub::Hub;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpStream, UnixListener, UnixStream},
    sync::oneshot,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const OWNER: &str = "owner@example";
const WAIT: Duration = Duration::from_secs(5);
const CAP: &str = "8 sessions started from the phone are running; end one first";

type Phone = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn send(phone: &mut Phone, message: Value) {
    let sent = timeout(WAIT, phone.send(Message::text(message.to_string()))).await;
    sent.expect("send in time").expect("send");
}

/// The next frame of type `kind`.
async fn recv_kind(phone: &mut Phone, kind: &str) -> Value {
    timeout(WAIT, async {
        loop {
            match phone.next().await {
                Some(Ok(Message::Text(text))) => {
                    let frame: Value = serde_json::from_str(text.as_str()).expect("a frame");
                    if frame["type"] == kind {
                        return frame;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("expected a frame, got {other:?}"),
            }
        }
    })
    .await
    .expect("a frame arrives")
}

async fn control(run: &std::path::Path, request: Value) -> Option<Value> {
    let stream = UnixStream::connect(run.join("hub.sock")).await.ok()?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{request}\n").as_bytes())
        .await
        .ok()?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await.ok()?;
    serde_json::from_str(&line).ok()
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
    loop {
        match phone.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str()).expect("a frame");
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }
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
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            Hub::serve_with(
                data,
                0,
                OWNER.to_owned(),
                shutdown,
                oneshot::channel().0,
                (
                    |_| Box::pin(async { Some("https://mac.tail1234.ts.net".to_owned()) }),
                    Duration::from_secs(10),
                ),
            )
            .await
        });
        let status = timeout(WAIT, async {
            loop {
                if let Some(status) = control(&run, json!({"type": "status"})).await {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
        let port = status["port"].as_u64().expect("a port");
        let secret = control(&run, json!({"type": "pair_secret"}))
            .await
            .expect("a secret");
        let connect = || async {
            let mut request = format!("ws://127.0.0.1:{port}/")
                .into_client_request()
                .expect("request");
            request
                .headers_mut()
                .insert("Tailscale-User-Login", OWNER.parse().expect("header"));
            let (phone, _) = timeout(WAIT, connect_async(request))
                .await
                .expect("in time")
                .expect("connects");
            phone
        };
        let mut pairing = connect().await;
        send(
            &mut pairing,
            json!({"v": 1, "req": 1, "type": "pair", "secret": secret["secret"], "name": "Pixel",
                "platform": "android"}),
        )
        .await;
        let paired = recv_kind(&mut pairing, "paired").await;
        let mut phone = connect().await;
        send(
            &mut phone,
            json!({"v": 1, "req": 2, "type": "hello", "token": paired["token"]}),
        )
        .await;
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
