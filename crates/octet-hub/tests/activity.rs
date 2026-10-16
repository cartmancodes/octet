//! The Live Activity end to end: a fake bridge opens approvals, and the hub sends to a local
//! stand-in for APNs with the real `curl`, through the debug-only `OCTET_HUB_APNS_URL` seam. This
//! is its own test binary with one test, so setting the variable before anything else runs cannot
//! race another test; a release build ignores it, so the test runs in debug builds only. Nothing
//! here reaches a real APNs host.
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
    path::PathBuf,
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
/// How long a send may take: the hub's 1 s look, the 2 s between updates, and `curl`.
const SEND_WITHIN: Duration = Duration::from_secs(10);
const JOURNAL: &str = "session-1-1.jsonl";
/// The host every hub here is published at, as its activities name it.
const MAC: &str = "mac.tail1234.ts.net";

type Phone = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One request the stand-in for APNs took: its path, its header lines (lowercased) and its body.
#[derive(Clone, Debug)]
struct Request {
    path: String,
    head: Vec<String>,
    body: Value,
}

/// The local stand-in for APNs: it keeps every request, and answers 410 to a token that starts
/// with `dead`, 200 to the rest.
#[derive(Clone, Default)]
struct Fake {
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    /// The requests so far to `token`.
    fn to(&self, token: &str) -> Vec<Request> {
        let path = format!("/3/device/{token}");
        let requests = self.requests.lock().expect("lock");
        requests
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    /// The `n`th request to `token`, once it arrives.
    async fn wait_for(&self, token: &str, n: usize) -> Request {
        timeout(SEND_WITHIN, async {
            loop {
                if let Some(request) = self.to(token).get(n) {
                    return request.clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a send arrives")
    }

    async fn serve(self, listener: TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(self.clone().answer(stream));
        }
    }

    /// Reads one HTTP/1.1 POST and answers it.
    async fn answer(self, stream: TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end().to_owned();
            if line.is_empty() {
                break;
            }
            head.push(line);
        }
        let Some(path) = head
            .first()
            .and_then(|line| line.strip_prefix("POST "))
            .and_then(|rest| rest.split(' ').next())
            .map(str::to_owned)
        else {
            return;
        };
        let head: Vec<String> = head.iter().map(|line| line.to_lowercase()).collect();
        let length = head
            .iter()
            .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
            .unwrap_or(0_usize);
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let body = serde_json::from_slice(&body).expect("the body is JSON");
        let status = if path.starts_with("/3/device/dead") {
            "410 Gone"
        } else {
            "200 OK"
        };
        self.requests
            .lock()
            .expect("lock")
            .push(Request { path, head, body });
        let answer = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let mut stream = reader.into_inner();
        let _ = stream.write_all(answer.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

/// A data directory with APNs configured, the hub serving it and its fake bridge.
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
        // The test key, alone in a private file, as `hub.json` names it.
        let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/apns-test.p8");
        let text = std::fs::read_to_string(data).expect("the test key");
        let pem = &text[text.find("-----BEGIN").expect("a PEM block")..];
        let key = root.join("AuthKey_TEST.p8");
        std::fs::write(&key, pem).expect("key");
        std::fs::set_permissions(&key, PermissionsExt::from_mode(0o600)).expect("chmod");
        let config = json!({"apns": {"team_id": "TEAM123456", "key_id": "KEY1234567",
            "key_path": key, "sandbox": false}});
        std::fs::write(root.join("hub.json"), config.to_string()).expect("hub.json");
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

    /// Pairs a phone called `name`; its device token.
    async fn pair(&self, name: &str) -> String {
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
        paired["token"].as_str().expect("a token").to_owned()
    }

    /// A phone that said hello with `device`.
    async fn hello(&self, device: &str) -> Phone {
        let mut phone = self.connect().await;
        send(
            &mut phone,
            json!({"v": 1, "req": 2, "type": "hello", "token": device}),
        )
        .await;
        recv_kind(&mut phone, "welcome").await;
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

/// Registers `token` and `start_token` for the phone's Live Activity.
async fn register(phone: &mut Phone, token: Option<&str>, start_token: Option<&str>) {
    let mut message = json!({"v": 1, "req": 3, "type": "register_activity"});
    if let Some(token) = token {
        message["token"] = json!(token);
    }
    if let Some(start_token) = start_token {
        message["start_token"] = json!(start_token);
    }
    send(phone, message).await;
    assert_eq!(recv_kind(phone, "ack").await, ack(3, "Activity registered"));
}

/// Streams `session`, as the app does in the foreground.
async fn subscribe(phone: &mut Phone, session: &str) {
    send(
        phone,
        json!({"v": 1, "req": 4, "type": "subscribe", "session": session}),
    )
    .await;
    recv_kind(phone, "subscribed").await;
}

/// Closes the phone's socket, as the app does in the background, and gives the hub a moment.
async fn background(mut phone: Phone) {
    let _ = timeout(WAIT, phone.close(None)).await;
    drop(phone);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// The content state an activity of `kind` shows for the session `session` with `approvals`.
fn assert_state(request: &Request, event: &str, kind: &str, session: &str, approvals: u64) {
    let aps = &request.body["aps"];
    assert_eq!(aps["event"], event, "{request:?}");
    let state = &aps["content-state"];
    assert_eq!(state["kind"], kind, "{request:?}");
    assert_eq!(state["session"], session, "{request:?}");
    assert_eq!(state["mac"], MAC, "{request:?}");
    assert_eq!(state["approvals"], approvals, "{request:?}");
    assert!(state["since"].as_u64().is_some(), "{request:?}");
    // Only a start carries the attributes.
    if event == "start" {
        assert_eq!(aps["attributes-type"], "OctetActivity", "{request:?}");
        assert_eq!(aps["attributes"], json!({}), "{request:?}");
    } else {
        let aps = aps.as_object().expect("aps");
        assert!(!aps.contains_key("attributes-type"), "{request:?}");
        assert!(!aps.contains_key("attributes"), "{request:?}");
    }
    let priority = if event == "start" || kind == "approval" {
        "apns-priority: 10"
    } else {
        "apns-priority: 5"
    };
    for header in [
        "apns-topic: dev.octet.mobile.push-type.liveactivity",
        "apns-push-type: liveactivity",
        priority,
    ] {
        assert!(
            request.head.iter().any(|line| line == header),
            "{header}: {request:?}"
        );
    }
    let bearer = request
        .head
        .iter()
        .find_map(|line| line.strip_prefix("authorization: bearer "))
        .expect("a JWT");
    assert_eq!(bearer.split('.').count(), 3, "{request:?}");
}

#[test]
fn the_live_activity_follows_the_sessions_while_the_phone_is_away() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the fake's port");
    listener.set_nonblocking(true).expect("non-blocking");
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("an address").port()
    );
    // SAFETY: this binary has one test and nothing else has started threads that read the
    // environment; the runtime below is built after this call.
    unsafe {
        std::env::set_var("OCTET_HUB_APNS_URL", &url);
    }
    let root = PathBuf::from(format!("/tmp/oha{}", std::process::id()));
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
        tokio::join!(
            start_update_and_end(&fake, root.join("a")),
            a_410_drops_the_token(&fake, root.join("b")),
            a_clear_and_a_register_start_afresh(&fake, root.join("c")),
        );
        // Nothing of the approval, the workspace, the model or the engine went out.
        let all = format!("{:?}", fake.requests.lock().expect("lock"));
        for secret in ["/work", "rm -rf", "secret-dir", "private", "opus", "claude"] {
            assert!(!all.contains(secret), "{secret} in {all}");
        }
    });
    drop(runtime);
    let _ = std::fs::remove_dir_all(&root);
}

/// An approval while the app streams a session is the app's to show. Once it is in the
/// background, a second approval starts the activity. A background wake that only registers the
/// new activity's token ends nothing, and the next change goes to that token. The app streaming a
/// session again ends the activity, and the hub then leaves it alone.
async fn start_update_and_end(fake: &Fake, root: PathBuf) {
    const TOKEN: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
    const START: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    const WOKEN: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
    let env = Env::start(root, 6201).await;
    let mut link = env.link().await;
    let device = env.pair("Story A phone").await;
    let mut phone = env.hello(&device).await;
    register(&mut phone, Some(TOKEN), Some(START)).await;
    subscribe(&mut phone, &env.id).await;
    // While the app streams a session, an approval is the app's to show.
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(fake.to(START).is_empty() && fake.to(TOKEN).is_empty());
    background(phone).await;
    link.state("idle", &[1, 2]).await;
    let start = fake.wait_for(START, 0).await;
    assert_state(&start, "start", "approval", &env.id, 2);
    assert_eq!(
        start.body["aps"]["alert"],
        json!({"title": "Octet", "body": "Approval waiting"})
    );
    // The app wakes in the background to hand over the new activity's token: a hello and a
    // registration, no stream. That ends nothing.
    let mut wake = env.hello(&device).await;
    register(&mut wake, Some(WOKEN), Some(START)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(fake.to(TOKEN).is_empty() && fake.to(WOKEN).is_empty());
    // Answered, and a turn runs: an update, to the token the wake handed over, while the wake's
    // connection is still open.
    link.state("running", &[]).await;
    let update = fake.wait_for(WOKEN, 0).await;
    assert_state(&update, "update", "running", &env.id, 0);
    background(wake).await;
    // The app is back and streams a session: the activity ends, and the app drives it from here.
    let mut phone = env.hello(&device).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(fake.to(WOKEN).len(), 1, "a bare hello ends nothing");
    subscribe(&mut phone, &env.id).await;
    let end = fake.wait_for(WOKEN, 1).await;
    assert_state(&end, "end", "running", &env.id, 0);
    assert!(end.body["aps"]["dismissal-date"].as_u64().is_some());
    link.state("idle", &[3]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        (
            fake.to(START).len(),
            fake.to(TOKEN).len(),
            fake.to(WOKEN).len()
        ),
        (1, 0, 2)
    );
    drop(phone);
}

/// APNs says the start token is gone: it is cleared from `devices.json` and never tried again.
async fn a_410_drops_the_token(fake: &Fake, root: PathBuf) {
    const START: &str = "dead5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    let env = Env::start(root, 6202).await;
    let mut link = env.link().await;
    let device = env.pair("Story B phone").await;
    let mut phone = env.hello(&device).await;
    register(&mut phone, None, Some(START)).await;
    assert!(env.devices().contains(START));
    background(phone).await;
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    let start = fake.wait_for(START, 0).await;
    assert_state(&start, "start", "approval", &env.id, 1);
    timeout(SEND_WITHIN, async {
        while env.devices().contains("activity") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the dead token is dropped");
    assert!(!env.devices().contains(START));
    // The next approval has nowhere to go.
    link.state("idle", &[1, 2]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(fake.to(START).len(), 1);
}

/// The phone turns the Live Activity off and on again in one wake, faster than the hub looks:
/// the next approval starts a new activity, rather than updating the one the clear forgot.
async fn a_clear_and_a_register_start_afresh(fake: &Fake, root: PathBuf) {
    const START: &str = "e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4";
    let env = Env::start(root, 6203).await;
    let mut link = env.link().await;
    let device = env.pair("Story C phone").await;
    let mut phone = env.hello(&device).await;
    register(&mut phone, None, Some(START)).await;
    background(phone).await;
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    let first = fake.wait_for(START, 0).await;
    assert_state(&first, "start", "approval", &env.id, 1);
    // Off and on again, in one connection that streams nothing.
    let mut wake = env.hello(&device).await;
    send(
        &mut wake,
        json!({"v": 1, "req": 5, "type": "clear_activity"}),
    )
    .await;
    assert_eq!(
        recv_kind(&mut wake, "ack").await,
        ack(5, "Activity cleared")
    );
    register(&mut wake, None, Some(START)).await;
    background(wake).await;
    link.state("idle", &[1, 2]).await;
    let second = fake.wait_for(START, 1).await;
    assert_state(&second, "start", "approval", &env.id, 2);
}
