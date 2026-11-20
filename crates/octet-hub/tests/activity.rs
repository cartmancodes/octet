//! The Live Activity end to end: a fake bridge opens approvals, and the hub sends to a local
//! stand-in for APNs with the real `curl`, through the debug-only `OCTET_HUB_APNS_URL` seam. This
//! is its own test binary with one test, so setting the variable before anything else runs cannot
//! race another test; a release build ignores it, so the test runs in debug builds only. Nothing
//! here reaches a real APNs host.
#![cfg(debug_assertions)]
use octet_core::remote::{
    protocol::{ack, bridge_state_frame},
    run::Registration,
};
use octet_hub::Hub;
use octet_testkit::relay::{Link as PairLink, Phone, PhoneKey, Relay, control, pair};
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
        TcpListener, TcpStream, UnixListener,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::timeout,
};

/// Where pushes go in this test: the same stand-in, at Expo's path.
const EXPO_PATH: &str = "/--/api/v2/push/send";
const WAIT: Duration = Duration::from_secs(5);
/// How long a send may take: the hub's 1 s look, the 2 s between updates, and `curl`.
const SEND_WITHIN: Duration = Duration::from_secs(10);
const JOURNAL: &str = "session-1-1.jsonl";

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

    /// The Expo messages posted so far to the push token `token`.
    fn pushes(&self, token: &str) -> Vec<Value> {
        let requests = self.requests.lock().expect("lock");
        requests
            .iter()
            .filter(|r| r.path == EXPO_PATH)
            .filter_map(|r| r.body.as_array().cloned())
            .flatten()
            .filter(|message| message["to"] == token)
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

/// A data directory with APNs configured, a relay, the hub serving it and its fake bridge.
struct Env {
    root: PathBuf,
    run: PathBuf,
    _relay: Relay,
    /// The hub's link, once a phone paired: its channel is the `mac` every send names.
    link: Mutex<Option<PairLink>>,
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
        let relay = Relay::start().await;
        let url = relay.url();
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            let log = Arc::new(|_: &str| {});
            Hub::serve_with(directory, url, log, shutdown, oneshot::channel().0).await
        });
        let env = Self {
            root,
            run,
            _relay: relay,
            link: Mutex::new(None),
            bridge,
            id: registration.id,
            stop: Some(stop),
        };
        timeout(WAIT, async {
            while control(&env.run, &json!({"type": "status"}))
                .await
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
        env
    }

    /// The hub's channel id: the `mac` its sends name.
    fn channel(&self) -> String {
        let link = self.link.lock().expect("lock");
        link.as_ref().expect("a phone paired").channel.clone()
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

    /// Pairs a phone called `name`; its key.
    async fn pair(&self, name: &str) -> PhoneKey {
        let (link, key, _) = pair(&self.run, name).await;
        *self.link.lock().expect("lock") = Some(link);
        key
    }

    /// A phone that said hello as `device`.
    async fn hello(&self, device: &PhoneKey) -> Phone {
        self.hello_with(device, None).await
    }

    /// A phone that said hello as `device`, and `background` when it is given. A background
    /// connection (a token hand-over, or a Deny) is not the app in front.
    async fn hello_with(&self, device: &PhoneKey, background: Option<bool>) -> Phone {
        let link = self
            .link
            .lock()
            .expect("lock")
            .clone()
            .expect("a phone paired");
        let mut phone = Phone::connect(&link, device).await.expect("connects");
        let mut hello = json!({"v": 1, "req": 2, "type": "hello"});
        if let Some(background) = background {
            hello["background"] = json!(background);
        }
        send(&mut phone, hello).await;
        recv_kind(&mut phone, "welcome").await;
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
    phone.send(&message).await;
}

async fn recv_kind(phone: &mut Phone, kind: &str) -> Value {
    phone.recv_kind(kind).await
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
    phone.close().await;
    drop(phone);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// The content state an activity of `kind` shows for the session `session` of the hub whose
/// channel is `mac`, with `approvals`.
fn assert_state(
    request: &Request,
    event: &str,
    kind: &str,
    (session, mac): (&str, &str),
    approvals: u64,
) {
    let aps = &request.body["aps"];
    assert_eq!(aps["event"], event, "{request:?}");
    let state = &aps["content-state"];
    assert_eq!(state["kind"], kind, "{request:?}");
    assert_eq!(state["session"], session, "{request:?}");
    assert_eq!(state["mac"], mac, "{request:?}");
    assert_eq!(state["approvals"], approvals, "{request:?}");
    assert!(state["since"].as_u64().is_some(), "{request:?}");
    // A start or an update goes stale 15 minutes on, unless another comes.
    if event == "end" {
        assert!(aps.get("stale-date").is_none(), "{request:?}");
    } else {
        let timestamp = aps["timestamp"].as_u64().expect("a timestamp");
        assert_eq!(aps["stale-date"], timestamp + 900, "{request:?}");
    }
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
    // environment; the runtime below is built after these calls.
    unsafe {
        std::env::set_var("OCTET_HUB_APNS_URL", &url);
        std::env::set_var("OCTET_HUB_PUSH_URL", format!("{url}{EXPO_PATH}"));
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
            the_apps_activity_is_updated_and_kept(&fake, root.join("a")),
            a_410_drops_the_token(&fake, root.join("b")),
            a_clear_and_a_register_start_afresh(&fake, root.join("c")),
            a_410_on_the_apps_activity_starts_one(&fake, root.join("d")),
            presence_is_any_hello_but_a_background_one(&fake, root.join("e")),
            a_push_and_an_activity_update_name_the_channel_as_mac(&fake, root.join("f")),
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

/// The app started its own activity in front and registered its token. An approval while it
/// streams a session is the app's to show. Once it is in the background, a second approval
/// updates that same activity, and nothing push-starts another. A background wake that only
/// registers a new token is told the state, and the next change goes to it. The app streaming a
/// session again ends nothing: it takes the same activity over.
async fn the_apps_activity_is_updated_and_kept(fake: &Fake, root: PathBuf) {
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
    let update = fake.wait_for(TOKEN, 0).await;
    assert_state(&update, "update", "approval", (&env.id, &env.channel()), 2);
    // The app wakes in the background with a new token: a background hello and a
    // registration. The new token is told the state as it stands; the next change goes there too.
    let mut wake = env.hello_with(&device, Some(true)).await;
    register(&mut wake, Some(WOKEN), Some(START)).await;
    let told = fake.wait_for(WOKEN, 0).await;
    assert_state(&told, "update", "approval", (&env.id, &env.channel()), 2);
    link.state("running", &[]).await;
    let running = fake.wait_for(WOKEN, 1).await;
    assert_state(&running, "update", "running", (&env.id, &env.channel()), 0);
    background(wake).await;
    // The app is back and streams a session: nothing ends, and the app drives it from here.
    let mut phone = env.hello(&device).await;
    subscribe(&mut phone, &env.id).await;
    link.state("idle", &[3]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        (
            fake.to(START).len(),
            fake.to(TOKEN).len(),
            fake.to(WOKEN).len()
        ),
        (0, 1, 2),
        "no start and no end"
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
    assert_state(&start, "start", "approval", (&env.id, &env.channel()), 1);
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
    assert_state(&first, "start", "approval", (&env.id, &env.channel()), 1);
    // Off and on again, in one connection that streams nothing.
    let mut wake = env.hello_with(&device, Some(true)).await;
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
    assert_state(&second, "start", "approval", (&env.id, &env.channel()), 2);
}

/// The app's own activity is gone: APNs answers 410 to its token, which is dropped, and the news
/// that update carried is judged again, so the next look push-starts an activity showing the
/// approval still waiting, without waiting for more news. A later approval starts no second one:
/// the started activity's token has yet to come.
async fn a_410_on_the_apps_activity_starts_one(fake: &Fake, root: PathBuf) {
    const TOKEN: &str = "deada1a1a1a1a1a1a1a1a1a1a1a1a1a1";
    const START: &str = "5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d";
    let env = Env::start(root, 6204).await;
    let mut link = env.link().await;
    let device = env.pair("Story D phone").await;
    let mut phone = env.hello(&device).await;
    register(&mut phone, Some(TOKEN), Some(START)).await;
    subscribe(&mut phone, &env.id).await;
    link.state("idle", &[]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    background(phone).await;
    link.state("idle", &[1]).await;
    let update = fake.wait_for(TOKEN, 0).await;
    assert_state(&update, "update", "approval", (&env.id, &env.channel()), 1);
    timeout(SEND_WITHIN, async {
        while env.devices().contains(TOKEN) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the dead token is dropped");
    let start = fake.wait_for(START, 0).await;
    assert_state(&start, "start", "approval", (&env.id, &env.channel()), 1);
    link.state("idle", &[1, 2]).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!((fake.to(START).len(), fake.to(TOKEN).len()), (1, 1));
}

/// The app in front is any connection whose hello is not a background one, whether it streams a
/// session or not (the Inbox streams nothing), and whether its hello says so or is an old one
/// without the field. A background connection never counts as the app in front.
async fn presence_is_any_hello_but_a_background_one(fake: &Fake, root: PathBuf) {
    const START: &str = "e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";
    let env = Env::start(root, 6205).await;
    let mut link = env.link().await;
    let device = env.pair("Story E phone").await;
    // An old hello, and no subscribe: the app is in front, so an approval sends nothing.
    let mut phone = env.hello(&device).await;
    register(&mut phone, None, Some(START)).await;
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        fake.to(START).is_empty(),
        "an old hello is the app in front"
    );
    background(phone).await;
    // A hello that says `background: false` is in front too.
    let phone = env.hello_with(&device, Some(false)).await;
    link.state("idle", &[1, 2]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        fake.to(START).is_empty(),
        "background: false is the app in front"
    );
    background(phone).await;
    // A background connection, open the whole time, is not: the next approval starts one.
    let wake = env.hello_with(&device, Some(true)).await;
    link.state("idle", &[1, 2, 3]).await;
    let start = fake.wait_for(START, 0).await;
    assert_state(&start, "start", "approval", (&env.id, &env.channel()), 3);
    drop(wake);
}

/// A phone in the background with both a push token and a Live Activity: an approval reaches
/// it both ways, and each names the Mac by the hub's channel id, the one its pairing link
/// carried.
async fn a_push_and_an_activity_update_name_the_channel_as_mac(fake: &Fake, root: PathBuf) {
    const PUSH: &str = "ExpoPushToken[story-f-token]";
    const TOKEN: &str = "f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6";
    let env = Env::start(root, 6206).await;
    let mut link = env.link().await;
    let device = env.pair("Story F phone").await;
    let mut phone = env.hello(&device).await;
    register(&mut phone, Some(TOKEN), None).await;
    send(
        &mut phone,
        json!({"v": 1, "req": 6, "type": "register_push", "token": PUSH}),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await,
        ack(6, "Notifications on")
    );
    background(phone).await;
    link.state("idle", &[]).await;
    link.state("idle", &[1]).await;
    let update = fake.wait_for(TOKEN, 0).await;
    let channel = env.channel();
    assert_eq!(channel.len(), 64);
    assert_state(&update, "update", "approval", (&env.id, &channel), 1);
    let pushed = timeout(SEND_WITHIN, async {
        loop {
            if let Some(message) = fake.pushes(PUSH).first() {
                return message.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a push arrives");
    assert_eq!(pushed["data"], json!({"session": env.id, "mac": channel}));
}
