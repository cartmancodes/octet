//! The hub's link to the relay end to end: when it dials, how it registers and comes back, the
//! phones it answers, and the limits it keeps whatever the relay sends. A real relay runs in
//! process where one is needed; a fake one (a WebSocket server in the test) plays a relay that
//! floods, replays, or hands the channel to another hub.
//!
//! The paused-clock tests set everything up in real time and pause only for the wait they
//! measure: paused from the start, tokio's clock can jump while a loopback write is still on its
//! way (macOS delivers loopback traffic asynchronously) and fire a timer early. One that must
//! keep traffic flowing while the clock moves (a phone answering pings for two minutes) moves it
//! by hand ([`crawl`]).
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use octet_hub::{Hub, Store};
use octet_testkit::relay::{
    Held, Link, Phone, PhoneKey, Relay, Socket, control, hex, hold, hold_with, open_pairing, pair,
    pair_answer, pair_frame, pair_link, pair_wait, relay_state,
};
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
    net::TcpListener,
    sync::{mpsc, oneshot},
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, accept_hdr_async,
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

/// A socket the fake relay accepted: on a plain stream, so a test phone can talk over it.
type Ws = Socket;

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
                    let stream = MaybeTlsStream::Plain(stream);
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
    let _pairing = open_pairing(&env.run);
    let (path, _) = fake.next_within(WAIT).await.expect("the hub dials");
    assert!(path.starts_with("/v1/hub/"), "{path}");
}

#[tokio::test]
async fn status_reports_the_relay_state() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    assert_eq!(env.status().await["relay"], "off");
    let _pairing = open_pairing(&env.run);
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

/// A relay whose side of the control connection is gone with no FIN reaching the Mac (a laptop
/// that slept, a new Wi-Fi network) leaves the socket open and silent: no pings, nothing. The hub
/// gives it up 50 seconds on, two of the relay's 20-second pings and a margin, says it is no
/// longer registered, and dials again.
#[tokio::test]
async fn a_relay_that_goes_silent_is_dropped_after_50_seconds_and_dialled_again() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    let _pairing = open_pairing(&env.run);
    // Registered, then never another word; the socket is kept open, and never read.
    let (_, silent) = fake.register().await;
    relay_state(&env.run, "registered").await;
    tokio::time::pause();
    let gave_up = crawl(Duration::from_secs(120), || env.logged("Lost the relay")).await;
    tokio::time::resume();
    let gave_up = gave_up.expect("the hub gives up a silent relay");
    assert!(
        (Duration::from_secs(49)..=Duration::from_secs(51)).contains(&gave_up),
        "{gave_up:?}"
    );
    // No longer registered, and it dials again.
    relay_state(&env.run, "connecting").await;
    let _again = fake.register().await;
    relay_state(&env.run, "registered").await;
    drop(silent);
}

#[tokio::test]
async fn a_replaced_hub_backs_off_and_logs_another_hub_took_this_channel() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    let _pairing = open_pairing(&env.run);
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
    let _pairing = open_pairing(&env.run);
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
    let _pairing = open_pairing(&env.run);
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
    let _pairing = open_pairing(&env.run);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hub_stays_on_the_relay_while_a_phone_waits_to_hear_it_is_paired() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let Held {
        mut phone, link, ..
    } = hold(&env.run, "Slow phone").await;
    let pending = pending(&env, link.attempt).await;
    // Another process holds the devices file's lock: the yes's save waits on it, with the secret
    // spent and nothing paired yet.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(env.root.join("devices.lock"))
        .expect("the lock file");
    lock.lock().expect("the lock");
    let run = env.run.clone();
    let answer = tokio::spawn(async move { pair_answer(&run, pending, true).await });
    // Past the hub's every-second look: the phone waits for its answer, so the link stays up.
    assert!(
        !phone.closes_within(Duration::from_millis(3_500)).await,
        "the link went while the phone waited"
    );
    assert_eq!(env.status().await["relay"], "registered");
    drop(lock);
    let answer = answer.await.expect("answered");
    assert_eq!(answer, json!({"paired": true, "device": "Slow phone"}));
    let paired = phone.recv().await;
    assert_eq!(paired["type"], "paired", "{paired}");
    assert_eq!(env.status().await["devices"], 1);
}

/// The pairing the hub holds for the opening `attempt`, as `pair_wait` gives it: its id.
async fn pending(env: &Env, attempt: u64) -> u64 {
    let question = pair_wait(&env.run, attempt).await;
    question["pending"]
        .as_u64()
        .unwrap_or_else(|| panic!("{question}"))
}

/// The names of the devices paired with `env`'s hub, as `devices.json` has them.
async fn names(env: &Env) -> Vec<String> {
    let store = Store::open(&env.root).await.expect("the store");
    store.devices().iter().map(|d| d.name.clone()).collect()
}

/// Waits until the hub holds no phone: `pair_wait` for the opening `attempt` has no question,
/// and with the secret spent, answers `expired`.
async fn no_phone_held(env: &Env, attempt: u64) {
    timeout(WAIT, async {
        while pair_wait(&env.run, attempt).await != json!({"expired": true}) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the hub lets the phone go");
}

/// A `pair_wait` for the opening `attempt` the hub has taken: two are asked, the hub refuses the
/// one it sees second, and the other is the one that waits.
async fn waiting(env: &Env, attempt: u64) -> tokio::task::JoinHandle<Value> {
    let ask = || {
        let run = env.run.clone();
        tokio::spawn(async move { pair_wait(&run, attempt).await })
    };
    let (mut first, mut second) = (ask(), ask());
    let (refused, waits) = tokio::select! {
        answer = &mut first => (answer, second),
        answer = &mut second => (answer, first),
    };
    assert_eq!(
        refused.expect("asked"),
        json!({"error": "Another pairing is open"})
    );
    waits
}

/// A refusal frame as the hub sends it.
fn refusal(req: u64, code: &str, reason: &str) -> Value {
    json!({"v": 1, "type": "refused", "req": req, "code": code, "reason": reason})
}

#[tokio::test]
async fn an_accepted_pairing_stores_the_key_and_answers_paired() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let Held {
        link,
        key,
        mut phone,
        ..
    } = hold(&env.run, "Test phone").await;
    // Nothing is stored before the answer.
    assert_eq!(env.status().await["devices"], 0);
    let pending = pending(&env, link.attempt).await;
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"paired": true, "device": "Test phone"})
    );
    let paired = phone.recv().await;
    assert_eq!(
        (paired["type"].as_str(), paired["device"].as_str()),
        (Some("paired"), Some("Test phone")),
        "{paired}"
    );
    // The phone says hello on a connection of its own.
    assert!(phone.closes().await);
    let store = Store::open(&env.root).await.expect("the store");
    let device = &store.devices()[0];
    assert_eq!(
        (device.name.as_str(), device.key.as_str()),
        ("Test phone", key.public_hex().as_str())
    );
    let mut again = Phone::connect(&link, &key).await.expect("connects");
    again
        .send(&json!({"v": 1, "req": 2, "type": "hello"}))
        .await;
    assert_eq!(again.recv().await["type"], "welcome");
    // The answer was for that phone alone.
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"error": "No phone is waiting for that answer"})
    );
}

#[tokio::test]
async fn a_refused_pairing_stores_nothing_and_spends_the_secret() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    // A phone paired already keeps the relay link up.
    pair(&env.run, "First phone").await;
    let Held {
        link, mut phone, ..
    } = hold(&env.run, "Second phone").await;
    let pending = pending(&env, link.attempt).await;
    assert_eq!(
        pair_answer(&env.run, pending, false).await,
        json!({"paired": false})
    );
    assert_eq!(
        phone.recv().await,
        refusal(1, "unpaired", "Pairing was refused on the Mac")
    );
    assert!(phone.closes().await);
    assert_eq!(names(&env).await, ["First phone"]);
    // The secret went with the refusal.
    let mut other = Phone::connect(&link, &PhoneKey::new())
        .await
        .expect("connects");
    other.send(&pair_frame(&link.secret, "Third phone")).await;
    assert_eq!(
        other.recv().await,
        refusal(1, "unpaired", "Pairing code already used or expired")
    );
    assert!(other.closes().await);
}

/// Moves the paused clock on in half-second steps, giving the loopback traffic between them a
/// few milliseconds of real time (the runtime is kept busy, so the clock never jumps on its
/// own), until `done` says so or `limit` has passed; how long it took on the clock.
async fn crawl(limit: Duration, mut done: impl FnMut() -> bool) -> Option<Duration> {
    let began = Instant::now();
    while began.elapsed() < limit {
        tokio::time::advance(Duration::from_millis(500)).await;
        let real = std::time::Instant::now();
        while real.elapsed() < Duration::from_millis(3) {
            tokio::task::yield_now().await;
        }
        if done() {
            return Some(began.elapsed());
        }
    }
    None
}

#[tokio::test]
async fn an_unanswered_pairing_times_out_after_two_minutes() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let Held { phone, link, .. } = hold(&env.run, "Slow phone").await;
    let pending = pending(&env, link.attempt).await;
    // The phone reads in a task of its own, answering the hub's pings as the clock moves.
    let (frames, mut heard) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        let mut phone = phone;
        while let Some(frame) = phone.next().await {
            if frames.send(frame).is_err() {
                return;
            }
        }
    });
    tokio::time::pause();
    let mut frame = None;
    let elapsed = crawl(Duration::from_secs(150), || {
        frame = heard.try_recv().ok();
        frame.is_some()
    })
    .await
    .expect("the phone hears in time");
    tokio::time::resume();
    // The hub looks every second: within one of the two minutes.
    assert!(
        (Duration::from_secs(120)..=Duration::from_secs(122)).contains(&elapsed),
        "{elapsed:?}"
    );
    assert_eq!(frame, Some(refusal(1, "unpaired", "Pairing timed out")));
    timeout(WAIT, reader)
        .await
        .expect("the connection ends")
        .expect("the reader");
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"error": "Pairing timed out"})
    );
    assert_eq!(env.status().await["devices"], 0);
}

#[tokio::test]
async fn a_held_phone_outlives_the_10_second_deadline_and_counts_as_unauthenticated() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    let opening = open_pairing(&env.run);
    let (_, mut control) = fake.register().await;
    let answer = opening.await.expect("asked").expect("answered");
    let link = Link::parse(answer["link"].as_str().expect("a link")).expect("a pairing link");
    // The phone arrives: the hub dials its data connection, and the test plays the phone there.
    connect(&mut control, &id(0)).await;
    let (_, ws) = fake.next_within(WAIT).await.expect("dialled");
    let mut phone = Phone::over(ws, &link.identity, &PhoneKey::new())
        .await
        .expect("the handshake");
    phone.send(&pair_frame(&link.secret, "Held phone")).await;
    assert_eq!(phone.recv().await["type"], "pair_code");
    // Twelve seconds on from its connect, the hub still holds it.
    tokio::time::pause();
    sleep(Duration::from_secs(12)).await;
    tokio::time::resume();
    assert!(phone.quiet().await, "the held phone was let go");
    // It takes one of the 32 places for phones not past their first frame: 31 more fill them.
    for n in 1..32 {
        connect(&mut control, &id(n)).await;
    }
    let others = data_connections(&mut fake, WATCH).await;
    assert_eq!(others.len(), 31);
    connect(&mut control, &id(100)).await;
    assert!(fake.next_within(WATCH).await.is_none(), "the room is full");
    assert!(phone.quiet().await, "the held phone was let go");
}

#[tokio::test]
async fn the_held_phone_gets_the_code_the_mac_is_shown() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let link = pair_link(&env.run).await;
    let question = waiting(&env, link.attempt).await;
    let Held { code, .. } = hold_with(&link, "Test phone").await;
    let question = question.await.expect("answered");
    let digits = question["code"].as_str().expect("a code").to_owned();
    assert!(
        digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()),
        "{question}"
    );
    assert_eq!(
        code,
        json!({"v": 1, "type": "pair_code", "req": 1, "code": digits})
    );
    let pending = question["pending"].as_u64().expect("an id");
    assert_eq!(
        question,
        json!({"pending": pending, "code": digits, "name": "Test phone", "platform": "ios",
            "replaces": false})
    );
    // Asked again, the hub gives the same question.
    assert_eq!(pair_wait(&env.run, link.attempt).await, question);
}

#[tokio::test]
async fn a_second_pair_while_one_waits_is_refused_busy_and_gets_no_code() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let Held {
        link, mut phone, ..
    } = hold(&env.run, "First phone").await;
    let mut second = Phone::connect(&link, &PhoneKey::new())
        .await
        .expect("connects");
    second.send(&pair_frame(&link.secret, "Second phone")).await;
    assert_eq!(
        second.recv().await,
        refusal(1, "busy", "Another phone is pairing")
    );
    assert!(second.closes().await);
    // The first is still the one waiting, and a yes pairs it alone.
    let pending = pending(&env, link.attempt).await;
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"paired": true, "device": "First phone"})
    );
    assert_eq!(phone.recv().await["type"], "paired");
    assert_eq!(names(&env).await, ["First phone"]);
}

#[tokio::test]
async fn a_second_phone_with_a_used_secret_is_refused_and_gets_no_code() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, _, _) = pair(&env.run, "First phone").await;
    let mut second = Phone::connect(&link, &PhoneKey::new())
        .await
        .expect("connects");
    second.send(&pair_frame(&link.secret, "Second phone")).await;
    assert_eq!(
        second.recv().await,
        refusal(1, "unpaired", "Pairing code already used or expired")
    );
    assert!(second.closes().await);
    assert_eq!(names(&env).await, ["First phone"]);
}

#[tokio::test]
async fn a_first_message_replayed_fifty_times_gets_no_code_and_pairs_nothing() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    let opening = open_pairing(&env.run);
    let (_, mut control) = fake.register().await;
    let answer = opening.await.expect("asked").expect("answered");
    let link = Link::parse(answer["link"].as_str().expect("a link")).expect("a pairing link");
    // A hostile relay keeps a copy of a phone's first handshake message and of its sealed
    // `pair`, which it does not pass on.
    let key = PhoneKey::new();
    connect(&mut control, &id(0)).await;
    let (_, ws) = fake.next_within(WAIT).await.expect("dialled");
    let mut phone = Phone::over(ws, &link.identity, &key)
        .await
        .expect("the handshake");
    let first = phone.first_message().to_vec();
    let sealed = phone.seal_frame(&pair_frame(&link.secret, "Real phone"));
    drop(phone);
    // It plays them to the hub fifty times. The hub answers each first message (alone, it proves
    // nothing), with keys of its own the copy cannot use: the sealed `pair` does not open, and
    // the hub closes having sent nothing more.
    for n in 1..=50 {
        connect(&mut control, &id(n)).await;
        let (_, mut ws) = fake.next_within(WAIT).await.expect("dialled");
        ws.send(Message::binary(first.clone())).await.expect("sent");
        let answer = timeout(WAIT, ws.next()).await.expect("an answer");
        let Some(Ok(Message::Binary(answer))) = answer else {
            panic!("replay {n}: {answer:?}")
        };
        assert_eq!(answer.len(), 48, "replay {n}");
        for message in &sealed {
            let _ = ws.send(Message::binary(message.clone())).await;
        }
        let rest = timeout(WAIT, async {
            let mut binary = 0;
            while let Some(Ok(message)) = ws.next().await {
                binary += usize::from(message.is_binary());
            }
            binary
        })
        .await
        .expect("the hub closes");
        assert_eq!(rest, 0, "replay {n}: the hub sent more");
    }
    // Nothing was held and the secret is unspent: the phone itself, on a handshake of its own,
    // is the one the hub holds.
    connect(&mut control, &id(51)).await;
    let (_, ws) = fake.next_within(WAIT).await.expect("dialled");
    let mut phone = Phone::over(ws, &link.identity, &key)
        .await
        .expect("the handshake");
    phone.send(&pair_frame(&link.secret, "Real phone")).await;
    assert_eq!(phone.recv().await["type"], "pair_code");
    assert_eq!(
        pair_wait(&env.run, attempt_of(&answer)).await["name"],
        "Real phone"
    );
    assert_eq!(env.status().await["devices"], 0);
}

#[tokio::test]
async fn a_phone_that_leaves_while_the_question_is_open_makes_the_answer_fail_with_the_phone_left()
{
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let Held {
        mut phone, link, ..
    } = hold(&env.run, "Leaving phone").await;
    let pending = pending(&env, link.attempt).await;
    phone.close().await;
    no_phone_held(&env, link.attempt).await;
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"error": "The phone left"})
    );
    assert_eq!(env.status().await["devices"], 0);
}

#[tokio::test]
async fn the_secret_stays_spent_after_the_held_phone_leaves() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    // A phone paired already keeps the relay link up.
    pair(&env.run, "First phone").await;
    let Held {
        link, mut phone, ..
    } = hold(&env.run, "Leaving phone").await;
    phone.close().await;
    no_phone_held(&env, link.attempt).await;
    let mut again = Phone::connect(&link, &PhoneKey::new())
        .await
        .expect("connects");
    again.send(&pair_frame(&link.secret, "Leaving phone")).await;
    assert_eq!(
        again.recv().await,
        refusal(1, "unpaired", "Pairing code already used or expired")
    );
    assert!(again.closes().await);
    assert_eq!(names(&env).await, ["First phone"]);
}

#[tokio::test]
async fn a_pairing_under_a_name_already_held_says_replaces_and_swaps_the_key() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, old_key, _) = pair(&env.run, "Test phone").await;
    let mut old = Phone::connect(&link, &old_key).await.expect("connects");
    old.send(&json!({"v": 1, "req": 2, "type": "hello"})).await;
    old.recv_kind("welcome").await;
    let token = "ExpoPushToken[old-key]";
    old.send(&json!({"v": 1, "req": 3, "type": "register_push", "token": token}))
        .await;
    assert_eq!(old.recv_kind("ack").await["req"], 3);
    // The same phone, with a new key after a wiped app, pairs under the same name.
    let Held {
        key,
        mut phone,
        link,
        ..
    } = hold(&env.run, "Test phone").await;
    let question = pair_wait(&env.run, link.attempt).await;
    assert_eq!(question["replaces"], true, "{question}");
    let pending = question["pending"].as_u64().expect("an id");
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"paired": true, "device": "Test phone"})
    );
    assert_eq!(phone.recv().await["type"], "paired");
    // The old key's connection closes, and its hello is refused.
    assert!(old.closes().await, "the old key stayed connected");
    let mut stale = Phone::connect(&link, &old_key).await.expect("connects");
    stale
        .send(&json!({"v": 1, "req": 4, "type": "hello"}))
        .await;
    assert_eq!(stale.recv().await["code"], "unpaired");
    // One device, the new key, and the old key's push token went with it.
    let store = Store::open(&env.root).await.expect("the store");
    let devices = store.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].key, key.public_hex());
    assert_eq!(devices[0].push_token, None);
    let file = std::fs::read_to_string(env.root.join("devices.json")).expect("devices.json");
    assert!(
        !file.contains(token) && !file.contains(&old_key.public_hex()),
        "{file}"
    );
}

#[tokio::test]
async fn a_device_that_cannot_be_saved_refuses_the_phone_and_says_so() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let Held {
        mut phone, link, ..
    } = hold(&env.run, "Test phone").await;
    let pending = pending(&env, link.attempt).await;
    // The devices file goes bad under the hub: the yes's save fails.
    std::fs::write(env.root.join("devices.json"), "not json").expect("written");
    assert_eq!(
        pair_answer(&env.run, pending, true).await,
        json!({"error": "Could not save the device"})
    );
    assert_eq!(
        phone.recv().await,
        refusal(1, "unpaired", "Could not save the device")
    );
    assert!(phone.closes().await);
}

#[tokio::test]
async fn pair_secret_while_a_phone_waits_is_refused() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let held = hold(&env.run, "Test phone").await;
    assert_eq!(
        control(&env.run, &json!({"type": "pair_secret"})).await,
        Some(json!({"link": null, "reason": "A phone is waiting for an answer"}))
    );
    assert_eq!(
        pair_wait(&env.run, held.link.attempt).await["name"],
        "Test phone"
    );
}

#[tokio::test]
async fn a_new_pair_secret_expires_the_wait_on_the_old() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let old = pair_link(&env.run).await;
    let wait = waiting(&env, old.attempt).await;
    let new = pair_link(&env.run).await;
    assert_eq!(wait.await.expect("answered"), json!({"expired": true}));
    // The old secret was replaced: a phone with it is refused.
    assert_ne!(old.secret, new.secret);
    let mut phone = Phone::connect(&old, &PhoneKey::new())
        .await
        .expect("connects");
    phone.send(&pair_frame(&old.secret, "Test phone")).await;
    assert_eq!(phone.recv().await["code"], "unpaired");
}

/// The attempt a `pair_secret` answer names.
fn attempt_of(answer: &Value) -> u64 {
    answer["attempt"]
        .as_u64()
        .unwrap_or_else(|| panic!("no attempt: {answer}"))
}

#[tokio::test]
async fn a_superseded_opening_gets_no_link_and_its_wait_is_expired() {
    let mut fake = FakeRelay::start().await;
    let mut env = Env::new();
    env.start(&fake.url).await;
    // Two openings while the relay link comes up, as `octet pair` and the terminal UI might.
    let first = open_pairing(&env.run);
    relay_state(&env.run, "connecting").await;
    let second = open_pairing(&env.run);
    // The hub reads the second request before the relay has it.
    sleep(Duration::from_millis(300)).await;
    let (_, mut control) = fake.register().await;
    let first = first.await.expect("asked").expect("answered");
    let second = second.await.expect("asked").expect("answered");
    assert_eq!(first["link"], Value::Null, "{first}");
    assert_eq!(first["expired"], true, "{first}");
    let link = Link::parse(second["link"].as_str().expect("a link")).expect("a pairing link");
    let (old, new) = (attempt_of(&first), attempt_of(&second));
    assert_ne!(old, new);
    // The first opener's wait is expired at once and takes no waiter: the second's hears the phone.
    assert_eq!(pair_wait_for(&env.run, old).await, json!({"expired": true}));
    let wait = {
        let run = env.run.clone();
        tokio::spawn(async move { pair_wait_for(&run, new).await })
    };
    sleep(Duration::from_millis(100)).await;
    connect(&mut control, &id(0)).await;
    let (_, ws) = fake.next_within(WAIT).await.expect("dialled");
    let mut phone = Phone::over(ws, &link.identity, &PhoneKey::new())
        .await
        .expect("the handshake");
    phone.send(&pair_frame(&link.secret, "Test phone")).await;
    let code = phone.recv().await;
    assert_eq!(code["type"], "pair_code", "{code}");
    assert_eq!(wait.await.expect("answered")["code"], code["code"]);
    // With that phone held, the first opener still hears only that its link expired.
    assert_eq!(pair_wait_for(&env.run, old).await, json!({"expired": true}));
}

/// `pair_wait` for the opening `attempt`, answered within [`WAIT`].
async fn pair_wait_for(run: &std::path::Path, attempt: u64) -> Value {
    ask(run, &json!({"type": "pair_wait", "attempt": attempt}), WAIT).await
}

#[tokio::test]
async fn pair_wait_without_an_attempt_is_refused() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let _link = pair_link(&env.run).await;
    for request in [
        json!({"type": "pair_wait"}),
        json!({"type": "pair_wait", "attempt": "1"}),
    ] {
        assert_eq!(
            ask(&env.run, &request, WAIT).await,
            json!({"error": "pair_wait needs attempt"}),
            "{request}"
        );
    }
}

#[tokio::test]
async fn a_second_pair_wait_is_refused() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let link = pair_link(&env.run).await;
    // Of two waits, the hub refuses one; the other still waits, and hears of the phone.
    let wait = waiting(&env, link.attempt).await;
    let Held { code, .. } = hold_with(&link, "Test phone").await;
    let question = wait.await.expect("answered");
    assert_eq!(question["code"], code["code"]);
}

/// One request on the control socket in `run`, and its answer, waiting up to `within`.
async fn ask(run: &std::path::Path, request: &Value, within: Duration) -> Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let asked = async {
        let stream = tokio::net::UnixStream::connect(run.join("hub.sock"))
            .await
            .expect("the control socket");
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("asked");
        let mut line = String::new();
        BufReader::new(read)
            .read_line(&mut line)
            .await
            .expect("an answer");
        serde_json::from_str(&line).expect("JSON")
    };
    timeout(within, asked).await.expect("answered in time")
}

#[tokio::test]
async fn pair_secret_with_the_relay_down_gives_the_reason() {
    // A port nothing listens on.
    let free = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let addr = free.local_addr().expect("an address");
    drop(free);
    let mut env = Env::new();
    env.start(&format!("ws://{addr}")).await;
    tokio::time::pause();
    let began = Instant::now();
    let answer = ask(
        &env.run,
        &json!({"type": "pair_secret"}),
        Duration::from_secs(60),
    )
    .await;
    let waited = began.elapsed();
    tokio::time::resume();
    assert_eq!(
        answer,
        json!({"link": null, "reason": format!("Can't reach the relay at {addr}")})
    );
    assert!(
        (Duration::from_secs(10)..Duration::from_secs(11)).contains(&waited),
        "{waited:?}"
    );
}

#[tokio::test]
async fn an_unknown_key_whose_first_frame_is_not_pair_or_hello_is_closed() {
    let relay = Relay::start().await;
    let mut env = Env::new();
    env.start(&relay.url()).await;
    let (link, _, _) = pair(&env.run, "Test phone").await;
    for (frame, code) in [
        (json!({"v": 1, "req": 5, "type": "ping"}), "unpaired"),
        (
            json!({"v": 1, "req": 5, "type": "subscribe", "session": "1-2"}),
            "unpaired",
        ),
        (
            json!({"v": 1, "req": 5, "type": "register_push", "token": null}),
            "unpaired",
        ),
        (json!({"v": 1, "req": 5, "type": "launch"}), "invalid"),
    ] {
        let mut stranger = Phone::connect(&link, &PhoneKey::new())
            .await
            .expect("connects");
        stranger.send(&frame).await;
        let refused = stranger.recv().await;
        assert_eq!(
            (
                refused["type"].as_str(),
                refused["req"].as_u64(),
                refused["code"].as_str()
            ),
            (Some("refused"), Some(5), Some(code)),
            "{frame}: {refused}"
        );
        assert!(stranger.closes().await, "{frame}");
    }
}
