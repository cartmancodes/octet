//! Push end to end: a fake bridge opens approvals, the hub posts to a local HTTP fake with the
//! real `curl`, and a stand-in `curl` first on `PATH` records its argv and stdin before running it.
//! This is its own test binary with one test, so changing `PATH` and `OCTET_HUB_PUSH_URL` before
//! anything else runs cannot race another test. A release build ignores `OCTET_HUB_PUSH_URL`, so
//! the test runs in debug builds only.
#![cfg(debug_assertions)]
use futures_util::{SinkExt, StreamExt};
use octet_core::remote::{
    protocol::{ack, bridge_state_frame},
    run::Registration,
};
use octet_hub::Hub;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpListener, TcpStream, UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const OWNER: &str = "owner@example";
const WAIT: Duration = Duration::from_secs(5);
/// How long a push may take: the 2 s window, the hub's 1 s sweep, and `curl`.
const PUSH_WITHIN: Duration = Duration::from_secs(10);
const JOURNAL: &str = "session-1-1.jsonl";

type Phone = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The local stand-in for Expo: it keeps every body posted, and answers `DeviceNotRegistered`
/// for any message to a token containing `gone`, `ok` for the rest.
#[derive(Clone, Default)]
struct Fake {
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// The messages posted so far to `token`.
    fn to(&self, token: &str) -> Vec<Value> {
        let bodies = self.bodies.lock().expect("lock").clone();
        bodies
            .iter()
            .flat_map(|body| {
                let batch: Value = serde_json::from_str(body).expect("a JSON array");
                batch.as_array().expect("an array").clone()
            })
            .filter(|message| message["to"] == token)
            .collect()
    }

    /// The `n`th message to `token`, once it arrives.
    async fn wait_for(&self, token: &str, n: usize) -> Value {
        timeout(PUSH_WITHIN, async {
            loop {
                if let Some(message) = self.to(token).get(n) {
                    return message.clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a push arrives")
    }

    async fn serve(self, listener: TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(self.clone().answer(stream));
        }
    }

    /// Reads one HTTP/1.1 POST and answers it.
    async fn answer(self, mut stream: TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let end = loop {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
        assert!(head.starts_with("post "), "{head}");
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
            .unwrap_or(0_usize);
        if head.contains("expect: 100-continue") {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        while buffer.len() < end + length {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8(buffer[end..end + length].to_vec()).expect("UTF-8");
        let messages: Value = serde_json::from_str(&body).expect("a JSON array");
        let tickets: Vec<Value> = messages
            .as_array()
            .expect("an array")
            .iter()
            .map(|message| {
                if message["to"].as_str().is_some_and(|to| to.contains("gone")) {
                    json!({"status": "error", "message": "not registered",
                        "details": {"error": "DeviceNotRegistered"}})
                } else {
                    json!({"status": "ok", "id": "ticket"})
                }
            })
            .collect();
        self.bodies.lock().expect("lock").push(body);
        let answer = json!({"data": tickets}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{answer}",
            answer.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

/// A data directory, the hub serving it and its fake bridge.
struct Env {
    root: PathBuf,
    run: PathBuf,
    port: u16,
    bridge: UnixListener,
    id: String,
    stop: Option<oneshot::Sender<()>>,
}

impl Env {
    async fn start(root: PathBuf, pid: u32) -> Self {
        let _ = std::fs::remove_dir_all(&root);
        let run = root.join("run");
        std::fs::create_dir_all(&run).expect("run directory");
        let journal = root.join(JOURNAL);
        std::fs::write(
            &journal,
            "{\"format\":\"octet-preview-1\",\"seq\":0,\"type\":\"text\",\"data\":{}}\n",
        )
        .expect("journal");
        let registration = Registration {
            id: format!("{pid}-1790000000"),
            pid,
            started: 1_790_000_000,
            socket: run.join(format!("{pid}.sock")),
            engine: "claude".into(),
            workspace: "/work".into(),
            model: "opus".into(),
            mode: "ask".into(),
            vendor_session: String::new(),
            journals: vec![journal],
            from_phone: false,
            tmux: None,
        };
        let bridge = UnixListener::bind(&registration.socket).expect("bridge socket");
        registration.write(&run).await.expect("registration");
        let (stop, stopped) = oneshot::channel::<()>();
        let directory = root.clone();
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            Hub::serve_with(
                directory,
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
        let mut env = Self {
            root,
            run,
            port: 0,
            bridge,
            id: registration.id,
            stop: Some(stop),
        };
        let status = timeout(WAIT, async {
            loop {
                if let Some(status) = env.control(&json!({"type": "status"})).await {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
        env.port = u16::try_from(status["port"].as_u64().expect("a port")).expect("a port");
        env
    }

    async fn control(&self, request: &Value) -> Option<Value> {
        let stream = UnixStream::connect(self.run.join("hub.sock")).await.ok()?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.ok()?;
        serde_json::from_str(&line).ok()
    }

    /// The hub's link to the bridge, once it is made.
    async fn link(&self) -> Link {
        let (stream, _) = timeout(WAIT, self.bridge.accept())
            .await
            .expect("the hub connects to the bridge")
            .expect("accept");
        let (read, write) = stream.into_split();
        Link {
            _read: BufReader::new(read),
            write,
        }
    }

    /// A phone called `name` that paired, said hello and turned on push for `token`.
    async fn phone(&self, name: &str, token: &str) -> Phone {
        let mut phone = self.connect().await;
        let secret = self
            .control(&json!({"type": "pair_secret"}))
            .await
            .expect("a secret");
        send(
            &mut phone,
            json!({"v": 1, "req": 1, "type": "pair", "secret": secret["secret"],
                "name": name, "platform": "ios"}),
        )
        .await;
        let paired = recv_kind(&mut phone, "paired").await;
        let device_token = paired["token"].as_str().expect("a token").to_owned();
        let mut phone = self.connect().await;
        send(
            &mut phone,
            json!({"v": 1, "req": 2, "type": "hello", "token": device_token}),
        )
        .await;
        recv_kind(&mut phone, "welcome").await;
        send(
            &mut phone,
            json!({"v": 1, "req": 3, "type": "register_push", "token": token}),
        )
        .await;
        assert_eq!(
            recv_kind(&mut phone, "ack").await,
            ack(3, "Notifications on")
        );
        phone
    }

    async fn connect(&self) -> Phone {
        let mut request = format!("ws://127.0.0.1:{}/", self.port)
            .into_client_request()
            .expect("a request");
        request
            .headers_mut()
            .insert("Tailscale-User-Login", OWNER.parse().expect("a header"));
        let (phone, _) = timeout(WAIT, connect_async(request))
            .await
            .expect("the handshake ends")
            .expect("connects");
        phone
    }

    fn devices(&self) -> String {
        std::fs::read_to_string(self.root.join("devices.json")).expect("devices.json")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The hub's connection to a fake bridge.
struct Link {
    _read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
}

impl Link {
    /// Tells the hub the session's state: `approvals` are open, the phase is `phase`.
    async fn state(&mut self, phase: &str, approvals: &[u64]) {
        let approvals: Vec<Value> = approvals
            .iter()
            .map(|id| {
                json!({"id": id, "summary": {"action": "Run rm -rf /secret-dir",
                    "files": ["/work/private.txt"], "scope": "This request only, once"}})
            })
            .collect();
        let state = json!({"phase": phase, "queue": [], "queued": 0, "mode": "ask",
            "approvals": approvals});
        self.write
            .write_all(format!("{}\n", bridge_state_frame(&state)).as_bytes())
            .await
            .expect("bridge write");
    }
}

async fn send(phone: &mut Phone, message: Value) {
    let sent = timeout(WAIT, phone.send(Message::text(message.to_string()))).await;
    sent.expect("send in time").expect("send");
}

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

/// Closes the phone's socket, as the app does in the background, and gives the hub a moment.
async fn background(mut phone: Phone) {
    let _ = timeout(WAIT, phone.close(None)).await;
    drop(phone);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Waits `ms` and counts the messages to `token`.
async fn pushes_after(fake: &Fake, token: &str, wait: Duration) -> usize {
    tokio::time::sleep(wait).await;
    fake.to(token).len()
}

/// What a push looks like on the phone: exactly these keys, nothing from the session.
fn approval_message(token: &str, session: &str) -> Value {
    json!({"to": token, "title": "Octet", "body": "Approval waiting",
        "data": {"session": session}, "sound": "default", "priority": "high",
        "channelId": "approvals"})
}

/// A push that has no word of the approval, the workspace, the model or the engine.
fn assert_content_free(fake: &Fake) {
    let all = fake.bodies.lock().expect("lock").join("\n");
    for secret in ["/work", "rm -rf", "secret-dir", "private", "opus", "claude"] {
        assert!(!all.contains(secret), "{secret} in {all}");
    }
}

/// The first `curl` on the original `PATH`, before the stand-in is put in front of it.
fn real_curl() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("curl"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .expect("a curl on PATH")
}

/// The stand-in `curl`'s records: `(argv lines, stdin)` for each call.
fn curl_calls(log: &Path) -> Vec<(Vec<String>, String)> {
    let mut calls = Vec::new();
    for entry in std::fs::read_dir(log).expect("the log").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(call) = name.strip_prefix("argv.") else {
            continue;
        };
        let argv = std::fs::read_to_string(entry.path()).expect("argv");
        let stdin = std::fs::read_to_string(log.join(format!("stdin.{call}"))).expect("stdin");
        calls.push((argv.lines().map(str::to_owned).collect(), stdin));
    }
    calls
}

#[test]
fn push_goes_out_content_free_to_a_phone_in_the_background() {
    let root = PathBuf::from(format!("/tmp/ohp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    let log = root.join("curl");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(&log).expect("log");
    // Records its argv and its stdin apart, one file each a call, then runs the real curl.
    let script = bin.join("curl");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{log}'/argv.$$\ncat > '{log}'/stdin.$$\n\
         exec '{curl}' \"$@\" < '{log}'/stdin.$$\n",
        log = log.display(),
        curl = real_curl().display()
    );
    std::fs::write(&script, body).expect("fake curl");
    std::fs::set_permissions(&script, PermissionsExt::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the fake's port");
    listener.set_nonblocking(true).expect("non-blocking");
    let url = format!(
        "http://127.0.0.1:{}/--/api/v2/push/send",
        listener.local_addr().expect("an address").port()
    );
    // SAFETY: this binary has one test and nothing else has started threads that read the
    // environment; the runtime below is built after these calls.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("OCTET_HUB_PUSH_URL", &url);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let fake = Fake::default();
        tokio::spawn(
            fake.clone()
                .serve(TcpListener::from_std(listener).expect("the fake listens")),
        );
        // Independent hubs, devices and tokens, so the stories run side by side.
        tokio::join!(
            approval_then_turn(&fake, root.join("a"), &url, &log),
            connected_phones_are_left_alone(&fake, root.join("b")),
            a_dead_token_is_dropped(&fake, root.join("c")),
        );
        assert_content_free(&fake);
        // No call to curl, whoever made it, had a push token in its argv.
        let calls = curl_calls(&log);
        assert!(calls.len() >= 4, "{calls:?}");
        for (argv, _) in &calls {
            assert!(!argv.concat().contains("PushToken"), "{argv:?}");
        }
    });
    drop(runtime);
    let _ = std::fs::remove_dir_all(&root);
}

/// An approval pushes once to a phone in the background, with the message and nothing else, a
/// finished turn pushes once, and `curl` was given neither the token nor the message.
async fn approval_then_turn(fake: &Fake, root: PathBuf, url: &str, log: &Path) {
    const TOKEN: &str = "ExpoPushToken[story-a-token]";
    let env = Env::start(root, 6101).await;
    let mut link = env.link().await;
    let phone = env.phone("Story A phone", TOKEN).await;
    background(phone).await;
    // The first state seeds what the hub has seen; the approval after it is news.
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    assert_eq!(
        fake.wait_for(TOKEN, 0).await,
        approval_message(TOKEN, &env.id)
    );
    // The same approval again, and answered, are not news.
    link.state("idle", &[1]).await;
    link.state("running", &[]).await;
    link.state("idle", &[]).await;
    let turn = fake.wait_for(TOKEN, 1).await;
    assert_eq!(
        turn,
        json!({"to": TOKEN, "title": "Octet", "body": "Turn finished",
            "data": {"session": env.id}, "priority": "normal", "channelId": "turns"})
    );
    assert_eq!(pushes_after(fake, TOKEN, Duration::from_secs(3)).await, 2);

    // The token and the message reached curl on stdin, never in argv: one call each.
    let calls: Vec<_> = curl_calls(log)
        .into_iter()
        .filter(|(_, stdin)| stdin.contains(TOKEN))
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let mut batches: Vec<Value> = calls
        .iter()
        .map(|(_, stdin)| serde_json::from_str(stdin).expect("stdin is the JSON array"))
        .collect();
    batches.sort_by_key(|batch| batch[0]["body"].as_str().map(str::to_owned));
    assert_eq!(
        batches,
        [json!([approval_message(TOKEN, &env.id)]), json!([turn])]
    );
    for (argv, _) in &calls {
        assert_eq!(
            argv,
            &[
                // First, so curl reads no ~/.curlrc that could add options or log the body.
                "-q",
                "-sS",
                "--max-time",
                "10",
                "-H",
                "Content-Type: application/json",
                "-H",
                "Accept: application/json",
                "--data-binary",
                "@-",
                url
            ]
        );
    }
}

/// A phone with the app open gets nothing. When it goes to the background, the approval it was
/// spared and that is still open reaches it, once; then the next one does.
async fn connected_phones_are_left_alone(fake: &Fake, root: PathBuf) {
    const TOKEN: &str = "ExpoPushToken[story-b-token]";
    let env = Env::start(root, 6102).await;
    let mut link = env.link().await;
    let phone = env.phone("Story B phone", TOKEN).await;
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    assert_eq!(pushes_after(fake, TOKEN, Duration::from_secs(4)).await, 0);
    background(phone).await;
    assert_eq!(
        fake.wait_for(TOKEN, 0).await,
        approval_message(TOKEN, &env.id)
    );
    assert_eq!(pushes_after(fake, TOKEN, Duration::from_secs(3)).await, 1);
    link.state("idle", &[1, 2]).await;
    fake.wait_for(TOKEN, 1).await;
    assert_eq!(pushes_after(fake, TOKEN, Duration::from_secs(3)).await, 2);
}

/// Expo says the token is dead: it is cleared from `devices.json` and never tried again.
async fn a_dead_token_is_dropped(fake: &Fake, root: PathBuf) {
    const TOKEN: &str = "ExpoPushToken[story-c-gone]";
    let env = Env::start(root, 6103).await;
    let mut link = env.link().await;
    let phone = env.phone("Story C phone", TOKEN).await;
    assert!(env.devices().contains(TOKEN));
    background(phone).await;
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    fake.wait_for(TOKEN, 0).await;
    timeout(PUSH_WITHIN, async {
        while env.devices().contains("push_token") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the dead token is dropped");
    assert!(!env.devices().contains(TOKEN));
    // The next approval has nowhere to go.
    link.state("idle", &[1, 2]).await;
    assert_eq!(pushes_after(fake, TOKEN, Duration::from_secs(4)).await, 1);
}
