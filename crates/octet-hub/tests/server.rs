//! The hub's WebSocket API end to end: a fake bridge (a Unix socket and a written registration),
//! real journal files, and phones connecting over loopback.
use futures_util::{SinkExt, StreamExt};
use octet_core::remote::{
    protocol::{
        Code, ack, bridge_catalog_frame, bridge_state_frame, parse_bridge_command, refused,
    },
    run::Registration,
};
use octet_hub::{Hub, Store};
use serde_json::{Value, json};
use std::{
    future::Future,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpStream, UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message, client::IntoClientRequest},
};

/// The tailnet login the hub is told is its owner.
const OWNER: &str = "owner@example";
/// How long any one step may take before the test fails.
const WAIT: Duration = Duration::from_secs(5);
/// The journal every fake session writes.
const JOURNAL: &str = "session-1-1.jsonl";

type Phone = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The address the fake `tailscale serve` shows every hub at.
const SERVED_URL: &str = "https://mac.tail1234.ts.net";
/// Ports whose hub the fake `tailscale serve` does not publish.
static UNPUBLISHED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
/// The port of every question the fake `tailscale serve` was asked.
static ASKED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
/// How often a test hub asks the fake `tailscale serve` again.
const RECHECK: Duration = Duration::from_secs(10);

/// A stand-in for asking Tailscale: every hub is published, except those in [`UNPUBLISHED`].
fn fake_publisher(port: u16) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> {
    ASKED.lock().expect("lock").push(port);
    let hidden = UNPUBLISHED.lock().expect("lock").contains(&port);
    Box::pin(async move { (!hidden).then(|| SERVED_URL.to_owned()) })
}

/// A loopback port nothing listens on now.
async fn free_port() -> u16 {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    probe.local_addr().expect("an address").port()
}

/// A data directory under `/tmp` (socket paths are limited to about 104 bytes), and the hub
/// serving it once started.
struct Env {
    root: PathBuf,
    run: PathBuf,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
}

impl Env {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/ohs{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let run = root.join("run");
        std::fs::create_dir_all(&run).expect("run directory");
        Self {
            root,
            run,
            port: 0,
            stop: None,
        }
    }

    /// Starts the hub on a free port and waits until its control socket answers.
    async fn start(&mut self) {
        self.start_with(0, RECHECK).await;
    }

    /// Starts the hub on `port` (0 for any), asking Tailscale again every `recheck`, and waits
    /// until its control socket answers.
    async fn start_with(&mut self, port: u16, recheck: Duration) {
        let (stop, stopped) = oneshot::channel::<()>();
        let directory = self.root.clone();
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            Hub::serve_with(
                directory,
                port,
                OWNER.to_owned(),
                shutdown,
                oneshot::channel().0,
                (fake_publisher, recheck),
            )
            .await
        });
        self.stop = Some(stop);
        let status = timeout(WAIT, async {
            loop {
                if let Some(status) = self.try_control(&json!({"type": "status"})).await {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
        self.port = u16::try_from(status["port"].as_u64().expect("a port")).expect("a port");
    }

    async fn try_control(&self, request: &Value) -> Option<Value> {
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

    /// One request on the control socket, and its answer.
    async fn control(&self, request: Value) -> Value {
        timeout(WAIT, self.try_control(&request))
            .await
            .expect("the control socket answers")
            .expect("a JSON answer")
    }

    /// Appends records `seqs` to the journal `name`.
    fn journal(&self, name: &str, seqs: std::ops::Range<u64>) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(name))
            .expect("journal");
        for seq in seqs {
            let record = json!({"format": "octet-preview-1", "seq": seq, "type": "text",
                "data": {"n": seq}});
            writeln!(file, "{record}").expect("journal write");
        }
    }

    /// A fake bridge for `pid`: its journal (records `seqs`), its socket and its registration.
    async fn bridge(&self, pid: u32, seqs: std::ops::Range<u64>) -> Bridge {
        self.bridge_started(pid, seqs, false).await
    }

    /// Like [`bridge`](Self::bridge), registered as started from the phone or not.
    async fn bridge_started(
        &self,
        pid: u32,
        seqs: std::ops::Range<u64>,
        from_phone: bool,
    ) -> Bridge {
        self.journal(JOURNAL, seqs);
        let registration = Registration {
            id: format!("{pid}-1790000000"),
            pid,
            started: 1_790_000_000,
            socket: self.run.join(format!("{pid}.sock")),
            engine: "claude".into(),
            workspace: "/work".into(),
            model: "opus".into(),
            mode: "ask".into(),
            vendor_session: String::new(),
            journals: vec![self.root.join(JOURNAL)],
            from_phone,
            tmux: from_phone.then(|| format!("octet-{pid}")),
        };
        let listener = UnixListener::bind(&registration.socket).expect("bridge socket");
        registration.write(&self.run).await.expect("registration");
        Bridge {
            id: registration.id,
            listener,
        }
    }

    /// Pairs a phone called `name` and returns its token.
    async fn pair(&self, name: &str) -> String {
        let mut phone = connect(self.port, Some(OWNER)).await.expect("connects");
        self.pair_on(&mut phone, name).await
    }

    /// Pairs a phone called `name` over `phone` and returns its token.
    async fn pair_on(&self, phone: &mut Phone, name: &str) -> String {
        let secret = self.control(json!({"type": "pair_secret"})).await;
        let secret = secret["secret"].as_str().expect("a secret").to_owned();
        send(
            phone,
            json!({"v": 1, "req": 1, "type": "pair", "secret": secret, "name": name,
                "platform": "ios"}),
        )
        .await;
        let paired = recv(phone).await;
        assert_eq!(paired["type"], "paired", "{paired}");
        assert_eq!(paired["device"], name);
        paired["token"].as_str().expect("a token").to_owned()
    }

    /// A paired phone that said hello, and the `sessions` frame it got.
    async fn phone(&self, name: &str) -> (Phone, Value) {
        let token = self.pair(name).await;
        hello(self.port, &token).await
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

/// A fake terminal UI's bridge socket.
struct Bridge {
    id: String,
    listener: UnixListener,
}

impl Bridge {
    /// The hub's connection to this bridge.
    async fn accept(&self) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        let (stream, _) = timeout(WAIT, self.listener.accept())
            .await
            .expect("the hub connects to the bridge")
            .expect("accept");
        let (read, write) = stream.into_split();
        (BufReader::new(read), write)
    }
}

/// The next line the hub wrote to a bridge, parsed.
async fn bridge_line(read: &mut BufReader<OwnedReadHalf>) -> Value {
    let mut line = String::new();
    timeout(WAIT, read.read_line(&mut line))
        .await
        .expect("the bridge gets a line")
        .expect("read");
    serde_json::from_str(&line).expect("a JSON line")
}

/// Whether the hub writes nothing to the bridge for a while.
async fn bridge_quiet(read: &mut BufReader<OwnedReadHalf>) -> bool {
    let mut line = String::new();
    timeout(Duration::from_millis(400), read.read_line(&mut line))
        .await
        .is_err()
}

async fn bridge_write(write: &mut OwnedWriteHalf, frame: Value) {
    write
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .expect("bridge write");
}

async fn connect(port: u16, login: Option<&str>) -> Result<Phone, tungstenite::Error> {
    let mut request = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .expect("a request");
    if let Some(login) = login {
        request
            .headers_mut()
            .insert("Tailscale-User-Login", login.parse().expect("a header"));
    }
    timeout(WAIT, connect_async(request))
        .await
        .expect("the handshake ends")
        .map(|(phone, _)| phone)
}

/// A fresh connection that said hello with `token`, and the `sessions` frame it got.
async fn hello(port: u16, token: &str) -> (Phone, Value) {
    let mut phone = connect(port, Some(OWNER)).await.expect("connects");
    send(
        &mut phone,
        json!({"v": 1, "req": 2, "type": "hello", "token": token}),
    )
    .await;
    let welcome = recv(&mut phone).await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    let sessions = recv(&mut phone).await;
    assert_eq!(sessions["type"], "sessions", "{sessions}");
    (phone, sessions)
}

async fn send(phone: &mut Phone, message: Value) {
    timeout(WAIT, phone.send(Message::text(message.to_string())))
        .await
        .expect("send in time")
        .expect("send");
}

/// The next text frame, parsed; fails if none comes or the connection ends.
async fn recv(phone: &mut Phone) -> Value {
    timeout(WAIT, async {
        loop {
            match phone.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(text.as_str()).expect("a JSON frame");
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("expected a frame, got {other:?}"),
            }
        }
    })
    .await
    .expect("a frame arrives")
}

/// The next text frame of type `kind`, passing over others (such as `sessions`).
async fn recv_kind(phone: &mut Phone, kind: &str) -> Value {
    loop {
        let frame = recv(phone).await;
        if frame["type"] == kind {
            return frame;
        }
    }
}

/// Whether the connection ends soon; text frames before the end are passed over.
async fn closes(phone: &mut Phone) -> bool {
    timeout(WAIT, async {
        loop {
            match phone.next().await {
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => {}
                _ => return,
            }
        }
    })
    .await
    .is_ok()
}

/// Whether no text frame arrives for a while.
async fn quiet(phone: &mut Phone) -> bool {
    timeout(Duration::from_millis(400), recv(phone))
        .await
        .is_err()
}

fn subscribe(req: u64, session: &str, after: Option<u64>) -> Value {
    let mut command = json!({"v": 1, "req": req, "type": "subscribe", "session": session});
    if let Some(seq) = after {
        command["after"] = json!({"journal": JOURNAL, "seq": seq});
    }
    command
}

#[tokio::test]
async fn pair_then_hello_lists_sessions() {
    let mut env = Env::new();
    let bridge = env.bridge(5001, 0..1).await;
    env.start().await;
    let status = env.control(json!({"type": "status"})).await;
    assert_eq!(status["devices"], 0, "{status}");
    assert_eq!(status["published"], true, "{status}");
    assert_eq!(status["url"], SERVED_URL, "{status}");
    let secret = env.control(json!({"type": "pair_secret"})).await;
    let text = secret["secret"].as_str().expect("a secret");
    assert_eq!(text.len(), 64);
    assert_eq!(secret["url"], SERVED_URL, "{secret}");
    assert!(env.control(json!({"type": "nope"})).await["error"].is_string());

    let token = env.pair("Test phone").await;
    assert_eq!(token.len(), 64);
    assert_eq!(env.control(json!({"type": "status"})).await["devices"], 1);
    let (_phone, sessions) = hello(env.port, &token).await;
    let rows = sessions["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{sessions}");
    assert_eq!(rows[0]["id"], bridge.id);
    assert_eq!(rows[0]["engine"], "claude");
}

#[tokio::test]
async fn the_hub_asks_tailscale_at_start_and_then_every_so_often_not_per_request() {
    let mut env = Env::new();
    env.start().await;
    for _ in 0..5 {
        assert_eq!(
            env.control(json!({"type": "status"})).await["published"],
            true
        );
        assert!(env.control(json!({"type": "pair_secret"})).await["secret"].is_string());
    }
    let asked = ASKED
        .lock()
        .expect("lock")
        .iter()
        .filter(|p| **p == env.port)
        .count();
    assert_eq!(asked, 1, "asked once, at start");
}

#[tokio::test]
async fn an_unpublished_hub_mints_no_pairing_secret() {
    let mut env = Env::new();
    let port = free_port().await;
    UNPUBLISHED.lock().expect("lock").push(port);
    env.start_with(port, Duration::from_millis(100)).await;
    let status = env.control(json!({"type": "status"})).await;
    assert_eq!(status["published"], false, "{status}");
    assert!(status["url"].is_null(), "{status}");
    assert_eq!(status["port"], env.port);
    let refusal = env.control(json!({"type": "pair_secret"})).await;
    assert!(refusal["secret"].is_null(), "{refusal}");
    assert!(refusal["url"].is_null(), "{refusal}");
    assert_eq!(
        refusal["reason"],
        format!("Publish it first: tailscale serve --bg {}", env.port)
    );
    // Once published, the hub sees it at its next look, and the secret pairs a phone.
    UNPUBLISHED
        .lock()
        .expect("lock")
        .retain(|port| *port != env.port);
    let status = timeout(WAIT, async {
        loop {
            let status = env.control(json!({"type": "status"})).await;
            if status["published"] == true {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the hub looks again");
    assert_eq!(status["url"], SERVED_URL, "{status}");
    let token = env.pair("Test phone").await;
    assert_eq!(token.len(), 64);
}

#[tokio::test]
async fn unauthenticated_and_flooding_clients_are_refused() {
    let mut env = Env::new();
    let bridge = env.bridge(5002, 0..1).await;
    env.start().await;
    let (mut read, _write) = bridge.accept().await;
    let session = bridge.id.clone();
    let prompt = |req: u64, session: &str| {
        let text = "hi";
        json!({"v": 1, "req": req, "type": "prompt", "session": session, "text": text})
    };

    // A command before hello: refused with its own req, then the socket closes.
    let mut stranger = connect(env.port, Some(OWNER)).await.expect("connects");
    send(&mut stranger, prompt(41, &session)).await;
    let refusal = recv(&mut stranger).await;
    assert_eq!(refusal["type"], "refused", "{refusal}");
    assert_eq!(refusal["req"], 41);
    assert_eq!(refusal["code"], "unpaired");
    assert!(closes(&mut stranger).await);
    // A bad token: refused and closed too.
    let mut liar = connect(env.port, Some(OWNER)).await.expect("connects");
    send(
        &mut liar,
        json!({"v": 1, "req": 3, "type": "hello", "token": "0".repeat(64)}),
    )
    .await;
    let refusal = recv(&mut liar).await;
    assert_eq!(
        (refusal["type"].as_str(), refusal["code"].as_str()),
        (Some("refused"), Some("unpaired"))
    );
    assert!(closes(&mut liar).await);

    // Another tailnet login, or none: turned away during the handshake.
    for login in [Some("someone@example"), Some("owner@example.evil"), None] {
        match connect(env.port, login).await {
            Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 403),
            other => panic!("{login:?} got in: {other:?}"),
        }
    }

    // A flood: the first 60 commands are taken, the rest refused until the bucket refills.
    let (mut flood, _) = env.phone("Flood phone").await;
    for req in 100..200 {
        send(&mut flood, prompt(req, "nobody")).await;
    }
    let mut reasons = Vec::new();
    for _ in 100..200 {
        let frame = recv_kind(&mut flood, "refused").await;
        let code = match frame["reason"].as_str() {
            Some("No such session") => "unavailable",
            _ => "rate_limited",
        };
        assert_eq!(frame["code"], code, "{frame}");
        reasons.push(frame["reason"].as_str().expect("a reason").to_owned());
    }
    assert!(
        reasons[..60].iter().all(|r| r == "No such session"),
        "{reasons:?}"
    );
    let limited = reasons
        .iter()
        .filter(|r| *r == "Too many commands; wait a minute")
        .count();
    assert!(limited >= 39, "{reasons:?}");

    // Frames that are not text spend the bucket too.
    let (mut binary, _) = env.phone("Binary phone").await;
    for _ in 0..60 {
        binary
            .send(Message::binary(vec![1, 2, 3]))
            .await
            .expect("send");
    }
    for _ in 0..60 {
        let frame = recv_kind(&mut binary, "refused").await;
        assert_eq!(frame["reason"], "Send each message as a text frame");
        assert_eq!(frame["code"], "invalid");
    }
    send(&mut binary, prompt(7, "nobody")).await;
    let frame = recv_kind(&mut binary, "refused").await;
    assert_eq!(frame["reason"], "Too many commands; wait a minute");

    // A frame over the limit closes the connection.
    let (mut big, _) = env.phone("Big phone").await;
    let text = "x".repeat(200 * 1024);
    let _ = big
        .send(Message::text(
            prompt(5, &session).to_string().replace("hi", &text),
        ))
        .await;
    assert!(closes(&mut big).await);

    // A revoked device's next frame closes it, and its idle connections close soon after.
    let token = env.pair("Lost phone").await;
    let (mut revoked, _) = hello(env.port, &token).await;
    let (mut idle, _) = hello(env.port, &token).await;
    let mut cli = Store::open(&env.root).await.expect("the store");
    assert!(cli.revoke("Lost phone").await);
    send(&mut revoked, prompt(6, &session)).await;
    assert!(closes(&mut revoked).await);
    assert!(closes(&mut idle).await);

    assert!(bridge_quiet(&mut read).await, "nothing reaches the bridge");
}

/// The refusal a revoked connection gets as its next text frame, and whether it then closes.
async fn revoked_then_closed(phone: &mut Phone) -> bool {
    let frame = recv(phone).await;
    assert_eq!(frame["v"], 1, "{frame}");
    assert_eq!(frame["type"], "refused", "{frame}");
    assert_eq!(frame["req"], 0, "{frame}");
    assert_eq!(frame["code"], "revoked", "{frame}");
    assert_eq!(
        frame["reason"], "This phone was unpaired on the Mac",
        "{frame}"
    );
    closes(phone).await
}

#[tokio::test]
async fn a_revoked_phone_is_told_so_before_it_is_closed() {
    let mut env = Env::new();
    env.start().await;
    let token = env.pair("Lost phone").await;
    // One phone is found by the hub's reload; the other by its next frame.
    let (mut idle, _) = hello(env.port, &token).await;
    let (mut chatty, _) = hello(env.port, &token).await;
    let mut cli = Store::open(&env.root).await.expect("the store");
    assert!(cli.revoke("Lost phone").await);
    send(
        &mut chatty,
        json!({"v": 1, "req": 9, "type": "subscribe", "session": "nobody"}),
    )
    .await;
    assert!(revoked_then_closed(&mut chatty).await);
    assert!(revoked_then_closed(&mut idle).await);
}

#[tokio::test]
async fn subscribe_replays_then_streams() {
    let mut env = Env::new();
    let bridge = env.bridge(5003, 0..5).await;
    env.start().await;
    let token = env.pair("Test phone").await;
    let (mut phone, _) = hello(env.port, &token).await;
    send(&mut phone, subscribe(3, &bridge.id, Some(2))).await;
    // The replay says where it starts, then replays.
    assert_eq!(
        recv(&mut phone).await,
        json!({"v": 1, "type": "subscribed", "session": bridge.id, "from": "cursor",
            "journals": [JOURNAL]})
    );
    for seq in [3, 4] {
        let event = recv(&mut phone).await;
        assert_eq!(event["type"], "event", "{event}");
        assert_eq!(event["session"], bridge.id);
        assert_eq!(event["journal"], JOURNAL);
        assert_eq!(event["seq"], seq);
        assert_eq!(event["event"], json!({"type": "text", "data": {"n": seq}}));
    }
    let subscribed = recv(&mut phone).await;
    assert_eq!(subscribed["type"], "ack", "{subscribed}");
    assert_eq!(subscribed["req"], 3);
    // Live from here.
    env.journal(JOURNAL, 5..6);
    let live = recv(&mut phone).await;
    assert_eq!(
        (live["type"].clone(), live["seq"].clone()),
        (json!("event"), json!(5))
    );

    // A phone that comes back with its cursor gets nothing it has, then what is new.
    drop(phone);
    let (mut again, _) = hello(env.port, &token).await;
    send(&mut again, subscribe(4, &bridge.id, Some(5))).await;
    assert_eq!(recv(&mut again).await["from"], "cursor");
    assert_eq!(recv(&mut again).await, ack(4, "Subscribed"));
    assert!(quiet(&mut again).await);
    env.journal(JOURNAL, 6..7);
    let next = recv(&mut again).await;
    assert_eq!(
        (next["type"].clone(), next["seq"].clone()),
        (json!("event"), json!(6))
    );

    // Without a cursor: every record from the start, then live.
    let (mut fresh, _) = hello(env.port, &token).await;
    send(&mut fresh, subscribe(5, &bridge.id, None)).await;
    assert_eq!(recv(&mut fresh).await["from"], "start");
    for seq in 0..7 {
        assert_eq!(recv(&mut fresh).await["seq"], seq);
    }
    assert_eq!(recv(&mut fresh).await, ack(5, "Subscribed"));
    send(&mut fresh, subscribe(6, "nobody", None)).await;
    assert_eq!(
        recv(&mut fresh).await,
        refused(6, Code::Unavailable, "No such session")
    );

    // A record the replay reads before the tail does (it polls every 100 ms) arrives once.
    let (mut racer, _) = hello(env.port, &token).await;
    env.journal(JOURNAL, 7..8);
    send(&mut racer, subscribe(7, &bridge.id, Some(6))).await;
    assert_eq!(recv(&mut racer).await["type"], "subscribed");
    assert_eq!(recv(&mut racer).await["seq"], 7);
    assert_eq!(recv(&mut racer).await, ack(7, "Subscribed"));
    assert!(quiet(&mut racer).await, "the tail's copy of 7 is a repeat");
    env.journal(JOURNAL, 8..9);
    assert_eq!(recv(&mut racer).await["seq"], 8);
}

#[tokio::test]
async fn a_subscription_without_a_known_cursor_replays_every_journal_from_the_start() {
    let mut env = Env::new();
    let older = "session-1-1.jsonl";
    let newer = "session-2-1.jsonl";
    env.journal(older, 0..3);
    env.journal(newer, 0..2);
    let registration = Registration {
        id: "5050-1790000000".into(),
        pid: 5050,
        started: 1_790_000_000,
        socket: env.run.join("5050.sock"),
        engine: "codex".into(),
        workspace: "/work".into(),
        model: String::new(),
        mode: "ask".into(),
        vendor_session: String::new(),
        journals: vec![env.root.join(older), env.root.join(newer)],
        from_phone: false,
        tmux: None,
    };
    let _listener = UnixListener::bind(&registration.socket).expect("bridge socket");
    registration.write(&env.run).await.expect("registration");
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let everything = [(older, 0), (older, 1), (older, 2), (newer, 0), (newer, 1)];
    let cursor = |journal: &str, seq: u64| json!({"journal": journal, "seq": seq});
    for (req, after, from) in [
        (2, Value::Null, "start"),
        (3, cursor("session-0-1.jsonl", 9), "start"),
        (4, cursor(older, 1), "cursor"),
    ] {
        send(
            &mut phone,
            json!({"v": 1, "req": req, "type": "subscribe", "session": registration.id,
                "after": after}),
        )
        .await;
        let subscribed = recv_kind(&mut phone, "subscribed").await;
        assert_eq!(subscribed["from"], from, "{subscribed}");
        assert_eq!(
            subscribed["journals"],
            json!([older, newer]),
            "{subscribed}"
        );
        let mut got = Vec::new();
        loop {
            let frame = recv(&mut phone).await;
            if frame == ack(req, "Subscribed") {
                break;
            }
            assert_eq!(frame["type"], "event", "{frame}");
            got.push((
                frame["journal"].as_str().expect("a journal").to_owned(),
                frame["seq"].as_u64().expect("a seq"),
            ));
        }
        let skip = if from == "cursor" { 2 } else { 0 };
        let wanted: Vec<(String, u64)> = everything[skip..]
            .iter()
            .map(|&(j, s)| (j.to_owned(), s))
            .collect();
        assert_eq!(got, wanted, "{after}");
    }
}

#[tokio::test]
async fn a_live_subscriber_gets_the_previous_journals_late_records_once() {
    let mut env = Env::new();
    let (old, new) = ("session-1-1.jsonl", "session-2-1.jsonl");
    env.journal(old, 0..2);
    let mut registration = Registration {
        id: "5060-1790000000".into(),
        pid: 5060,
        started: 1_790_000_000,
        socket: env.run.join("5060.sock"),
        engine: "codex".into(),
        workspace: "/work".into(),
        model: String::new(),
        mode: "ask".into(),
        vendor_session: String::new(),
        journals: vec![env.root.join(old)],
        from_phone: false,
        tmux: None,
    };
    let _listener = UnixListener::bind(&registration.socket).expect("bridge socket");
    registration.write(&env.run).await.expect("registration");
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(
        &mut phone,
        json!({"v": 1, "req": 2, "type": "subscribe", "session": registration.id,
            "after": {"journal": old, "seq": 1}}),
    )
    .await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(2, "Subscribed"));
    // The interface reconnects: a new journal begins, and the hub's next scan sees it.
    env.journal(new, 0..1);
    registration.journals.push(env.root.join(new));
    registration.write(&env.run).await.expect("registration");
    let first = recv_kind(&mut phone, "event").await;
    assert_eq!(
        (first["journal"].clone(), first["seq"].clone()),
        (json!(new), json!(0))
    );
    // The old session's stop record lands after the switch.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(env.root.join(old))
        .expect("journal");
    let stopped = json!({"format": "octet-preview-1", "seq": 2, "type": "stopped", "data": null});
    writeln!(file, "{stopped}").expect("journal write");
    let late = recv_kind(&mut phone, "event").await;
    assert_eq!(
        (late["journal"].clone(), late["seq"].clone()),
        (json!(old), json!(2))
    );
    assert_eq!(late["event"]["type"], "stopped");
    assert!(quiet(&mut phone).await, "it arrives once");
}

#[tokio::test]
async fn commands_reach_the_bridge_and_replies_come_back() {
    let mut env = Env::new();
    let bridge = env.bridge(5004, 0..1).await;
    env.start().await;
    let (mut read, mut write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(
        &mut phone,
        json!({"v": 1, "req": 1000, "type": "prompt", "session": bridge.id, "text": "hello there"}),
    )
    .await;
    let line = bridge_line(&mut read).await;
    assert_eq!(line["type"], "prompt");
    assert_eq!(line["by"], "Test phone");
    assert_eq!(line["text"], "hello there");
    let (hub_req, by, _) = parse_bridge_command(&line.to_string()).expect("a bridge command");
    assert_eq!(by, "Test phone");
    bridge_write(&mut write, ack(hub_req, "queued")).await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(1000, "queued"));

    // A refusal comes back to the phone's own req, and hub reqs never repeat.
    send(
        &mut phone,
        json!({"v": 1, "req": 1000, "type": "cancel", "session": bridge.id}),
    )
    .await;
    let line = bridge_line(&mut read).await;
    assert_eq!(line["type"], "cancel");
    let second = line["req"].as_u64().expect("a req");
    assert_ne!(second, hub_req);
    bridge_write(
        &mut write,
        refused(second, Code::Invalid, "No turn is running"),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(1000, Code::Invalid, "No turn is running")
    );
    // A reply to a req the hub never sent goes nowhere.
    bridge_write(&mut write, ack(999_999, "stray")).await;
    assert!(quiet(&mut phone).await);
}

#[tokio::test]
async fn prompts_and_steers_over_the_prompt_limit_are_refused() {
    let mut env = Env::new();
    let bridge = env.bridge(5009, 0..1).await;
    env.start().await;
    let (mut read, _write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let long = "x".repeat(octet_core::PROMPT_LIMIT + 1);
    for (req, kind) in [(1, "prompt"), (2, "steer")] {
        send(
            &mut phone,
            json!({"v": 1, "req": req, "type": kind, "session": bridge.id, "text": long}),
        )
        .await;
        let refusal = recv_kind(&mut phone, "refused").await;
        assert_eq!(refusal["req"], req, "{refusal}");
        assert_eq!(refusal["code"], "too_large", "{refusal}");
        assert_eq!(refusal["reason"], "The text is over 64 KiB");
    }
    assert!(bridge_quiet(&mut read).await, "the bridge receives nothing");
    // At the limit, it goes through.
    let most = "x".repeat(octet_core::PROMPT_LIMIT);
    send(
        &mut phone,
        json!({"v": 1, "req": 3, "type": "prompt", "session": bridge.id, "text": most}),
    )
    .await;
    assert_eq!(bridge_line(&mut read).await["type"], "prompt");
}

#[tokio::test]
async fn a_command_to_a_session_that_ends_is_refused_at_once() {
    let mut env = Env::new();
    let bridge = env.bridge(5011, 0..1).await;
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let prompt = |req: u64| json!({"v": 1, "req": req, "type": "prompt", "session": bridge.id, "text": "hi"});
    let ended = |req: u64| refused(req, Code::Unavailable, "The session ended");
    // The bridge's link closes with the command unanswered.
    let (mut read, write) = bridge.accept().await;
    send(&mut phone, prompt(1)).await;
    assert_eq!(bridge_line(&mut read).await["type"], "prompt");
    drop((read, write));
    assert_eq!(recv_kind(&mut phone, "refused").await, ended(1));
    // The hub links again; then the session goes away with a command unanswered.
    let (mut read, _write) = bridge.accept().await;
    send(&mut phone, prompt(2)).await;
    assert_eq!(bridge_line(&mut read).await["type"], "prompt");
    std::fs::remove_file(env.run.join("5011.json")).expect("unregister");
    assert_eq!(recv_kind(&mut phone, "refused").await, ended(2));
}

#[tokio::test]
async fn full_access_needs_the_confirm_here_too() {
    let mut env = Env::new();
    let bridge = env.bridge(5005, 0..1).await;
    env.start().await;
    let (mut read, _write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let reason = "Full access needs confirm: \"full access\"";
    send(
        &mut phone,
        json!({"v": 1, "req": 7, "type": "set_mode", "session": bridge.id, "mode": "full-access"}),
    )
    .await;
    assert_eq!(recv(&mut phone).await, refused(7, Code::NotAllowed, reason));
    send(
        &mut phone,
        json!({"v": 1, "req": 8, "type": "set_mode", "session": bridge.id, "mode": "full-access",
            "confirm": "yes"}),
    )
    .await;
    assert_eq!(recv(&mut phone).await, refused(8, Code::NotAllowed, reason));
    send(
        &mut phone,
        json!({"v": 1, "req": 9, "type": "start", "engine": "claude", "workspace": "/work",
            "mode": "full-access"}),
    )
    .await;
    assert_eq!(recv(&mut phone).await, refused(9, Code::NotAllowed, reason));
    assert!(bridge_quiet(&mut read).await, "the bridge receives nothing");
    // Confirmed, it goes through.
    send(
        &mut phone,
        json!({"v": 1, "req": 10, "type": "set_mode", "session": bridge.id, "mode": "full-access",
            "confirm": "full access"}),
    )
    .await;
    let line = bridge_line(&mut read).await;
    assert_eq!(
        (
            line["type"].clone(),
            line["mode"].clone(),
            line["confirm"].clone()
        ),
        (
            json!("set_mode"),
            json!("full-access"),
            json!("full access")
        )
    );
}

#[tokio::test]
async fn state_and_catalog_follow_subscriptions() {
    let mut env = Env::new();
    let bridge = env.bridge(5006, 0..1).await;
    env.start().await;
    let (_read, mut write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let state = |phase: &str| json!({"phase": phase, "queue": [], "queued": 0, "mode": "ask", "approvals": []});
    let catalog =
        |mode: &str| json!({"modes": ["ask", "auto"], "current_mode": mode, "models": []});
    bridge_write(&mut write, bridge_catalog_frame(&catalog("ask"))).await;
    bridge_write(&mut write, bridge_state_frame(&state("running"))).await;
    // The row follows the state, so the phone sees the session running.
    loop {
        let sessions = recv_kind(&mut phone, "sessions").await;
        if sessions["sessions"][0]["phase"] == "running" {
            break;
        }
    }
    send(&mut phone, subscribe(2, &bridge.id, Some(0))).await;
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(recv(&mut phone).await);
    }
    assert!(
        got.contains(&json!({"v": 1, "type": "state", "session": bridge.id,
        "state": state("running")})),
        "{got:?}"
    );
    assert!(
        got.contains(&json!({"v": 1, "type": "catalog", "session": bridge.id,
        "catalog": catalog("ask")})),
        "{got:?}"
    );
    assert!(got.contains(&ack(2, "Subscribed")), "{got:?}");
    // Updates follow.
    bridge_write(&mut write, bridge_state_frame(&state("idle"))).await;
    let update = recv_kind(&mut phone, "state").await;
    assert_eq!(update["state"], state("idle"));
    bridge_write(&mut write, bridge_catalog_frame(&catalog("auto"))).await;
    let update = recv_kind(&mut phone, "catalog").await;
    assert_eq!(update["catalog"], catalog("auto"));
    // Unsubscribed, they stop.
    send(
        &mut phone,
        json!({"v": 1, "req": 3, "type": "unsubscribe", "session": bridge.id}),
    )
    .await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(3, "Unsubscribed"));
    bridge_write(&mut write, bridge_catalog_frame(&catalog("ask"))).await;
    env.journal(JOURNAL, 1..2);
    assert!(quiet(&mut phone).await);
}

/// The next line a watching control client gets, parsed.
async fn watch_line(read: &mut BufReader<OwnedReadHalf>) -> Value {
    let mut line = String::new();
    timeout(WAIT, read.read_line(&mut line))
        .await
        .expect("the watcher gets a line")
        .expect("read");
    serde_json::from_str(&line).expect("a JSON line")
}

/// The next watch frame of `kind`, skipping others.
async fn watch_kind(read: &mut BufReader<OwnedReadHalf>, kind: &str) -> Value {
    loop {
        let frame = watch_line(read).await;
        if frame["type"] == kind {
            return frame;
        }
    }
}

#[tokio::test]
async fn watch_streams_sessions_and_live_events() {
    let mut env = Env::new();
    let bridge = env.bridge(5010, 0..2).await;
    env.start().await;
    let (_read, mut write) = bridge.accept().await;
    let stream = UnixStream::connect(env.run.join("hub.sock"))
        .await
        .expect("the control socket");
    let (read, mut out) = stream.into_split();
    let mut read = BufReader::new(read);
    out.write_all(b"{\"type\":\"watch\"}\n")
        .await
        .expect("watch");
    // The rows as they stand, with no replay of the history.
    let first = watch_line(&mut read).await;
    assert_eq!(first["type"], "sessions", "{first}");
    assert_eq!(first["sessions"][0]["id"], bridge.id);
    // A row follows the state, and carries its NOW line.
    let state = json!({"phase": "running", "queue": [], "queued": 0, "mode": "ask",
        "now": "Running Bash cargo test", "approvals": []});
    bridge_write(&mut write, bridge_state_frame(&state)).await;
    loop {
        let sessions = watch_kind(&mut read, "sessions").await;
        if sessions["sessions"][0]["now"] == "Running Bash cargo test" {
            assert_eq!(sessions["sessions"][0]["phase"], "running");
            break;
        }
    }
    // Live events for every session, no subscribe needed.
    env.journal(JOURNAL, 2..3);
    let event = watch_kind(&mut read, "event").await;
    assert_eq!(
        (event["session"].clone(), event["seq"].clone()),
        (json!(bridge.id), json!(2))
    );
    // A phone's view of the rows has the NOW line too.
    let token = env.pair("Test phone").await;
    let (_phone, sessions) = hello(env.port, &token).await;
    assert_eq!(
        sessions["sessions"][0]["now"], "Running Bash cargo test",
        "{sessions}"
    );
    // Control requests still work next to a watcher.
    assert_eq!(env.control(json!({"type": "status"})).await["devices"], 1);
}

#[tokio::test]
async fn audit_records_remote_actions() {
    let mut env = Env::new();
    let bridge = env.bridge(5007, 0..1).await;
    env.start().await;
    let (mut read, mut write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let commands = [
        json!({"v": 1, "req": 1, "type": "prompt", "session": bridge.id, "text": "secret plans"}),
        json!({"v": 1, "req": 2, "type": "answer", "session": bridge.id, "approval": 4,
            "allow": true}),
    ];
    for command in commands {
        send(&mut phone, command).await;
        let line = bridge_line(&mut read).await;
        let req = line["req"].as_u64().expect("a req");
        bridge_write(&mut write, ack(req, "done")).await;
        assert_eq!(recv_kind(&mut phone, "ack").await["message"], "done");
    }
    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let lines: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    assert_eq!(lines.len(), 2, "{audit}");
    for line in &lines {
        assert_eq!(line["device"], "Test phone");
        assert_eq!(line["session"], bridge.id);
    }
    assert_eq!(lines[0]["what"], "prompt");
    assert_eq!(lines[1]["what"], "answer 4 allow");
    assert!(!audit.contains("secret plans"));
}

const PUSH: &str = "ExpoPushToken[Zk3_secretvalue-77]";

fn register(req: u64, token: Option<&str>) -> Value {
    json!({"v": 1, "req": req, "type": "register_push", "token": token})
}

#[tokio::test]
async fn register_push_sets_and_clears_the_devices_token() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let (mut other, _) = env.phone("Other phone").await;
    let stored = |env: &Env| std::fs::read_to_string(env.root.join("devices.json")).expect("file");

    send(&mut phone, register(7, Some(PUSH))).await;
    let ack = recv_kind(&mut phone, "ack").await;
    assert_eq!(
        (ack["req"].as_u64(), ack["message"].as_str()),
        (Some(7), Some("Notifications on"))
    );
    let text = stored(&env);
    assert!(text.contains(PUSH), "{text}");
    let reopened = Store::open(&env.root).await.expect("the store");
    let tokens: Vec<_> = reopened
        .devices()
        .iter()
        .map(|d| (d.name.as_str(), d.push_token.as_deref()))
        .collect();
    assert_eq!(tokens, [("Test phone", Some(PUSH)), ("Other phone", None)]);

    // A later hello still sees a working device, and a second registration replaces the first.
    let newer = "ExponentPushToken[rotated-1]";
    send(&mut phone, register(8, Some(newer))).await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await["message"],
        "Notifications on"
    );
    let text = stored(&env);
    assert!(text.contains(newer) && !text.contains(PUSH), "{text}");

    // Turning it off removes it, and only for that device.
    send(&mut other, register(1, Some("ExpoPushToken[other-device]"))).await;
    assert_eq!(
        recv_kind(&mut other, "ack").await["message"],
        "Notifications on"
    );
    send(&mut phone, register(9, None)).await;
    let ack = recv_kind(&mut phone, "ack").await;
    assert_eq!(
        (ack["req"].as_u64(), ack["message"].as_str()),
        (Some(9), Some("Notifications off"))
    );
    let text = stored(&env);
    assert!(
        !text.contains(newer) && text.contains("other-device"),
        "{text}"
    );
    assert_eq!(
        std::fs::metadata(env.root.join("devices.json"))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn a_bad_push_token_is_refused_and_the_connection_goes_on() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    for (req, token) in [
        (1, "https://exp.host/evil"),
        (2, "ExpoPushToken[]"),
        (3, ""),
    ] {
        send(&mut phone, register(req, Some(token))).await;
        let refusal = recv_kind(&mut phone, "refused").await;
        assert_eq!(
            (refusal["req"].as_u64(), refusal["code"].as_str()),
            (Some(req), Some("invalid")),
            "{refusal}"
        );
    }
    send(&mut phone, json!({"v": 1, "req": 4, "type": "ping"})).await;
    assert_eq!(recv_kind(&mut phone, "ack").await["message"], "pong");
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(!text.contains("push_token"), "{text}");
}

#[tokio::test]
async fn push_commands_before_hello_are_refused_as_unpaired() {
    let mut env = Env::new();
    env.start().await;
    for command in [
        register(41, Some(PUSH)),
        register(42, None),
        json!({"v": 1, "req": 43, "type": "ping"}),
    ] {
        let mut stranger = connect(env.port, Some(OWNER)).await.expect("connects");
        let req = command["req"].clone();
        send(&mut stranger, command).await;
        let refusal = recv(&mut stranger).await;
        assert_eq!(refusal["type"], "refused", "{refusal}");
        assert_eq!(refusal["req"], req);
        assert_eq!(refusal["code"], "unpaired");
        assert!(closes(&mut stranger).await);
    }
}

/// How many commands a device may send at once before the hub rate limits it: the hub's
/// `COMMANDS_PER_MINUTE` in `server.rs` (the flood test above spells it 60 too).
const BURST: usize = 60;

#[tokio::test]
async fn ping_answers_pong_and_spends_the_rate_limit_like_any_command() {
    const SENT: usize = 100;
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    for req in 1..=SENT as u64 {
        send(&mut phone, json!({"v": 1, "req": req, "type": "ping"})).await;
    }
    let mut answers = Vec::new();
    for _ in 0..SENT {
        let frame = loop {
            let frame = recv(&mut phone).await;
            if matches!(frame["type"].as_str(), Some("ack" | "refused")) {
                break frame;
            }
        };
        answers.push((
            frame["type"].as_str().unwrap_or_default().to_owned(),
            frame["message"].clone(),
            frame["code"].clone(),
        ));
    }
    assert!(
        answers[..BURST]
            .iter()
            .all(|(kind, message, _)| kind == "ack" && message == "pong"),
        "{answers:?}"
    );
    let limited = answers
        .iter()
        .filter(|(kind, _, code)| kind == "refused" && code == "rate_limited")
        .count();
    assert!(limited + 1 >= SENT - BURST, "{answers:?}");

    // A registration is charged like any other command too.
    let (mut spender, _) = env.phone("Spender").await;
    for req in 1..=BURST as u64 {
        send(&mut spender, json!({"v": 1, "req": req, "type": "ping"})).await;
    }
    send(&mut spender, register(1_000, Some(PUSH))).await;
    let frame = loop {
        let frame = recv(&mut spender).await;
        if frame["req"] == 1_000 {
            break frame;
        }
    };
    assert_eq!(
        (frame["type"].as_str(), frame["code"].as_str()),
        (Some("refused"), Some("rate_limited"))
    );
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(!text.contains(PUSH), "{text}");
}

#[tokio::test]
async fn the_audit_log_says_only_push_on_or_push_off() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(&mut phone, register(1, Some(PUSH))).await;
    recv_kind(&mut phone, "ack").await;
    send(&mut phone, register(2, None)).await;
    recv_kind(&mut phone, "ack").await;
    send(&mut phone, json!({"v": 1, "req": 3, "type": "ping"})).await;
    recv_kind(&mut phone, "ack").await;
    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let lines: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    let what: Vec<_> = lines
        .iter()
        .map(|l| l["what"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(what, ["push on", "push off"], "{audit}");
    assert!(
        lines
            .iter()
            .all(|l| l["device"] == "Test phone" && l["session"] == ""),
        "{audit}"
    );
    assert!(
        !audit.contains("secretvalue") && !audit.contains("ExpoPushToken"),
        "{audit}"
    );
}

#[tokio::test]
async fn a_long_replay_hands_over_to_live_without_gaps_or_repeats() {
    let mut env = Env::new();
    let bridge = env.bridge(5008, 0..1200).await;
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(&mut phone, subscribe(2, &bridge.id, Some(0))).await;
    // Records keep landing while the replay pages through the journal.
    for start in (1200..1300).step_by(10) {
        env.journal(JOURNAL, start..start + 10);
        tokio::task::yield_now().await;
    }
    let mut seqs = Vec::new();
    let mut acked = false;
    while seqs.last() != Some(&1299) {
        let frame = recv(&mut phone).await;
        match frame["type"].as_str() {
            Some("event") => seqs.push(frame["seq"].as_u64().expect("a seq")),
            Some("ack") => acked = true,
            _ => {}
        }
    }
    assert!(seqs.iter().copied().eq(1..1300), "a gap or a repeat");
    if !acked {
        assert_eq!(recv_kind(&mut phone, "ack").await, ack(2, "Subscribed"));
    }
    assert!(quiet(&mut phone).await);
}

#[tokio::test]
async fn connections_that_never_say_hello_are_capped() {
    let mut env = Env::new();
    env.start().await;
    let mut waiting = Vec::new();
    for _ in 0..8 {
        waiting.push(connect(env.port, Some(OWNER)).await.expect("connects"));
    }
    assert!(
        connect(env.port, Some(OWNER)).await.is_err(),
        "a ninth silent connection is turned away"
    );
    // One that pairs and says hello leaves the waiting room, and another can come in.
    let token = env.pair_on(&mut waiting[0], "Test phone").await;
    send(
        &mut waiting[0],
        json!({"v": 1, "req": 2, "type": "hello", "token": token}),
    )
    .await;
    assert_eq!(recv(&mut waiting[0]).await["type"], "welcome");
    assert!(connect(env.port, Some(OWNER)).await.is_ok());
}

/// A journal in `env`'s data directory for a Claude session `session` in `cwd`.
fn recorded(env: &Env, stamp: u64, cwd: &std::path::Path, session: &str, prompt: &str) {
    let header = json!({"engine": "claude", "cwd": cwd, "resume": null, "model": "opus",
        "mode": "ask"});
    let records = [
        ("session", header),
        ("ready", json!(session)),
        ("user", json!(prompt)),
    ];
    octet_testkit::write_journal(&env.root, stamp, &records, "");
}

#[tokio::test]
async fn recent_lists_the_journals_sessions() {
    let mut env = Env::new();
    let workspace = env.root.join("w");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("workspace");
    let elsewhere = std::fs::canonicalize(elsewhere).expect("canonical workspace");
    recorded(&env, 1_000_000_000, &workspace, "older", "first thing");
    recorded(
        &env,
        2_000_000_000,
        &workspace,
        "newer",
        &"long ".repeat(40),
    );
    recorded(&env, 3_000_000_000, &elsewhere, "other", "somewhere else");
    env.start().await;
    let (mut phone, _) = env.phone("Pixel").await;
    let shown = workspace.to_string_lossy();
    send(
        &mut phone,
        json!({"v": 1, "req": 5, "type": "recent", "workspace": shown}),
    )
    .await;
    let recent = recv_kind(&mut phone, "recent").await;
    assert_eq!(recent["req"], 5, "{recent}");
    let items = recent["items"].as_array().expect("items");
    let sessions: Vec<&str> = items.iter().filter_map(|i| i["session"].as_str()).collect();
    assert_eq!(sessions, ["newer", "older"], "{recent}");
    assert_eq!(items[0]["engine"], "claude");
    assert_eq!(items[0]["workspace"], json!(shown));
    assert_eq!(items[0]["model"], "opus");
    assert_eq!(
        items[0]["title"].as_str().expect("a title").chars().count(),
        60
    );
    assert_eq!(items[1]["title"], "first thing");
    assert_eq!(items[1]["started"], 1);

    // Without a workspace every known one is listed, newest first.
    send(&mut phone, json!({"v": 1, "req": 6, "type": "recent"})).await;
    let all = recv_kind(&mut phone, "recent").await;
    assert_eq!(all["req"], 6, "{all}");
    let sessions: Vec<&str> = all["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["session"].as_str())
        .collect();
    assert_eq!(sessions, ["other", "newer", "older"], "{all}");

    // A workspace that is not allowed is refused.
    send(
        &mut phone,
        json!({"v": 1, "req": 7, "type": "recent", "workspace": "/"}),
    )
    .await;
    let refusal = recv_kind(&mut phone, "refused").await;
    assert!(
        refusal["reason"]
            .as_str()
            .is_some_and(|m| m.starts_with("Workspace not allowed")),
        "{refusal}"
    );
    assert_eq!(refusal["code"], "not_allowed");
}

#[tokio::test]
async fn end_reaches_the_bridge_and_start_is_refused_when_not_allowed() {
    let mut env = Env::new();
    let bridge = env.bridge(5020, 0..1).await;
    env.start().await;
    let (mut read, mut write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(
        &mut phone,
        json!({"v": 1, "req": 3, "type": "end", "session": bridge.id}),
    )
    .await;
    let line = bridge_line(&mut read).await;
    assert_eq!(line["type"], "end");
    assert_eq!(line["by"], "Test phone");
    let (hub_req, _, _) = parse_bridge_command(&line.to_string()).expect("a bridge command");
    bridge_write(
        &mut write,
        refused(hub_req, Code::TurnOpen, "A turn is running"),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(3, Code::TurnOpen, "A turn is running")
    );
    // A bridge's refusal without a known code still reaches the phone, as invalid.
    send(
        &mut phone,
        json!({"v": 1, "req": 30, "type": "end", "session": bridge.id}),
    )
    .await;
    let line = bridge_line(&mut read).await;
    let hub_req = line["req"].as_u64().expect("a req");
    let bare = json!({"v": 1, "type": "refused", "req": hub_req, "reason": "No", "code": "?"});
    bridge_write(&mut write, bare).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(30, Code::Invalid, "No")
    );

    let start = |req: u64, engine: &str, workspace: &str| {
        json!({"v": 1, "req": req, "type": "start", "engine": engine,
            "workspace": workspace})
    };
    send(&mut phone, start(4, "claude", "/")).await;
    let reason = recv_kind(&mut phone, "refused").await;
    assert!(
        reason["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("Workspace not allowed: /"))
    );
    assert_eq!(reason["code"], "not_allowed");
    send(&mut phone, start(5, "claude", "/no/such/workspace")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(5, Code::Invalid, "No such workspace")
    );
}

#[tokio::test]
async fn at_most_eight_phone_started_sessions_run() {
    let mut env = Env::new();
    let workspace = env.root.join("w");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let mut bridges = Vec::new();
    for pid in 5030..5038 {
        bridges.push(env.bridge_started(pid, 0..1, true).await);
    }
    // A session started at the desk does not count.
    bridges.push(env.bridge(5040, 0..1).await);
    std::fs::write(
        env.root.join("hub.json"),
        json!({"workspaces": [workspace]}).to_string(),
    )
    .expect("hub.json");
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(
        &mut phone,
        json!({"v": 1, "req": 6, "type": "start", "engine": "demo", "workspace": workspace}),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(
            6,
            Code::Busy,
            "8 sessions started from the phone are running; end one first"
        )
    );
}
