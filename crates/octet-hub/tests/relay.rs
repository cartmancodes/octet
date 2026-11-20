//! The hub's link to the relay end to end: when it dials, how it registers and comes back, the
//! phones it answers, and the limits it keeps whatever the relay sends. A real relay runs in
//! process where one is needed; a fake one (a WebSocket server in the test) plays a relay that
//! floods, replays, or hands the channel to another hub.
//!
//! The paused-clock test sets everything up in real time and pauses only for the wait it
//! measures: paused from the start, tokio's clock can jump while a loopback write is still on
//! its way (macOS delivers loopback traffic asynchronously) and fire a timer early.
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use octet_hub::{Hub, Store};
use octet_testkit::relay::{Phone, PhoneKey, Relay, control, hex, pair, pair_link, relay_state};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

/// How long any one step may take before the test fails.
const WAIT: Duration = Duration::from_secs(10);
/// How long the hub is given to do something it must not do.
const WATCH: Duration = Duration::from_millis(700);

/// A data directory under `/tmp` (socket paths are limited), and the hub serving it once
/// started, with the relay link's log lines.
struct Env {
    root: PathBuf,
    run: PathBuf,
    log: Arc<Mutex<Vec<String>>>,
    stop: Option<oneshot::Sender<()>>,
}

impl Env {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/ohr{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let run = root.join("run");
        std::fs::create_dir_all(&run).expect("run directory");
        Self {
            root,
            run,
            log: Arc::default(),
            stop: None,
        }
    }

    /// Starts the hub, dialling `relay`, and waits until its control socket answers.
    async fn start(&mut self, relay: &str) {
        let (stop, stopped) = oneshot::channel::<()>();
        let (directory, relay, log) = (self.root.clone(), relay.to_owned(), Arc::clone(&self.log));
        let sink = Arc::new(move |line: &str| log.lock().expect("lock").push(line.to_owned()));
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            Hub::serve_with(directory, relay, sink, shutdown, oneshot::channel().0).await
        });
        self.stop = Some(stop);
        timeout(WAIT, async {
            while control(&self.run, &json!({"type": "status"}))
                .await
                .is_none()
            {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
    }

    /// Stops the hub and waits until its control socket is gone.
    async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        timeout(WAIT, async {
            while self.run.join("hub.sock").exists() {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub stops");
    }

    async fn status(&self) -> Value {
        control(&self.run, &json!({"type": "status"}))
            .await
            .expect("a status")
    }

    /// Whether the relay link logged a line containing `text`.
    fn logged(&self, text: &str) -> bool {
        self.log
            .lock()
            .expect("lock")
            .iter()
            .any(|line| line.contains(text))
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

/// A socket the fake relay accepted.
type Ws = WebSocketStream<TcpStream>;

/// A relay played by the test: every upgrade it accepts comes out of [`next`](Self::next) with
/// its path, and the test answers as it likes.
struct FakeRelay {
    url: String,
    arrived: mpsc::UnboundedReceiver<(String, Ws)>,
}

impl FakeRelay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let url = format!("ws://{}", listener.local_addr().expect("an address"));
        let (arrive, arrived) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let arrive = arrive.clone();
                tokio::spawn(async move {
                    let mut path = String::new();
                    #[expect(
                        clippy::result_large_err,
                        reason = "tungstenite's own callback type, which only says yes here"
                    )]
                    let callback = |request: &Request, response: Response| {
                        request.uri().path().clone_into(&mut path);
                        Ok(response)
                    };
                    if let Ok(ws) = accept_hdr_async(stream, callback).await {
                        let _ = arrive.send((path, ws));
                    }
                });
            }
        });
        Self { url, arrived }
    }

    /// The next upgrade, within `wait`.
    async fn next_within(&mut self, wait: Duration) -> Option<(String, Ws)> {
        timeout(wait, self.arrived.recv()).await.ok().flatten()
    }

    /// The hub's next control connection, registered ([`register`]).
    async fn register(&mut self) -> (String, Ws) {
        let (path, ws) = self.next_within(WAIT).await.expect("the hub dials");
        register(&path, ws).await
    }
}

/// Registers the hub's control connection `ws`, opened at `path`: the challenge, a signature
/// that verifies for a key whose hash is the channel in the path, and `{"registered": true}`.
/// The channel, and the connection.
async fn register(path: &str, mut ws: Ws) -> (String, Ws) {
    let channel = path
        .strip_prefix("/v1/hub/")
        .expect("a control path")
        .to_owned();
    assert_eq!(channel.len(), 64, "{path}");
    let challenge = [7_u8; 32];
    let ask = json!({"challenge": hex(&challenge)}).to_string();
    ws.send(Message::text(ask)).await.expect("the challenge");
    let answer = loop {
        match timeout(WAIT, ws.next()).await.expect("an answer") {
            Some(Ok(Message::Text(text))) => {
                break serde_json::from_str::<Value>(text.as_str()).expect("JSON");
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected an answer, got {other:?}"),
        }
    };
    let bytes = |field: &str| {
        let text = answer[field].as_str().expect("hex");
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
            .collect::<Vec<u8>>()
    };
    let key = <[u8; 32]>::try_from(bytes("key")).expect("a key");
    assert_eq!(
        hex(&Sha256::digest(key)),
        channel,
        "the key is the channel's"
    );
    let key = VerifyingKey::from_bytes(&key).expect("an Ed25519 key");
    let signature = Signature::from_slice(&bytes("signature")).expect("a signature");
    let signed = [b"octet-relay-v1".as_slice(), &challenge].concat();
    key.verify_strict(&signed, &signature)
        .expect("the signature verifies");
    let registered = json!({"registered": true}).to_string();
    ws.send(Message::text(registered))
        .await
        .expect("registered");
    (channel, ws)
}

/// Sends `{"connect": id}` on the control connection.
async fn connect(control: &mut Ws, id: &str) {
    let text = json!({"connect": id}).to_string();
    control.send(Message::text(text)).await.expect("a connect");
}

/// The `n`th connection id: 32 lowercase hex digits.
fn id(n: u64) -> String {
    format!("{n:032x}")
}

/// Every data connection the fake relay accepts within `wait` of the last, each with its path.
async fn data_connections(relay: &mut FakeRelay, wait: Duration) -> Vec<(String, Ws)> {
    let mut all = Vec::new();
    while let Some(arrived) = relay.next_within(wait).await {
        all.push(arrived);
    }
    all
}

#[tokio::test]
async fn a_known_key_says_hello_and_is_welcomed_with_the_macs_name() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, key, paired) = pair(&env.run, "Test phone").await;
    let name = paired["name"].as_str().expect("the Mac's name").to_owned();
    assert!(!name.is_empty() && name.chars().count() <= 64, "{paired}");
    assert_eq!(
        paired,
        json!({"v": 1, "type": "paired", "device": "Test phone", "name": name})
    );
    // A hello carries no token; one an old app sends is ignored.
    for hello in [
        json!({"v": 1, "req": 2, "type": "hello"}),
        json!({"v": 1, "req": 2, "type": "hello", "token": "f".repeat(64)}),
    ] {
        let mut phone = Phone::connect(&link, &key).await.expect("connects");
        phone.send(&hello).await;
        assert_eq!(
            phone.recv().await,
            json!({"v": 1, "type": "welcome", "device": "Test phone", "name": name})
        );
        assert_eq!(phone.recv().await["type"], "sessions");
    }
}

#[tokio::test]
async fn an_unknown_key_that_says_hello_is_refused_unpaired_and_closed() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, _, _) = pair(&env.run, "Test phone").await;
    let mut stranger = Phone::connect(&link, &PhoneKey::new())
        .await
        .expect("the handshake works for any key");
    stranger
        .send(&json!({"v": 1, "req": 9, "type": "hello"}))
        .await;
    let refusal = stranger.recv().await;
    assert_eq!(
        (
            refusal["type"].as_str(),
            refusal["req"].as_u64(),
            refusal["code"].as_str()
        ),
        (Some("refused"), Some(9), Some("unpaired")),
        "{refusal}"
    );
    assert!(stranger.closes().await);
}

#[tokio::test]
async fn a_revoked_key_is_dropped_within_two_seconds() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, key, _) = pair(&env.run, "Lost phone").await;
    let mut phone = Phone::connect(&link, &key).await.expect("connects");
    phone
        .send(&json!({"v": 1, "req": 2, "type": "hello"}))
        .await;
    phone.recv_kind("welcome").await;
    let mut cli = Store::open(&env.root).await.expect("the store");
    assert!(cli.revoke("Lost phone").await);
    let began = Instant::now();
    let refusal = phone.recv_kind("refused").await;
    assert_eq!(refusal["code"], "revoked", "{refusal}");
    assert!(phone.closes().await);
    // The hub re-reads the devices every 2 seconds; a little more for a busy machine.
    assert!(
        began.elapsed() < Duration::from_millis(2_500),
        "{:?}",
        began.elapsed()
    );
    // With nothing paired and no pairing open, the hub lets the relay go.
    relay_state(&env.run, "off").await;
}

#[tokio::test]
async fn nothing_is_dialled_with_no_device_and_no_pairing() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    assert!(
        fake.next_within(Duration::from_millis(1500))
            .await
            .is_none()
    );
    assert_eq!(env.status().await, json!({"relay": "off", "devices": 0}));
    // A pairing opened, it dials.
    pair_link(&env.run).await;
    let (path, _) = fake.next_within(WAIT).await.expect("the hub dials");
    assert!(path.starts_with("/v1/hub/"), "{path}");
}

#[tokio::test]
async fn status_reports_the_relay_state() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    assert_eq!(env.status().await["relay"], "off");
    pair_link(&env.run).await;
    // Dialled, but the relay has not answered.
    let (_, waiting) = fake.next_within(WAIT).await.expect("the hub dials");
    relay_state(&env.run, "connecting").await;
    drop(waiting);
    let (_, mut control) = fake.register().await;
    relay_state(&env.run, "registered").await;
    // Lost, it dials again.
    let _ = control.close(None).await;
    drop(control);
    relay_state(&env.run, "connecting").await;
    let _control = fake.register().await;
    relay_state(&env.run, "registered").await;
}

#[tokio::test]
async fn a_replaced_hub_backs_off_and_logs_another_hub_took_this_channel() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    pair_link(&env.run).await;
    // Another hub with the same key registers each time this one does: every registration is
    // closed with 4409 as soon as it is made.
    let registrations = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&registrations);
    tokio::spawn(async move {
        while let Some((path, ws)) = fake.next_within(Duration::from_secs(3600)).await {
            counted.fetch_add(1, Ordering::Relaxed);
            let (_, mut control) = register(&path, ws).await;
            let replaced = CloseFrame {
                code: CloseCode::from(4409),
                reason: "replaced".into(),
            };
            let _ = control.close(Some(replaced)).await;
        }
    });
    timeout(WAIT, async {
        while !env.logged("Another hub took this channel") {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the hub says so");
    // Ten seconds on the clock: the hub waits the longest step, and never spins.
    tokio::time::pause();
    sleep(Duration::from_secs(10)).await;
    let registrations = registrations.load(Ordering::Relaxed);
    assert!(
        (1..=3).contains(&registrations),
        "{registrations} registrations in 10 seconds"
    );
}

#[tokio::test]
async fn a_data_connection_that_fails_the_handshake_frees_its_seat() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    pair_link(&env.run).await;
    let (channel, mut control) = fake.register().await;
    // Thirty-two phones fill the room's waiting seats; the hub dials each.
    for n in 0..32 {
        connect(&mut control, &id(n)).await;
    }
    let mut held = data_connections(&mut fake, WATCH).await;
    assert_eq!(held.len(), 32);
    assert!(
        held.iter()
            .all(|(path, _)| path.starts_with(&format!("/v1/hub/{channel}/")))
    );
    // A thirty-third is ignored: no seat.
    connect(&mut control, &id(100)).await;
    assert!(fake.next_within(WATCH).await.is_none());
    // One fails its handshake (a first message of the wrong size): its seat goes back.
    let (_, mut failing) = held.pop().expect("a connection");
    failing
        .send(Message::binary(vec![0; 10]))
        .await
        .expect("sent");
    assert!(
        timeout(WAIT, async { while failing.next().await.is_some() {} })
            .await
            .is_ok()
    );
    sleep(Duration::from_millis(100)).await;
    connect(&mut control, &id(101)).await;
    let (path, _taken) = fake
        .next_within(WAIT)
        .await
        .expect("the freed seat is used");
    assert!(path.ends_with(&id(101)), "{path}");
    connect(&mut control, &id(102)).await;
    assert!(
        fake.next_within(WATCH).await.is_none(),
        "the room is full again"
    );
}

#[tokio::test]
async fn a_phone_silent_after_the_handshake_is_closed_10_seconds_after_its_connect() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, key, _) = pair(&env.run, "Quiet phone").await;
    let began = Instant::now();
    let mut phone = Phone::connect(&link, &key).await.expect("connects");
    assert!(
        !phone.closes_within(Duration::from_secs(9)).await,
        "closed before its 10 seconds"
    );
    assert!(
        phone.closes_within(Duration::from_secs(4)).await,
        "never closed"
    );
    let elapsed = began.elapsed();
    assert!(elapsed < Duration::from_millis(12_500), "{elapsed:?}");
}

#[tokio::test]
async fn a_flood_of_connects_dials_no_more_than_32_at_once_and_120_a_minute() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    pair_link(&env.run).await;
    let (_, mut control) = fake.register().await;
    let mut next = 0;
    let mut dialled = Vec::new();
    // Rounds of 1,000 connects (5,000 in all), each round's data connections let go before the
    // next: never more than 32 dialled at once, and 120 in all within the minute.
    for _ in 0..5 {
        for _ in 0..1000 {
            connect(&mut control, &id(next)).await;
            next += 1;
        }
        let held = data_connections(&mut fake, WATCH).await;
        assert!(held.len() <= 32, "{} at once", held.len());
        dialled.push(held.len());
        // Each fails its handshake as it goes, and gives its seat back.
        drop(held);
        sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(dialled, [32, 32, 32, 24, 0], "{dialled:?}");
}

#[tokio::test]
async fn a_connection_id_seen_before_or_malformed_is_ignored() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    pair_link(&env.run).await;
    let (channel, mut control) = fake.register().await;
    connect(&mut control, &id(1)).await;
    let (path, first) = fake.next_within(WAIT).await.expect("dialled");
    assert_eq!(path, format!("/v1/hub/{channel}/{}", id(1)));
    drop(first);
    sleep(Duration::from_millis(100)).await;
    // The same id again, and ids or messages that are not well formed: nothing is dialled.
    connect(&mut control, &id(1)).await;
    for bad in [
        json!({"connect": id(0xab_cdef).to_uppercase()}),
        json!({"connect": &id(0xab_cdef)[..30]}),
        json!({"connect": format!("{}00", id(0xab_cdef))}),
        json!({"connect": format!("../{}", &id(0xab_cdef)[3..])}),
        json!({"connect": 7}),
        json!({"connect": id(0xab_cdef), "also": 1}),
    ] {
        control
            .send(Message::text(bad.to_string()))
            .await
            .expect("sent");
    }
    control.send(Message::text("not json")).await.expect("sent");
    let more = fake.next_within(WATCH).await.map(|(path, _)| path);
    assert_eq!(more, None, "nothing more is dialled");
    // The hub still answers a good one.
    connect(&mut control, &id(3)).await;
    let (path, _) = fake.next_within(WAIT).await.expect("dialled");
    assert!(path.ends_with(&id(3)), "{path}");
}

#[tokio::test]
async fn a_hub_that_made_a_new_key_starts_with_no_devices() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    // Devices from before, with no key file: they pinned another key.
    let mut store = Store::open(&env.root).await.expect("the store");
    let secret = store.pairing_secret(SystemTime::now());
    store
        .pair(
            &secret,
            "Old phone",
            "ios",
            &PhoneKey::new().public_hex(),
            "wss://r.example",
            SystemTime::now(),
        )
        .await
        .expect("paired");
    assert!(
        store
            .set_push_token(
                &store.devices()[0].key.clone(),
                Some("ExpoPushToken[old]".into())
            )
            .await
            .expect("saved")
    );
    env.start(&relay.url()).await;
    assert_eq!(env.status().await["devices"], 0);
    let devices = std::fs::read_to_string(env.root.join("devices.json")).expect("devices.json");
    assert!(
        !devices.contains("Old phone") && !devices.contains("ExpoPushToken"),
        "{devices}"
    );
    // A hub that keeps its key keeps its devices.
    pair(&env.run, "New phone").await;
    env.stop().await;
    env.start(&relay.url()).await;
    assert_eq!(env.status().await["devices"], 1);
    relay_state(&env.run, "registered").await;
}
