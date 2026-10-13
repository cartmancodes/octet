//! A phone drives a live terminal interface through the real hub: the `octet hub` process, an
//! `octet` interface on a PTY against the fake Codex, and a WebSocket client on loopback.
#![cfg(unix)]
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    fs::File,
    future::Future,
    io::Read,
    os::{
        fd::FromRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixStream},
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

/// The tailnet login the hub is told is its owner.
const OWNER: &str = "test@example";
/// How long any one step may take before the test fails.
const WAIT: Duration = Duration::from_secs(10);

type Phone = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Everything the test starts, and what it needs to say why a step failed.
struct World {
    /// The hub's and the interface's shared data directory, short for the socket path limit.
    dir: PathBuf,
    /// Where the hub process writes its output.
    hub_log: PathBuf,
    /// The directory holding a fake `tailscale` that shows the hub as published.
    bin: PathBuf,
    port: u16,
    hub: Child,
    tui: Child,
    /// Everything the interface has written to its terminal so far.
    screen: Arc<Mutex<Vec<u8>>>,
    /// The terminal's far end, kept open so the interface's reads and writes work.
    _master: File,
    _slave: File,
}

impl World {
    /// Starts the hub on a free port, then the interface, and waits for it to register.
    async fn start() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = format!(
            "oe{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = PathBuf::from("/tmp").join(&id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("data directory");
        let hub_log = PathBuf::from("/tmp").join(format!("{id}.hub.log"));
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
            probe.local_addr().expect("an address").port()
        };
        let log = File::create(&hub_log).expect("hub log");
        // The hub only pairs phones once `tailscale serve` publishes it, so a fake `tailscale`
        // answers `serve status --json` with the hub's port proxied at a tailnet name.
        let bin = PathBuf::from("/tmp").join(format!("{id}.bin"));
        std::fs::create_dir_all(&bin).expect("fake tailscale directory");
        let served = json!({
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"mac.tail1234.ts.net:443": {"Handlers": {"/": {
                "Proxy": format!("http://127.0.0.1:{port}")}}}},
        });
        let script = bin.join("tailscale");
        std::fs::write(&script, format!("#!/bin/sh\necho '{served}'\n")).expect("fake tailscale");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let hub = Command::new(env!("CARGO_BIN_EXE_octet"))
            .env("PATH", path)
            .args(["hub", "--journal-dir"])
            .arg(&dir)
            .args(["--port", &port.to_string(), "--owner", OWNER])
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log handle"))
            .stderr(log)
            .spawn()
            .expect("the hub starts");
        let (tui, screen, master, slave) = spawn_interface(&dir);
        let mut world = Self {
            dir,
            hub_log,
            bin,
            port,
            hub,
            tui,
            screen,
            _master: master,
            _slave: slave,
        };
        world.hub_answers().await;
        world
            .within("the interface registers", async {
                while !std::fs::read_dir(world.dir.join("run")).is_ok_and(|mut entries| {
                    entries
                        .any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json")))
                }) {
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
        world
    }

    /// Waits for the hub to answer on `hub.sock`, failing at once if its process has exited.
    async fn hub_answers(&mut self) {
        let limit = tokio::time::Instant::now() + WAIT;
        while self.control(&json!({"type": "status"})).await.is_none() {
            if let Some(status) = self.hub.try_wait().expect("wait") {
                panic!("the hub exited early ({status})\n{}", self.evidence());
            }
            assert!(
                tokio::time::Instant::now() < limit,
                "timed out: the hub answers on hub.sock\n{}",
                self.evidence()
            );
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until no frame arrives for a moment, for at most [`WAIT`].
    async fn settle(&self, phone: &mut Phone) {
        let limit = tokio::time::Instant::now() + WAIT;
        while !self.quiet(phone).await {
            assert!(
                tokio::time::Instant::now() < limit,
                "frames kept arriving\n{}",
                self.evidence()
            );
        }
    }

    /// What the interface drew and what the hub said, for a failure message.
    fn evidence(&self) -> String {
        let screen = String::from_utf8_lossy(&self.screen.lock().expect("screen")).into_owned();
        let tail: String = screen
            .chars()
            .rev()
            .take(3000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let hub = std::fs::read_to_string(&self.hub_log).unwrap_or_default();
        format!("--- hub output ---\n{hub}\n--- interface output (tail) ---\n{tail:?}")
    }

    /// `future`, or a panic naming `what` and carrying the evidence if it takes too long.
    async fn within<T>(&self, what: &str, future: impl Future<Output = T>) -> T {
        if let Ok(done) = timeout(WAIT, future).await {
            done
        } else {
            panic!("timed out: {what}\n{}", self.evidence());
        }
    }

    /// One request on the hub's control socket, and its answer; `None` while it is not up.
    async fn control(&self, request: &Value) -> Option<Value> {
        let stream = UnixStream::connect(self.dir.join("run/hub.sock"))
            .await
            .ok()?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.ok()?;
        serde_json::from_str(&line).ok()
    }

    /// A WebSocket connection carrying the owner's tailnet login.
    async fn connect(&self) -> Phone {
        let mut request = format!("ws://127.0.0.1:{}/", self.port)
            .into_client_request()
            .expect("a request");
        request
            .headers_mut()
            .insert("Tailscale-User-Login", OWNER.parse().expect("a header"));
        let (phone, _) = self
            .within("the WebSocket handshake", Box::pin(connect_async(request)))
            .await
            .expect("the handshake succeeds");
        phone
    }

    async fn send(&self, phone: &mut Phone, message: Value) {
        self.within(
            "sending a frame",
            phone.send(Message::text(message.to_string())),
        )
        .await
        .expect("send");
    }

    /// The next text frame, parsed.
    async fn recv(&self, phone: &mut Phone) -> Value {
        self.within("a frame from the hub", async {
            loop {
                match phone.next().await {
                    Some(Ok(Message::Text(text))) => {
                        return serde_json::from_str(text.as_str()).expect("a JSON frame");
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    other => panic!("expected a frame, got {other:?}\n{}", self.evidence()),
                }
            }
        })
        .await
    }

    /// Whether no frame arrives for a while.
    async fn quiet(&self, phone: &mut Phone) -> bool {
        timeout(Duration::from_millis(700), self.recv(phone))
            .await
            .is_err()
    }

    /// The first journal event that arrives within a moment, passing over other frames (`state`,
    /// `sessions`); `None` if there is none.
    async fn no_events(&self, phone: &mut Phone) -> Option<Value> {
        let watch = async {
            loop {
                let frame = self.recv(phone).await;
                if frame["type"] == "event" {
                    return frame;
                }
            }
        };
        timeout(Duration::from_millis(700), watch).await.ok()
    }

    /// Frames until `done` accepts one, which is returned last; events passing by are recorded.
    async fn until(
        &self,
        phone: &mut Phone,
        seen: &mut Events,
        done: impl Fn(&Value) -> bool,
    ) -> Value {
        loop {
            let frame = self.recv(phone).await;
            seen.note(&frame);
            if done(&frame) {
                return frame;
            }
        }
    }

    /// A phone that has paired (once) and said hello, and the `sessions` frame it got.
    async fn hello(&self, token: &str) -> (Phone, Value) {
        let mut phone = self.connect().await;
        self.send(
            &mut phone,
            json!({"v": 1, "req": 2, "type": "hello", "token": token}),
        )
        .await;
        let welcome = self.recv(&mut phone).await;
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        let sessions = self.recv(&mut phone).await;
        assert_eq!(sessions["type"], "sessions", "{sessions}");
        (phone, sessions)
    }

    /// Pairs a new device and returns its token.
    async fn pair(&self) -> String {
        let secret = self
            .within("the pairing secret", async {
                self.control(&json!({"type": "pair_secret"})).await
            })
            .await
            .expect("a control answer");
        let secret = secret["secret"].as_str().expect("a secret");
        let mut phone = self.connect().await;
        self.send(
            &mut phone,
            json!({"v": 1, "req": 1, "type": "pair", "secret": secret, "name": "Test phone",
                "platform": "ios"}),
        )
        .await;
        let paired = self.recv(&mut phone).await;
        assert_eq!(paired["type"], "paired", "{paired}");
        paired["token"].as_str().expect("a token").to_owned()
    }

    /// Waits for the interface process to exit.
    async fn interface_exits(&mut self) {
        let limit = tokio::time::Instant::now() + WAIT;
        while self.tui.try_wait().expect("wait").is_none() {
            assert!(
                tokio::time::Instant::now() < limit,
                "the interface did not exit after `end`\n{}",
                self.evidence()
            );
            sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.tui.kill();
        let _ = self.tui.wait();
        let _ = self.hub.kill();
        let _ = self.hub.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.hub_log);
        let _ = std::fs::remove_dir_all(&self.bin);
    }
}

/// The journal events a phone has been sent, by cursor.
#[derive(Default)]
struct Events {
    /// `(journal, seq)` of every event frame, in arrival order.
    cursors: Vec<(String, u64)>,
    /// `(type, data)` of every event frame, in arrival order.
    kinds: Vec<(String, Value)>,
}

impl Events {
    /// Records `frame` if it is an event, failing on a cursor seen before.
    fn note(&mut self, frame: &Value) {
        if frame["type"] != "event" {
            return;
        }
        let cursor = (
            frame["journal"].as_str().expect("a journal").to_owned(),
            frame["seq"].as_u64().expect("a seq"),
        );
        assert!(!self.cursors.contains(&cursor), "{cursor:?} arrived twice");
        self.cursors.push(cursor);
        self.kinds.push((
            frame["event"]["type"].as_str().expect("a type").to_owned(),
            frame["event"]["data"].clone(),
        ));
    }

    /// The last event's cursor, as the `after` of a `subscribe`.
    fn last(&self) -> Value {
        let (journal, seq) = self.cursors.last().expect("at least one event");
        json!({"journal": journal, "seq": seq})
    }

    /// The position of the first event of `kind`.
    fn index(&self, kind: &str) -> Option<usize> {
        self.kinds.iter().position(|(k, _)| k == kind)
    }
}

/// An `octet` interface on a PTY (`--engine codex` against the fake vendor), the output reader's
/// shared buffer and the terminal's two ends.
fn spawn_interface(dir: &Path) -> (Child, Arc<Mutex<Vec<u8>>>, File, File) {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 36,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        // SAFETY: openpty writes two descriptors into the locals above; a null name and termios
        // are allowed, and `size` lives through the call.
        unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(size),
            )
        },
        0
    );
    // SAFETY: openpty succeeded, so `master` is an open descriptor owned by nothing else.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: likewise `slave`.
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_octet"));
    command
        .args(["--engine", "codex", "--binary"])
        .arg(octet_testkit::protocol_child())
        .arg("--journal-dir")
        .arg(dir)
        .arg("--cwd")
        .arg(dir)
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .stdin(Stdio::from(slave.try_clone().expect("slave handle")))
        .stdout(Stdio::from(slave.try_clone().expect("slave handle")))
        .stderr(Stdio::from(slave.try_clone().expect("slave handle")));
    // SAFETY: the closure runs in the child before exec and calls only async-signal-safe
    // functions (setsid, ioctl).
    unsafe {
        command.pre_exec(|| {
            #[cfg_attr(
                target_os = "linux",
                expect(
                    clippy::useless_conversion,
                    reason = "TIOCSCTTY already has ioctl's request type on Linux; \
                              macOS needs the widening from u32"
                )
            )]
            let request = libc::TIOCSCTTY.into();
            if libc::setsid() == -1 || libc::ioctl(0, request, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("the interface starts");
    // A reader thread keeps the terminal drained, so the interface never blocks on a full buffer.
    let screen = Arc::new(Mutex::new(Vec::new()));
    let mut reader = master.try_clone().expect("master handle");
    let sink = Arc::clone(&screen);
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(n) = reader.read(&mut bytes) {
            if n == 0 {
                break;
            }
            sink.lock().expect("screen").extend_from_slice(&bytes[..n]);
        }
    });
    (child, screen, master, slave)
}

/// A paired phone looking at one session, and what it has been sent.
struct Drive<'a> {
    world: &'a World,
    token: String,
    phone: Phone,
    session: String,
    seen: Events,
}

impl<'a> Drive<'a> {
    /// Pairs a phone, waits for the hub to list the interface, and returns the phone watching it.
    async fn start(world: &'a World) -> Self {
        let token = world.pair().await;
        let (phone, sessions) = world
            .within("the hub lists the interface", async {
                loop {
                    let (phone, sessions) = world.hello(&token).await;
                    if sessions["sessions"]
                        .as_array()
                        .is_some_and(|r| !r.is_empty())
                    {
                        return (phone, sessions);
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
        let rows = sessions["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 1, "{sessions}");
        assert_eq!(rows[0]["engine"], "codex", "{sessions}");
        let session = rows[0]["id"].as_str().expect("an id").to_owned();
        Self {
            world,
            token,
            phone,
            session,
            seen: Events::default(),
        }
    }

    /// Sends command `kind` as request `req`, with the session and `fields` added.
    async fn command(&mut self, req: u64, kind: &str, fields: Value) {
        let mut command = json!({"v": 1, "req": req, "type": kind, "session": self.session});
        command
            .as_object_mut()
            .expect("an object")
            .extend(fields.as_object().expect("fields").clone());
        self.world.send(&mut self.phone, command).await;
    }

    /// Frames until the reply (`ack` or `refused`) to request `req`, which is returned.
    async fn reply(&mut self, req: u64) -> Value {
        let (world, seen) = (self.world, &mut self.seen);
        world
            .until(&mut self.phone, seen, |f| {
                matches!(f["type"].as_str(), Some("ack" | "refused")) && f["req"] == req
            })
            .await
    }

    /// Frames until a journal event of `kind`, which is returned.
    async fn event(&mut self, kind: &str) -> Value {
        let (world, seen) = (self.world, &mut self.seen);
        world
            .until(&mut self.phone, seen, |f| {
                f["type"] == "event" && f["event"]["type"] == kind
            })
            .await
    }

    /// Like [`event`](Self::event) for an event of `kind` whose data is `data`; one already
    /// received (it may come before the ack) counts.
    async fn event_with(&mut self, kind: &str, data: &str) {
        let wanted = json!(data);
        if self
            .seen
            .kinds
            .iter()
            .any(|(k, d)| k == kind && *d == wanted)
        {
            return;
        }
        let (world, seen) = (self.world, &mut self.seen);
        world
            .until(&mut self.phone, seen, |f| {
                f["type"] == "event" && f["event"]["type"] == kind && f["event"]["data"] == wanted
            })
            .await;
    }

    /// Drops the connection and says hello again with the same token.
    async fn reconnect(&mut self) {
        let (phone, _) = self.world.hello(&self.token).await;
        self.phone = phone;
    }
}

#[tokio::test]
async fn a_phone_drives_a_live_interface_through_the_hub() {
    let mut world = World::start().await;
    let mut phone = Drive::start(&world).await;

    // Subscribing from the start replays the journal and ends with an ack.
    phone.command(3, "subscribe", json!({"after": null})).await;
    assert_eq!(phone.reply(3).await["type"], "ack");

    // A prompt that makes the fake Codex ask for approval; allowing it ends the turn.
    phone
        .command(4, "prompt", json!({"text": "approval"}))
        .await;
    let id = phone.event("approval").await["event"]["data"]["id"]
        .as_u64()
        .expect("an approval id");
    phone
        .command(5, "answer", json!({"approval": id, "allow": true}))
        .await;
    phone.event("finished").await;
    let (closed, finished) = (
        phone.seen.index("approval_closed"),
        phone.seen.index("finished"),
    );
    assert!(
        closed.is_some_and(|c| c < finished.expect("finished")),
        "{:?}",
        phone.seen.kinds
    );

    // A mode change is acknowledged and then recorded.
    phone
        .command(6, "set_mode", json!({"mode": "accept-edits"}))
        .await;
    let ack = phone.reply(6).await;
    assert!(
        ack["message"]
            .as_str()
            .is_some_and(|m| m.contains("accept-edits")),
        "{ack}"
    );
    phone.event_with("mode", "accept-edits").await;
    world.settle(&mut phone.phone).await;

    // A phone that drops and returns from its last cursor gets nothing twice.
    let after = phone.seen.last();
    phone.reconnect().await;
    phone.command(7, "subscribe", json!({"after": after})).await;
    let before = phone.seen.cursors.len();
    assert_eq!(phone.reply(7).await["type"], "ack");
    assert_eq!(
        phone.seen.cursors.len(),
        before,
        "events repeated after the reconnect"
    );
    assert!(
        world.no_events(&mut phone.phone).await.is_none(),
        "an event arrived after the ack"
    );

    // `end` is acknowledged to the phone, which is only possible while the interface is still up;
    // then the interface exits and the session leaves the list.
    phone.command(8, "end", json!({})).await;
    assert_eq!(phone.reply(8).await["type"], "ack");
    let token = phone.token.clone();
    drop(phone);
    world.interface_exits().await;
    world
        .within("the session leaves the list", async {
            loop {
                let (_phone, sessions) = world.hello(&token).await;
                if sessions["sessions"].as_array().is_some_and(Vec::is_empty) {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
}
