//! The hub's phone API end to end: a fake bridge (a Unix socket and a written registration), real
//! journal files, an in-process relay, and phones that reach the hub through it over the Noise
//! channel, as the app does.
use octet_core::remote::{
    protocol::{
        Code, ack, bridge_catalog_frame, bridge_state_frame, parse_bridge_command, refused,
    },
    run::Registration,
};
use octet_hub::{Hub, Store};
use octet_testkit::relay::{Link, Phone, PhoneKey, Relay, control, pair, pair_link, relay_state};
use serde_json::{Value, json};
use std::{
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::timeout,
};

/// How long any one step may take before the test fails.
const WAIT: Duration = Duration::from_secs(5);
/// The journal every fake session writes.
const JOURNAL: &str = "session-1-1.jsonl";

/// A data directory under `/tmp` (socket paths are limited to about 104 bytes), the relay, and
/// the hub serving it once started.
struct Env {
    root: PathBuf,
    run: PathBuf,
    relay: Option<Relay>,
    /// The hub's link, once a pairing gave it: its identity and channel never change.
    link: Mutex<Option<Link>>,
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
            relay: None,
            link: Mutex::new(None),
            stop: None,
        }
    }

    /// Starts a relay and the hub, pointed at it, and waits until its control socket answers.
    async fn start(&mut self) {
        let relay = Relay::start().await;
        let url = relay.url();
        self.relay = Some(relay);
        self.start_hub(url).await;
    }

    /// Starts the hub, dialling `relay`, and waits until its control socket answers.
    async fn start_hub(&mut self, relay: String) {
        let (stop, stopped) = oneshot::channel::<()>();
        let directory = self.root.clone();
        tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            let log = Arc::new(|_: &str| {});
            Hub::serve_with(directory, relay, log, shutdown, oneshot::channel().0).await
        });
        self.stop = Some(stop);
        timeout(WAIT, async {
            while control(&self.run, &json!({"type": "status"}))
                .await
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
    }

    /// One request on the control socket, and its answer.
    async fn control(&self, request: Value) -> Value {
        control(&self.run, &request)
            .await
            .expect("the control socket answers")
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
        self.bridge_with(pid, seqs, from_phone, "/work".into())
            .await
    }

    /// A fake bridge for `pid` registered as running in `workspace`.
    async fn bridge_in(&self, pid: u32, workspace: &Path) -> Bridge {
        self.bridge_with(pid, 0..1, false, workspace.to_owned())
            .await
    }

    async fn bridge_with(
        &self,
        pid: u32,
        seqs: std::ops::Range<u64>,
        from_phone: bool,
        workspace: PathBuf,
    ) -> Bridge {
        self.journal(JOURNAL, seqs);
        let registration = Registration {
            id: format!("{pid}-1790000000"),
            pid,
            started: 1_790_000_000,
            socket: self.run.join(format!("{pid}.sock")),
            engine: "claude".into(),
            workspace,
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

    /// The hub's link, from the pairing that gave it, or else from a new pairing secret (which
    /// brings the relay link up).
    async fn link(&self) -> Link {
        let known = self.link.lock().expect("lock").clone();
        if let Some(link) = known {
            return link;
        }
        let link = pair_link(&self.run).await;
        *self.link.lock().expect("lock") = Some(link.clone());
        link
    }

    /// Pairs a phone called `name` and returns its key.
    async fn pair(&self, name: &str) -> PhoneKey {
        let (link, key, paired) = pair(&self.run, name).await;
        assert_eq!(paired["device"], name, "{paired}");
        *self.link.lock().expect("lock") = Some(link);
        key
    }

    /// A phone past its handshake as `key`, once the relay has the hub, before it says anything.
    async fn connect(&self, key: &PhoneKey) -> Phone {
        let link = self.link().await;
        relay_state(&self.run, "registered").await;
        Phone::connect(&link, key)
            .await
            .expect("the phone connects")
    }

    /// A phone whose key the hub does not know, past its handshake.
    async fn stranger(&self) -> Phone {
        self.connect(&PhoneKey::new()).await
    }

    /// A fresh connection of `key` that said hello, and the `sessions` frame it got.
    async fn hello(&self, key: &PhoneKey) -> (Phone, Value) {
        let mut phone = self.connect(key).await;
        send(&mut phone, json!({"v": 1, "req": 2, "type": "hello"})).await;
        let welcome = recv(&mut phone).await;
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        let sessions = recv(&mut phone).await;
        assert_eq!(sessions["type"], "sessions", "{sessions}");
        (phone, sessions)
    }

    /// A paired phone that said hello, and the `sessions` frame it got.
    async fn phone(&self, name: &str) -> (Phone, Value) {
        let key = self.pair(name).await;
        self.hello(&key).await
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

async fn send(phone: &mut Phone, message: Value) {
    phone.send(&message).await;
}

/// The next frame, parsed; fails if none comes or the connection ends.
async fn recv(phone: &mut Phone) -> Value {
    phone.recv().await
}

/// The next frame of type `kind`, passing over others (such as `sessions`).
async fn recv_kind(phone: &mut Phone, kind: &str) -> Value {
    phone.recv_kind(kind).await
}

/// Whether the connection ends soon; frames before the end are passed over.
async fn closes(phone: &mut Phone) -> bool {
    phone.closes().await
}

/// Whether no frame arrives for a while.
async fn quiet(phone: &mut Phone) -> bool {
    phone.quiet().await
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
    assert_eq!(status, json!({"relay": "off", "devices": 0}));
    let link = env.link().await;
    assert_eq!(link.channel.len(), 64);
    assert_eq!(link.relay(), env.relay.as_ref().expect("a relay").url());
    assert!(env.control(json!({"type": "nope"})).await["error"].is_string());

    let key = env.pair("Test phone").await;
    assert_eq!(env.control(json!({"type": "status"})).await["devices"], 1);
    let devices = Store::open(&env.root).await.expect("the store");
    let device = &devices.devices()[0];
    assert_eq!(device.key, key.public_hex(), "a device is its key");
    assert_eq!(device.relay, env.relay.as_ref().expect("a relay").url());
    let (_phone, sessions) = env.hello(&key).await;
    let rows = sessions["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{sessions}");
    assert_eq!(rows[0]["id"], bridge.id);
    assert_eq!(rows[0]["engine"], "claude");
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
    let mut stranger = env.stranger().await;
    send(&mut stranger, prompt(41, &session)).await;
    let refusal = recv(&mut stranger).await;
    assert_eq!(refusal["type"], "refused", "{refusal}");
    assert_eq!(refusal["req"], 41);
    assert_eq!(refusal["code"], "unpaired");
    assert!(closes(&mut stranger).await);
    // A hello from a key the hub does not know, with an old app's token: refused and closed.
    let mut liar = env.stranger().await;
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

    // A frame over the limit closes the connection.
    let (mut big, _) = env.phone("Big phone").await;
    let text = "x".repeat(200 * 1024);
    big.send_text(&prompt(5, &session).to_string().replace("hi", &text))
        .await;
    assert!(closes(&mut big).await);

    // A revoked device's next frame closes it, and its idle connections close soon after.
    let token = env.pair("Lost phone").await;
    let (mut revoked, _) = env.hello(&token).await;
    let (mut idle, _) = env.hello(&token).await;
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
    let (mut idle, _) = env.hello(&token).await;
    let (mut chatty, _) = env.hello(&token).await;
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
    let (mut phone, _) = env.hello(&token).await;
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
    let (mut again, _) = env.hello(&token).await;
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
    let (mut fresh, _) = env.hello(&token).await;
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
    let (mut racer, _) = env.hello(&token).await;
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
    // The bridge's link closes with the command unanswered, the session still registered: it may
    // have run the command, so the phone is told to look before sending it again.
    let (mut read, write) = bridge.accept().await;
    send(&mut phone, prompt(1)).await;
    assert_eq!(bridge_line(&mut read).await["type"], "prompt");
    drop((read, write));
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(
            1,
            Code::Unavailable,
            "Lost the link to that session; check it before sending again"
        )
    );
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
    // The row follows too, within a quarter second of the last list.
    loop {
        let sessions = recv_kind(&mut phone, "sessions").await;
        if sessions["sessions"][0]["phase"] == "idle" {
            break;
        }
    }
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
    let (_phone, sessions) = env.hello(&token).await;
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
        .filter(|line: &Value| line["what"] != "paired")
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

#[tokio::test]
async fn a_pairing_is_audited_without_its_key_or_secret() {
    let mut env = Env::new();
    env.start().await;
    let (link, key, _) = pair(&env.run, "Test phone").await;
    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let lines: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    assert_eq!(lines.len(), 1, "{audit}");
    assert_eq!(lines[0]["device"], "Test phone");
    assert_eq!(lines[0]["session"], "");
    assert_eq!(lines[0]["what"], "paired");
    assert!(
        !audit.contains(&key.public_hex()) && !audit.contains(&link.secret),
        "{audit}"
    );
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
        let mut stranger = env.stranger().await;
        let req = command["req"].clone();
        send(&mut stranger, command).await;
        let refusal = recv(&mut stranger).await;
        assert_eq!(refusal["type"], "refused", "{refusal}");
        assert_eq!(refusal["req"], req);
        assert_eq!(refusal["code"], "unpaired");
        assert!(closes(&mut stranger).await);
    }
}

const ACTIVITY: &str = "5ec7e7aaaa0123456789abcdef";
const START: &str = "5ec7e7bbbb0123456789abcdef";

fn register_activity(req: u64, token: Option<&str>, start_token: Option<&str>) -> Value {
    let mut message = json!({"v": 1, "req": req, "type": "register_activity"});
    if let Some(token) = token {
        message["token"] = json!(token);
    }
    if let Some(start_token) = start_token {
        message["start_token"] = json!(start_token);
    }
    message
}

/// Each device's stored activity tokens, by name, as a fresh store reads them.
async fn stored_activity(env: &Env) -> Vec<(String, Option<(Option<String>, Option<String>)>)> {
    Store::open(&env.root)
        .await
        .expect("the store")
        .devices()
        .iter()
        .map(|d| {
            (
                d.name.clone(),
                d.activity
                    .as_ref()
                    .map(|a| (a.token.clone(), a.start_token.clone())),
            )
        })
        .collect()
}

#[tokio::test]
async fn register_activity_is_stored_acked_audited_and_dropped_on_revoke() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let (_other, _) = env.phone("Other phone").await;

    send(
        &mut phone,
        register_activity(7, Some(ACTIVITY), Some(START)),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await,
        ack(7, "Activity registered")
    );
    assert_eq!(
        stored_activity(&env).await,
        [
            (
                "Test phone".to_owned(),
                Some((Some(ACTIVITY.to_owned()), Some(START.to_owned())))
            ),
            ("Other phone".to_owned(), None),
        ]
    );

    // A later registration replaces both; leaving out the start token clears it.
    let newer = "0123456789abcdef5ec7e7cccc";
    send(&mut phone, register_activity(8, Some(newer), None)).await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await,
        ack(8, "Activity registered")
    );
    assert_eq!(
        stored_activity(&env).await[0],
        (
            "Test phone".to_owned(),
            Some((Some(newer.to_owned()), None))
        )
    );
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(!text.contains(ACTIVITY) && !text.contains(START), "{text}");

    // With no activity running, the phone has only a push-to-start token, and says so.
    send(&mut phone, register_activity(9, None, Some(START))).await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await,
        ack(9, "Activity registered")
    );
    assert_eq!(
        stored_activity(&env).await[0],
        (
            "Test phone".to_owned(),
            Some((None, Some(START.to_owned())))
        )
    );

    // The audit names the device and the action, never a token.
    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let lines: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    let registered: Vec<_> = lines
        .iter()
        .filter(|l| l["what"] == "activity registered")
        .collect();
    assert_eq!(registered.len(), 3, "{audit}");
    assert!(
        registered
            .iter()
            .all(|l| l["device"] == "Test phone" && l["session"] == ""),
        "{audit}"
    );
    assert!(
        !audit.contains("5ec7e7") && !audit.contains("0123456789abcdef"),
        "{audit}"
    );

    // Revoking the device drops its tokens with it.
    let mut cli = Store::open(&env.root).await.expect("the store");
    assert!(cli.revoke("Test phone").await);
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(
        !text.contains(START) && !text.contains("\"activity\""),
        "{text}"
    );
    assert_eq!(
        stored_activity(&env).await,
        [("Other phone".to_owned(), None)]
    );
}

#[tokio::test]
async fn activity_tokens_as_ios_issues_them_are_registered_up_to_1024_digits() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    // iOS 26.5 issues 128-byte tokens: 256 hex digits.
    let ios = "0123456789abcdef".repeat(16);
    let longest = "b".repeat(1024);
    for (req, token, start) in [(1, &ios, &longest), (2, &longest, &ios)] {
        send(&mut phone, register_activity(req, Some(token), Some(start))).await;
        assert_eq!(
            recv_kind(&mut phone, "ack").await,
            ack(req, "Activity registered")
        );
        assert_eq!(
            stored_activity(&env).await,
            [(
                "Test phone".to_owned(),
                Some((Some(token.clone()), Some(start.clone())))
            )]
        );
    }
}

#[tokio::test]
async fn bad_activity_tokens_are_refused_as_invalid_and_the_connection_goes_on() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let too_long = "a".repeat(1025);
    let mut extra = register_activity(7, Some(ACTIVITY), None);
    extra["extra"] = json!(1);
    for command in [
        register_activity(1, Some("ABCDEF"), None),
        register_activity(2, Some("not-hex"), None),
        register_activity(3, Some(""), None),
        register_activity(4, Some(&too_long), None),
        register_activity(5, Some(ACTIVITY), Some("ABCDEF")),
        json!({"v": 1, "req": 6, "type": "register_activity"}),
        register_activity(8, None, Some("NOT-HEX")),
        extra,
    ] {
        let req = command["req"].as_u64();
        send(&mut phone, command).await;
        let refusal = recv_kind(&mut phone, "refused").await;
        assert_eq!(
            (refusal["req"].as_u64(), refusal["code"].as_str()),
            (req, Some("invalid")),
            "{refusal}"
        );
        assert!(
            !refusal.to_string().contains("5ec7e7"),
            "the refusal repeats a token: {refusal}"
        );
    }
    send(&mut phone, json!({"v": 1, "req": 9, "type": "ping"})).await;
    assert_eq!(recv_kind(&mut phone, "ack").await["message"], "pong");
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(!text.contains("\"activity\""), "{text}");
}

fn clear_activity(req: u64) -> Value {
    json!({"v": 1, "req": req, "type": "clear_activity"})
}

#[tokio::test]
async fn clear_activity_removes_the_tokens_and_is_acked_and_audited() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let (mut other, _) = env.phone("Other phone").await;
    let theirs = "0123456789abcdef5ec7e7dddd";
    send(
        &mut phone,
        register_activity(1, Some(ACTIVITY), Some(START)),
    )
    .await;
    recv_kind(&mut phone, "ack").await;
    send(&mut other, register_activity(1, Some(theirs), None)).await;
    recv_kind(&mut other, "ack").await;

    send(&mut phone, clear_activity(2)).await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await,
        ack(2, "Activity cleared")
    );
    assert_eq!(
        stored_activity(&env).await,
        [
            ("Test phone".to_owned(), None),
            (
                "Other phone".to_owned(),
                Some((Some(theirs.to_owned()), None))
            ),
        ]
    );
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(!text.contains(ACTIVITY) && !text.contains(START), "{text}");
    // Clearing again is still acked: there is nothing left to start one.
    send(&mut phone, clear_activity(3)).await;
    assert_eq!(
        recv_kind(&mut phone, "ack").await,
        ack(3, "Activity cleared")
    );
    // An extra field is refused, and the connection goes on.
    let mut extra = clear_activity(4);
    extra["token"] = json!(ACTIVITY);
    send(&mut phone, extra).await;
    let refusal = recv_kind(&mut phone, "refused").await;
    assert_eq!(
        (refusal["req"].as_u64(), refusal["code"].as_str()),
        (Some(4), Some("invalid")),
        "{refusal}"
    );
    send(&mut phone, json!({"v": 1, "req": 5, "type": "ping"})).await;
    assert_eq!(recv_kind(&mut phone, "ack").await["message"], "pong");

    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let lines: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    let cleared: Vec<_> = lines
        .iter()
        .filter(|l| l["what"] == "activity cleared")
        .collect();
    assert_eq!(cleared.len(), 2, "{audit}");
    assert!(
        cleared
            .iter()
            .all(|l| l["device"] == "Test phone" && l["session"] == ""),
        "{audit}"
    );
    assert!(!audit.contains("5ec7e7"), "{audit}");
}

#[tokio::test]
async fn a_failed_activity_write_is_refused_in_words_that_fit() {
    let mut env = Env::new();
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(
        &mut phone,
        register_activity(1, Some(ACTIVITY), Some(START)),
    )
    .await;
    recv_kind(&mut phone, "ack").await;
    std::fs::set_permissions(&env.root, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    // Root (a container) writes into a read-only directory, so there is nothing to test.
    if std::fs::write(env.root.join("probe"), b"x").is_ok() {
        std::fs::set_permissions(&env.root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        return;
    }
    send(&mut phone, clear_activity(2)).await;
    let cleared = recv_kind(&mut phone, "refused").await;
    send(&mut phone, register_activity(3, Some(START), None)).await;
    let registered = recv_kind(&mut phone, "refused").await;
    std::fs::set_permissions(&env.root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert_eq!(
        cleared,
        refused(
            2,
            Code::Unavailable,
            "Could not update the Live Activity; try again"
        )
    );
    assert_eq!(
        registered,
        refused(
            3,
            Code::Unavailable,
            "Could not save the Live Activity; try again"
        )
    );
    assert_eq!(
        stored_activity(&env).await[0],
        (
            "Test phone".to_owned(),
            Some((Some(ACTIVITY.to_owned()), Some(START.to_owned())))
        )
    );
}

#[tokio::test]
async fn activity_tokens_follow_the_newest_phone_to_register_them() {
    let mut env = Env::new();
    env.start().await;
    let (mut old, _) = env.phone("Old pairing").await;
    let (mut new, _) = env.phone("New pairing").await;
    send(&mut old, register_activity(1, Some(ACTIVITY), Some(START))).await;
    recv_kind(&mut old, "ack").await;
    send(&mut new, register_activity(1, Some(ACTIVITY), Some(START))).await;
    recv_kind(&mut new, "ack").await;
    assert_eq!(
        stored_activity(&env).await,
        [
            ("Old pairing".to_owned(), None),
            (
                "New pairing".to_owned(),
                Some((Some(ACTIVITY.to_owned()), Some(START.to_owned())))
            ),
        ]
    );
}

#[tokio::test]
async fn activity_commands_before_hello_are_refused_as_unpaired() {
    let mut env = Env::new();
    env.start().await;
    for command in [
        register_activity(44, Some(ACTIVITY), Some(START)),
        clear_activity(45),
    ] {
        let mut stranger = env.stranger().await;
        let req = command["req"].clone();
        send(&mut stranger, command).await;
        let refusal = recv(&mut stranger).await;
        assert_eq!(refusal["type"], "refused", "{refusal}");
        assert_eq!(refusal["req"], req);
        assert_eq!(refusal["code"], "unpaired");
        assert!(closes(&mut stranger).await);
    }
    let devices = env.root.join("devices.json");
    if let Ok(text) = std::fs::read_to_string(devices) {
        assert!(!text.contains(ACTIVITY), "{text}");
    }
}

#[tokio::test]
async fn register_activity_spends_the_rate_limit_like_any_command() {
    let mut env = Env::new();
    env.start().await;
    let (mut spender, _) = env.phone("Spender").await;
    for req in 1..=BURST as u64 {
        send(&mut spender, json!({"v": 1, "req": req, "type": "ping"})).await;
    }
    send(
        &mut spender,
        register_activity(1_000, Some(ACTIVITY), Some(START)),
    )
    .await;
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
    assert!(!text.contains(ACTIVITY), "{text}");
}

#[tokio::test]
async fn clear_activity_spends_the_rate_limit_like_any_command() {
    let mut env = Env::new();
    env.start().await;
    let (mut spender, _) = env.phone("Spender").await;
    send(
        &mut spender,
        register_activity(1, Some(ACTIVITY), Some(START)),
    )
    .await;
    recv_kind(&mut spender, "ack").await;
    // A few past the burst, so a refill while they are sent still leaves none.
    for req in 2..=BURST as u64 + 4 {
        send(&mut spender, json!({"v": 1, "req": req, "type": "ping"})).await;
    }
    send(&mut spender, clear_activity(1_000)).await;
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
    assert!(text.contains(ACTIVITY), "{text}");
}

#[tokio::test]
async fn a_typed_command_needs_hello_names_its_session_and_is_audited() {
    let mut env = Env::new();
    let bridge = env.bridge(5013, 0..1).await;
    env.start().await;
    let (mut read, mut write) = bridge.accept().await;
    let command = |req: u64, session: &str, name: &str| json!({"v": 1, "req": req, "type": "command", "session": session, "name": name});
    // Before hello it is refused as unpaired, and the connection closes.
    let mut stranger = env.stranger().await;
    send(&mut stranger, command(1, &bridge.id, "new")).await;
    let refusal = recv(&mut stranger).await;
    assert_eq!(
        (refusal["type"].clone(), refusal["code"].clone()),
        (json!("refused"), json!("unpaired"))
    );
    assert!(closes(&mut stranger).await);
    assert!(bridge_quiet(&mut read).await, "the bridge receives nothing");

    let (mut phone, _) = env.phone("Test phone").await;
    // An unknown session is unavailable, and an unknown name or a path is invalid.
    send(&mut phone, command(2, "nobody", "help")).await;
    assert_eq!(
        recv(&mut phone).await,
        refused(2, Code::Unavailable, "No such session")
    );
    for (req, name) in [(3, "model"), (4, "export /tmp/x")] {
        send(&mut phone, command(req, &bridge.id, name)).await;
        let refusal = recv(&mut phone).await;
        assert_eq!(refusal["code"], "invalid", "{refusal}");
    }
    assert!(bridge_quiet(&mut read).await, "the bridge receives nothing");

    // A known one reaches the bridge as a typed command, and is audited with the device.
    for (req, name) in [(5, "new"), (6, "session"), (7, "help"), (8, "export")] {
        send(&mut phone, command(req, &bridge.id, name)).await;
        let line = bridge_line(&mut read).await;
        assert_eq!(
            (
                line["type"].clone(),
                line["name"].clone(),
                line["by"].clone()
            ),
            (json!("command"), json!(name), json!("Test phone"))
        );
        let asked = line["req"].as_u64().expect("a req");
        bridge_write(&mut write, ack(asked, "done")).await;
        assert_eq!(recv_kind(&mut phone, "ack").await["message"], "done");
    }
    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let lines: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .filter(|line: &Value| line["what"] != "paired")
        .collect();
    let what: Vec<&str> = lines.iter().map(|l| l["what"].as_str().unwrap()).collect();
    assert_eq!(
        what,
        [
            "command new",
            "command session",
            "command help",
            "command export"
        ]
    );
    for line in &lines {
        assert_eq!(
            (&line["device"], &line["session"]),
            (&json!("Test phone"), &json!(bridge.id))
        );
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
    assert_eq!(what, ["paired", "push on", "push off"], "{audit}");
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

#[tokio::test]
async fn a_run_directory_that_is_a_symlink_is_refused() {
    let root = PathBuf::from(format!("/tmp/ohs{}-link", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("target");
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).expect("mode");
    std::os::unix::fs::symlink(&elsewhere, root.join("run")).expect("planted symlink");
    let serving = Hub::serve_with(
        root.clone(),
        "ws://127.0.0.1:9".to_owned(),
        Arc::new(|_: &str| {}),
        std::future::pending(),
        oneshot::channel().0,
    );
    let result = timeout(Duration::from_secs(2), serving).await;
    let socket = elsewhere.join("hub.sock").exists();
    let _ = std::fs::remove_dir_all(&root);
    let error = result
        .expect("the hub refuses to start")
        .expect_err("a refusal");
    assert!(
        error.contains(&root.join("run").display().to_string()) && error.contains("symlink"),
        "{error}"
    );
    assert!(!socket, "no socket appears where the symlink points");
}

#[tokio::test]
async fn a_loose_run_directory_is_tightened() {
    let mut env = Env::new();
    std::fs::set_permissions(&env.run, std::fs::Permissions::from_mode(0o755)).expect("mode");
    env.start().await;
    let mode = std::fs::symlink_metadata(&env.run)
        .expect("run")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

/// Appends records `seqs` to the journal `name` of `env`, each padded with `pad` bytes.
fn padded_journal(env: &Env, name: &str, seqs: std::ops::Range<u64>, pad: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env.root.join(name))
        .expect("journal");
    let filler = "x".repeat(pad);
    for seq in seqs {
        let record = json!({"format": "octet-preview-1", "seq": seq, "type": "text",
            "data": {"n": seq, "pad": filler}});
        writeln!(file, "{record}").expect("journal write");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_live_events_than_the_outbox_holds_wait_for_a_slow_phone_at_the_handover() {
    let mut env = Env::new();
    let bridge = env.bridge(5101, 0..0).await;
    // 300 records of 60 KB: the replay reads them and then waits on a phone that does not read.
    padded_journal(&env, JOURNAL, 0..300, 60_000);
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(&mut phone, subscribe(3, &bridge.id, None)).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    // 700 small records land while the replay waits: more than the 512 live frames the outbox
    // holds, so the handover cannot put them all there at once.
    env.journal(JOURNAL, 300..1000);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut seqs = Vec::new();
    let read = timeout(Duration::from_secs(30), async {
        while let Some(frame) = phone.next().await {
            match frame["type"].as_str() {
                Some("event") => seqs.push(frame["seq"].as_u64().expect("a seq")),
                Some("ack") => return Some(frame),
                _ => {}
            }
        }
        None
    })
    .await
    .expect("the phone reads in time");
    assert_eq!(
        read,
        Some(ack(3, "Subscribed")),
        "the connection was cut after {} events (last {:?})",
        seqs.len(),
        seqs.last()
    );
    assert!(seqs.iter().copied().eq(0..1000), "a gap or a repeat");
    // Still connected, and live.
    send(&mut phone, json!({"v": 1, "req": 4, "type": "ping"})).await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(4, "pong"));
    env.journal(JOURNAL, 1000..1001);
    assert_eq!(recv_kind(&mut phone, "event").await["seq"], 1000);
}

#[tokio::test]
async fn no_replayed_event_follows_the_unsubscribe_ack() {
    let mut env = Env::new();
    let bridge = env.bridge(5102, 0..0).await;
    // About 10 MB: more than the socket buffers hold, so the replay is still running when the
    // unsubscribe arrives, on Linux too.
    padded_journal(&env, JOURNAL, 0..5000, 2000);
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(&mut phone, subscribe(3, &bridge.id, None)).await;
    // The phone does not read for a while, so the replay fills its queue to the writer.
    tokio::time::sleep(Duration::from_millis(500)).await;
    send(
        &mut phone,
        json!({"v": 1, "req": 4, "type": "unsubscribe", "session": bridge.id}),
    )
    .await;
    loop {
        let frame = recv(&mut phone).await;
        if frame["type"] == "ack" {
            assert_eq!(frame, ack(4, "Unsubscribed"));
            break;
        }
    }
    let mut late = 0;
    while let Ok(frame) = timeout(Duration::from_millis(400), recv(&mut phone)).await {
        if frame["type"] == "event" {
            late += 1;
        }
    }
    assert_eq!(late, 0, "replayed events arrived after the unsubscribe ack");
    // A new subscription's frames come after its own `subscribed`, and none of the old one's.
    send(&mut phone, subscribe(5, &bridge.id, Some(4990))).await;
    assert_eq!(recv(&mut phone).await["type"], "subscribed");
    for seq in 4991..5000 {
        assert_eq!(recv(&mut phone).await["seq"], seq);
    }
    assert_eq!(recv(&mut phone).await, ack(5, "Subscribed"));
}

/// Runs git in `dir` as the person at the desk would.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A git repository at `dir` with one commit of `tracked.txt`; its own config names who
/// commits, so the hub's git needs nothing from the machine.
fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo directory");
    git_in(dir, &["init", "-q"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
    ] {
        git_in(dir, &["config", key, value]);
    }
    std::fs::write(dir.join("tracked.txt"), "one\n").expect("write");
    git_in(dir, &["add", "-A"]);
    git_in(dir, &["commit", "-q", "-m", "First"]);
}

/// The bridge's state: `phase`, with `approvals` waiting.
fn turn_state(phase: &str, approvals: u64) -> Value {
    let approvals: Vec<Value> = (1..=approvals)
        .map(|id| {
            json!({"id": id, "summary": {"action": "Run a shell command", "files": [],
            "scope": "This request only, once"}})
        })
        .collect();
    json!({"phase": phase, "queue": [], "queued": 0, "mode": "ask", "approvals": approvals})
}

/// Waits until the hub lists session `id` in `phase` with `approvals` waiting.
async fn listed_as(phone: &mut Phone, id: &str, phase: &str, approvals: u64) {
    timeout(WAIT, async {
        loop {
            let frame = recv_kind(phone, "sessions").await;
            let rows = frame["sessions"].as_array().cloned().unwrap_or_default();
            if rows.iter().any(|row| {
                row["id"] == id && row["phase"] == phase && row["approvals"] == approvals
            }) {
                return;
            }
        }
    })
    .await
    .expect("the session is listed in that phase");
}

#[tokio::test]
async fn changes_and_diffs_work_on_the_sessions_workspace() {
    let mut env = Env::new();
    let workspace = env.root.join("ws");
    git_repo(&workspace);
    std::fs::write(workspace.join("tracked.txt"), "one\ntwo\n").expect("write");
    std::fs::write(workspace.join("new.txt"), "fresh\n").expect("write");
    let bridge = env.bridge_in(5050, &workspace).await;
    env.start().await;
    let (_read, _write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let changes = |req: u64| json!({"v": 1, "req": req, "type": "changes", "session": bridge.id});
    let diff = |req: u64, path: &str| json!({"v": 1, "req": req, "type": "diff", "session": bridge.id, "path": path});

    // A diff before any listing is refused.
    send(&mut phone, diff(1, "tracked.txt")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(1, Code::Invalid, "Ask for the changes first")
    );

    send(&mut phone, changes(2)).await;
    let listed = recv_kind(&mut phone, "changes").await;
    assert_eq!(listed["req"], 2, "{listed}");
    assert_eq!(listed["more"], 0);
    let files = listed["files"].as_array().expect("files");
    let tracked = files.iter().find(|f| f["path"] == "tracked.txt");
    assert_eq!(
        tracked,
        Some(
            &json!({"path": "tracked.txt", "add": 1, "del": 0, "binary": false,
            "status": "M"})
        )
    );
    let fresh = files.iter().find(|f| f["path"] == "new.txt");
    assert_eq!(fresh.map(|f| f["status"].clone()), Some(json!("?")));

    send(&mut phone, diff(3, "tracked.txt")).await;
    let shown = recv_kind(&mut phone, "diff").await;
    assert_eq!(shown["req"], 3);
    assert_eq!(shown["path"], "tracked.txt");
    assert_eq!(shown["clipped"], false);
    assert_eq!(shown["binary"], false);
    assert!(
        shown["text"].as_str().is_some_and(|t| t.contains("+two\n")),
        "{shown}"
    );

    // A path the listing did not have is refused, and works after a fresh listing.
    std::fs::write(workspace.join("later.txt"), "later\n").expect("write");
    for path in ["later.txt", "../outside", "/etc/passwd", "--output=x"] {
        send(&mut phone, diff(4, path)).await;
        assert_eq!(
            recv_kind(&mut phone, "refused").await,
            refused(4, Code::Invalid, "Ask for the changes first"),
            "{path}"
        );
    }
    send(&mut phone, changes(5)).await;
    recv_kind(&mut phone, "changes").await;
    send(&mut phone, diff(6, "later.txt")).await;
    let shown = recv_kind(&mut phone, "diff").await;
    assert!(
        shown["text"]
            .as_str()
            .is_some_and(|t| t.contains("+later\n")),
        "{shown}"
    );

    // Another connection has a listing of its own.
    let token = env.pair("Other phone").await;
    let (mut other, _) = env.hello(&token).await;
    send(&mut other, diff(7, "later.txt")).await;
    assert_eq!(
        recv_kind(&mut other, "refused").await,
        refused(7, Code::Invalid, "Ask for the changes first")
    );
}

#[tokio::test]
async fn a_commit_waits_for_the_turn_and_is_audited_without_its_message() {
    let mut env = Env::new();
    let workspace = env.root.join("ws");
    git_repo(&workspace);
    std::fs::write(workspace.join("tracked.txt"), "one\ntwo\n").expect("write");
    let bridge = env.bridge_in(5052, &workspace).await;
    env.start().await;
    let (_read, mut write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    let commit = |req: u64, message: &str| json!({"v": 1, "req": req, "type": "commit", "session": bridge.id, "message": message});

    // No commit while the session's turn is unknown, running, or waiting on an approval.
    send(&mut phone, commit(8, "Too early")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(8, Code::NotConnected, "The session is not connected")
    );
    bridge_write(&mut write, bridge_state_frame(&turn_state("running", 0))).await;
    listed_as(&mut phone, &bridge.id, "running", 0).await;
    send(&mut phone, commit(9, "During a turn")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(9, Code::TurnOpen, "Finish or cancel the current turn first")
    );
    bridge_write(&mut write, bridge_state_frame(&turn_state("idle", 1))).await;
    listed_as(&mut phone, &bridge.id, "idle", 1).await;
    send(&mut phone, commit(10, "With an approval waiting")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(
            10,
            Code::TurnOpen,
            "Finish or cancel the current turn first"
        )
    );
    bridge_write(&mut write, bridge_state_frame(&turn_state("idle", 0))).await;
    listed_as(&mut phone, &bridge.id, "idle", 0).await;
    send(&mut phone, commit(11, "")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(11, Code::Invalid, "Write a commit message")
    );

    // A hook that refuses gives git's words; git staged first, so the audit says it failed.
    let hook = workspace.join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().expect("hooks")).expect("hooks directory");
    std::fs::write(&hook, "#!/bin/sh\necho 'tests failed' >&2\nexit 1\n").expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    send(&mut phone, commit(14, "Secret plans, refused")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(14, Code::NotAllowed, "tests failed")
    );
    std::fs::remove_file(&hook).expect("unhook");

    send(&mut phone, commit(12, "Secret plans for the phone")).await;
    let done = recv_kind(&mut phone, "ack").await;
    let hash = git_in(&workspace, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(done, ack(12, &format!("Committed {}", hash.trim())));
    assert_eq!(
        git_in(&workspace, &["log", "-1", "--format=%s"]).trim(),
        "Secret plans for the phone"
    );
    send(&mut phone, commit(13, "Again")).await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(13, Code::Invalid, "Nothing to commit")
    );

    // The audit names the device and the session, never the message.
    let audit = std::fs::read_to_string(env.root.join("hub-audit.jsonl")).expect("the audit log");
    let commits: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .filter(|line: &Value| line["what"] == "commit")
        .collect();
    assert_eq!(commits.len(), 1, "{audit}");
    assert_eq!(commits[0]["device"], "Test phone");
    assert_eq!(commits[0]["session"], bridge.id);
    // Only the refusal after git staged is a failed commit; the earlier refusals ran no git.
    let failed = audit
        .lines()
        .filter(|l| l.contains("\"commit failed\""))
        .count();
    assert_eq!(failed, 1, "{audit}");
    assert!(!audit.contains("Secret plans"), "{audit}");
}

#[tokio::test]
async fn git_on_a_folder_that_is_not_a_repository_is_refused_and_the_hub_goes_on() {
    let mut env = Env::new();
    let workspace = env.root.join("plain");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let bridge = env.bridge_in(5051, &workspace).await;
    env.start().await;
    let (_read, _write) = bridge.accept().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(
        &mut phone,
        json!({"v": 1, "req": 1, "type": "changes", "session": bridge.id}),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(1, Code::Unavailable, "This folder isn't a git repository")
    );
    send(
        &mut phone,
        json!({"v": 1, "req": 2, "type": "changes", "session": "9-9"}),
    )
    .await;
    assert_eq!(
        recv_kind(&mut phone, "refused").await,
        refused(2, Code::Unavailable, "No such session")
    );
    send(&mut phone, json!({"v": 1, "req": 3, "type": "ping"})).await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(3, "pong"));
}

/// Raises this process's soft limit on open files to `wanted` (at most the hard limit), for a
/// test that holds hundreds of sockets: a shell's default can be 256.
fn raise_open_files(wanted: u64) {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    if limit.current.is_none_or(|current| current >= wanted) {
        return;
    }
    let current = Some(limit.maximum.map_or(wanted, |most| most.min(wanted)));
    let _ = setrlimit(
        Resource::Nofile,
        Rlimit {
            current,
            maximum: limit.maximum,
        },
    );
}

/// The `sessions` frames `phone` gets until `until`, moved on by `stalls`, each with when it
/// came.
async fn sessions_until(
    phone: &mut Phone,
    until: tokio::time::Instant,
    stalls: &Stalls,
) -> Vec<(Instant, Value)> {
    let mut got = Vec::new();
    loop {
        let deadline = until + stalls.lost();
        match tokio::time::timeout_at(deadline, recv(phone)).await {
            Ok(frame) if frame["type"] == "sessions" => got.push((Instant::now(), frame)),
            Ok(_) => {}
            // A stall since moves the deadline on.
            Err(_) if until + stalls.lost() > deadline => {}
            Err(_) => return got,
        }
    }
}

/// How long the machine has kept this test from running, as a ticker every 10 ms sees it: the
/// time its ticks came late, summed. A deadline on the wall clock moved on by it holds the hub
/// to its own pace, not to a busy machine's: a hub that falls behind on its own still fails,
/// since the ticker, on another worker, keeps time meanwhile.
#[derive(Clone)]
struct Stalls(std::sync::Arc<AtomicU64>);

impl Stalls {
    /// Starts the ticker, which runs as long as the test's runtime.
    fn start() -> Self {
        let lost = std::sync::Arc::new(AtomicU64::new(0));
        let ticks = std::sync::Arc::clone(&lost);
        tokio::spawn(async move {
            let tick = Duration::from_millis(10);
            loop {
                let asleep = Instant::now();
                tokio::time::sleep(tick).await;
                let late = asleep.elapsed().saturating_sub(tick);
                ticks.fetch_add(
                    u64::try_from(late.as_micros()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            }
        });
        Self(lost)
    }

    /// The time lost so far.
    fn lost(&self) -> Duration {
        Duration::from_micros(self.0.load(Ordering::Relaxed))
    }
}

/// The most `sessions` frames a phone may get in `elapsed`: four a second, and the one a hello
/// or the first change after a quiet spell sends at once.
fn most_lists(elapsed: Duration) -> usize {
    usize::try_from(elapsed.as_millis().div_ceil(250)).unwrap_or(usize::MAX) + 2
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_storm_of_state_changes_sends_at_most_four_lists_a_second() {
    raise_open_files(4096);
    let mut env = Env::new();
    let mut bridges = Vec::new();
    for pid in 6000..6200 {
        bridges.push(env.bridge(pid, 0..1).await);
    }
    env.start().await;
    let mut links = Vec::new();
    for bridge in &bridges {
        links.push(bridge.accept().await);
    }
    let mut phones = Vec::new();
    for n in 0..4 {
        let (phone, sessions) = env.phone(&format!("Phone {n}")).await;
        assert_eq!(sessions["sessions"].as_array().map(Vec::len), Some(200));
        phones.push(phone);
    }
    // A fifth phone reads nothing while the storm lasts.
    let (mut slow, _) = env.phone("Slow phone").await;
    let (rounds, every) = (10, Duration::from_millis(200));
    let storm = every * rounds;
    // The machine may be busy with other tests: the wall-clock bounds below are moved on by
    // the time it kept this test from running.
    let stalls = Stalls::start();
    let began = Instant::now();
    let settle = tokio::time::Instant::now() + storm + Duration::from_millis(1500);
    let readers: Vec<_> = phones
        .into_iter()
        .map(|mut phone| {
            let stalls = stalls.clone();
            tokio::spawn(async move { sessions_until(&mut phone, settle, &stalls).await })
        })
        .collect();
    // Every session changes its NOW line five times a second: 1,000 state frames a second. The
    // first one's approval comes halfway, and stays.
    let state = |round: u32, approval: bool| {
        let approvals = if approval {
            json!([{"id": 1, "summary": {"action": "Run Bash", "command": "ls", "files": [],
                "scope": "this call"}}])
        } else {
            json!([])
        };
        json!({"phase": "running", "queue": [], "queued": 0, "mode": "ask",
            "now": format!("Step {round}"), "approvals": approvals})
    };
    let mut asked = None;
    for round in 0..rounds {
        for (i, (_read, write)) in links.iter_mut().enumerate() {
            let approval = i == 0 && round >= rounds / 2;
            bridge_write(write, bridge_state_frame(&state(round, approval))).await;
        }
        if round == rounds / 2 {
            asked = Some((Instant::now(), stalls.lost()));
        }
        tokio::time::sleep(every).await;
    }
    let (asked, lost_by_then) = asked.expect("the approval was sent");
    let last = format!("Step {}", rounds - 1);
    for reader in readers {
        let got = reader.await.expect("the phone's reader");
        let elapsed = began.elapsed();
        assert!(
            got.len() <= most_lists(elapsed),
            "{} lists in {elapsed:?}",
            got.len()
        );
        // The lists keep up: the approval shows within the coalescing window and some, and the
        // last list is the storm's end.
        let shown = got
            .iter()
            .find(|(_, frame)| frame["sessions"][0]["approvals"] == 1)
            .map(|(at, _)| at.saturating_duration_since(asked))
            .expect("the approval is listed");
        let stalled = stalls.lost().saturating_sub(lost_by_then);
        assert!(
            shown < Duration::from_secs(1) + stalled,
            "the approval took {shown:?}, {stalled:?} of it stalled"
        );
        let (_, final_list) = got.last().expect("lists came");
        let rows = final_list["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 200);
        assert!(
            rows.iter().all(|row| row["now"] == last.as_str()),
            "stale rows"
        );
    }
    // The phone that read nothing was kept, with at most one list waiting in its queue: it now
    // gets a handful, the newest last, not one per change.
    let mut lists = 0;
    loop {
        let frame = recv(&mut slow).await;
        if frame["type"] != "sessions" {
            continue;
        }
        lists += 1;
        if frame["sessions"][199]["now"] == last.as_str() {
            break;
        }
    }
    assert!(lists <= most_lists(began.elapsed()), "{lists} lists");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_devices_write_never_stalls_the_hub() {
    let mut env = Env::new();
    env.start().await;
    let (mut writer, _) = env.phone("Writer phone").await;
    let (mut other, _) = env.phone("Other phone").await;
    // Another process (an `octet hub revoke`, say) holds the devices file's lock.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(env.root.join("devices.lock"))
        .expect("the lock file");
    lock.lock().expect("the lock");
    send(&mut writer, register(7, Some(PUSH))).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The hub goes on meanwhile: another phone's ping is answered at once.
    let began = Instant::now();
    send(&mut other, json!({"v": 1, "req": 3, "type": "ping"})).await;
    assert_eq!(recv(&mut other).await, ack(3, "pong"));
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    // The registration is acked only once it is saved, after the lock is let go.
    let early = timeout(Duration::from_millis(300), recv(&mut writer)).await;
    assert!(early.is_err(), "acked before it was saved: {early:?}");
    drop(lock);
    let acked = recv_kind(&mut writer, "ack").await;
    assert_eq!(
        (acked["req"].as_u64(), acked["message"].as_str()),
        (Some(7), Some("Notifications on"))
    );
    let text = std::fs::read_to_string(env.root.join("devices.json")).expect("file");
    assert!(text.contains(PUSH), "{text}");
}

#[tokio::test]
async fn a_phone_that_says_hello_mid_window_is_not_left_with_a_passing_list() {
    let mut env = Env::new();
    let bridge = env.bridge(5060, 0..1).await;
    env.start().await;
    let (_read, mut write) = bridge.accept().await;
    let token = env.pair("Test phone").await;
    let state = |phase: &str| json!({"phase": phase, "queue": [], "queued": 0, "mode": "ask", "approvals": []});
    // A quiet spell, then `running` goes out at once and opens a quarter-second window.
    tokio::time::sleep(Duration::from_millis(400)).await;
    bridge_write(&mut write, bridge_state_frame(&state("running"))).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Within the window the session goes idle, and a phone says hello: its list shows `idle`.
    bridge_write(&mut write, bridge_state_frame(&state("idle"))).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let (mut phone, sessions) = env.hello(&token).await;
    assert_eq!(sessions["sessions"][0]["phase"], "idle", "{sessions}");
    // Then, still within the window, it runs again: the list the others last got. The phone,
    // which last got `idle`, must hear of it.
    bridge_write(&mut write, bridge_state_frame(&state("running"))).await;
    listed_as(&mut phone, &bridge.id, "running", 0).await;
}

#[tokio::test]
async fn the_hub_registers_again_after_the_relay_restarts_and_a_phone_resumes_from_its_cursor() {
    let mut env = Env::new();
    let bridge = env.bridge(5201, 0..5).await;
    env.start().await;
    let key = env.pair("Test phone").await;
    let (mut phone, _) = env.hello(&key).await;
    send(&mut phone, subscribe(3, &bridge.id, Some(2))).await;
    assert_eq!(recv(&mut phone).await["type"], "subscribed");
    let mut cursor = 2;
    loop {
        let frame = recv(&mut phone).await;
        if frame == ack(3, "Subscribed") {
            break;
        }
        cursor = frame["seq"].as_u64().expect("a seq");
    }
    assert_eq!(cursor, 4);
    // The relay goes away: the phone's channel goes with it, and the hub dials again.
    let relay = env.relay.take().expect("a relay");
    let addr = relay.addr();
    relay.stop().await;
    assert!(closes(&mut phone).await);
    relay_state(&env.run, "connecting").await;
    env.journal(JOURNAL, 5..7);
    let relay = timeout(WAIT, async {
        loop {
            if let Some(relay) = Relay::start_at(addr, octet_testkit::relay::roomy()).await {
                return relay;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the relay's port is free again");
    env.relay = Some(relay);
    // Back on its backoff, the hub has its channel again; the pairing held.
    relay_state(&env.run, "registered").await;
    assert_eq!(env.control(json!({"type": "status"})).await["devices"], 1);
    let (mut again, _) = env.hello(&key).await;
    send(&mut again, subscribe(4, &bridge.id, Some(cursor))).await;
    assert_eq!(recv(&mut again).await["from"], "cursor");
    for seq in [5, 6] {
        assert_eq!(recv(&mut again).await["seq"], seq);
    }
    assert_eq!(recv(&mut again).await, ack(4, "Subscribed"));
}

#[tokio::test]
#[expect(
    non_snake_case,
    reason = "the size is named as the plan names it, in KiB"
)]
async fn an_event_of_200_KiB_reaches_the_phone_whole() {
    let mut env = Env::new();
    let bridge = env.bridge(5202, 0..1).await;
    env.start().await;
    let (mut phone, _) = env.phone("Test phone").await;
    send(&mut phone, subscribe(2, &bridge.id, Some(0))).await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(2, "Subscribed"));
    // Four chunks on the wire: three of 65,000 bytes and the rest.
    let text: String = (0..200 * 1024)
        .map(|n| char::from(b'a' + u8::try_from(n % 26).expect("a letter")))
        .collect();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(env.root.join(JOURNAL))
        .expect("journal");
    let record = json!({"format": "octet-preview-1", "seq": 1, "type": "text", "data": text});
    writeln!(file, "{record}").expect("journal write");
    let event = recv_kind(&mut phone, "event").await;
    assert_eq!(event["seq"], 1);
    assert_eq!(
        event["event"]["data"],
        json!(text),
        "the text arrives whole"
    );
    // The channel goes on.
    send(&mut phone, json!({"v": 1, "req": 3, "type": "ping"})).await;
    assert_eq!(recv_kind(&mut phone, "ack").await, ack(3, "pong"));
}
