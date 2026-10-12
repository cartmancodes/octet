//! The bridge: shares this terminal UI's session with a hub over a private Unix socket. It lives
//! across reconnects, keeps its registration in the run directory up to date, and sends `state`
//! and `catalog` frames to every connected hub. It never makes the interface wait: each hub has
//! a bounded queue, and one that falls behind is disconnected. A bridge that cannot start or
//! breaks turns itself off with one note; the interface goes on.
use crate::app::{App, ConnPhase};
use octet_core::remote::{
    protocol::{self, BridgeCommand, MAX_MESSAGE},
    run::{Registration, run_dir},
};
use octet_core::{Config, Mode};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    task::{AbortHandle, JoinHandle},
};

/// Lines waiting to be written to one hub before it counts as too slow.
const PEER_QUEUE: usize = 256;
/// Commands read from hubs and not yet run by the interface.
const COMMAND_QUEUE: usize = 64;
/// How long one write to a hub may take before the hub is let go.
const WRITE_STALL: Duration = Duration::from_secs(30);
/// The most a reader takes for one line: a whole message, its newline, and one byte over.
const LINE_LIMIT: u64 = MAX_MESSAGE as u64 + 2;
/// The longest a queued prompt's first line is in a `state` frame, in characters.
const QUEUE_LINE: usize = 200;

/// A command from a hub, for the interface to run.
#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the interface only refuses commands until it runs them"
    )
)]
pub(crate) struct Remote {
    /// The hub's id for the command, echoed in the reply.
    pub(crate) req: u64,
    /// The device the command came from.
    pub(crate) by: String,
    /// What it asks.
    pub(crate) command: BridgeCommand,
}

/// One connected hub: the queue its writer drains, and its tasks.
struct Peer {
    lines: mpsc::Sender<String>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Peer {
    /// Starts the tasks that read commands from `stream` and write the queued lines to it.
    fn spawn(stream: UnixStream, commands: mpsc::Sender<Remote>) -> Self {
        let (read, write) = stream.into_split();
        let (lines, queue) = mpsc::channel(PEER_QUEUE);
        let writer = tokio::spawn(write_peer(write, queue)).abort_handle();
        let reader = tokio::spawn(read_peer(read, lines.clone(), commands)).abort_handle();
        Self {
            lines,
            reader,
            writer,
        }
    }
}

impl Drop for Peer {
    /// A hub that is let go sends no more commands. Its writer ends by itself once the lines
    /// still queued are written, which closes the connection.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Writes queued lines until the queue closes, the hub goes, or a write stalls.
async fn write_peer(mut write: OwnedWriteHalf, mut queue: mpsc::Receiver<String>) {
    while let Some(line) = queue.recv().await {
        let written = tokio::time::timeout(WRITE_STALL, write.write_all(line.as_bytes())).await;
        if !matches!(written, Ok(Ok(()))) {
            break;
        }
    }
}

/// Reads a hub's command lines. A bad line is refused to that hub alone; a good one goes to the
/// interface. Ends when the hub goes or sends a message over the limit.
async fn read_peer(
    read: OwnedReadHalf,
    replies: mpsc::Sender<String>,
    commands: mpsc::Sender<Remote>,
) {
    let refuse = |req: u64, reason: &str| {
        let _ = replies.try_send(frame_line(&protocol::refused(req, reason)));
    };
    let mut reader = BufReader::new(read);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match (&mut reader)
            .take(LINE_LIMIT)
            .read_until(b'\n', &mut buffer)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buffer.last() != Some(&b'\n') && buffer.len() > MAX_MESSAGE {
            refuse(
                0,
                &format!("Message too large; the limit is {MAX_MESSAGE} bytes"),
            );
            return;
        }
        let Ok(text) = std::str::from_utf8(&buffer) else {
            refuse(0, "Message must be UTF-8 text");
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match protocol::parse_bridge_command(text) {
            Ok((req, by, command)) => {
                if commands.send(Remote { req, by, command }).await.is_err() {
                    return;
                }
            }
            Err(reason) => refuse(protocol::req_of(text), &reason),
        }
    }
}

/// Hands each new connection to the bridge.
async fn accept(
    listener: UnixListener,
    arrivals: mpsc::Sender<Peer>,
    commands: mpsc::Sender<Remote>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                if arrivals
                    .send(Peer::spawn(stream, commands.clone()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Out of descriptors, say: try again shortly rather than spin.
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// `frame` as one line.
fn frame_line(frame: &Value) -> String {
    format!("{frame}\n")
}

/// A running bridge.
struct Live {
    run: PathBuf,
    socket: PathBuf,
    /// What `<pid>.json` holds. Only the bridge writes it, and only from its own methods, one
    /// at a time.
    registration: Registration,
    peers: Vec<Peer>,
    arrivals: mpsc::Receiver<Peer>,
    commands: mpsc::Receiver<Remote>,
    accepting: JoinHandle<()>,
    /// The last `state` and `catalog` lines sent, which a new hub gets at once.
    state: Option<String>,
    catalog: Option<String>,
}

impl Live {
    /// Makes the run directory and listens on `<run>/<pid>.sock`.
    async fn open(directory: &Path) -> std::io::Result<Self> {
        let run = run_dir(directory);
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&run)
            .await?;
        // A directory that was there already may be looser.
        tokio::fs::set_permissions(&run, PermissionsExt::from_mode(0o700)).await?;
        let pid = std::process::id();
        let socket = run.join(format!("{pid}.sock"));
        // Only a dead process of ours could have left it, since the pid is ours.
        match tokio::fs::remove_file(&socket).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(&socket)?;
        if let Err(e) = tokio::fs::set_permissions(&socket, PermissionsExt::from_mode(0o600)).await
        {
            let _ = tokio::fs::remove_file(&socket).await;
            return Err(e);
        }
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let (arrivals_in, arrivals) = mpsc::channel(16);
        let (commands_in, commands) = mpsc::channel(COMMAND_QUEUE);
        Ok(Self {
            registration: Registration {
                id: format!("{pid}-{started}"),
                pid,
                started,
                socket: socket.clone(),
                engine: String::new(),
                workspace: PathBuf::new(),
                model: String::new(),
                mode: String::new(),
                vendor_session: String::new(),
                journals: Vec::new(),
                from_phone: false,
            },
            run,
            socket,
            peers: Vec::new(),
            arrivals,
            commands,
            accepting: tokio::spawn(accept(listener, arrivals_in, commands_in)),
            state: None,
            catalog: None,
        })
    }

    /// Starts sending to a new hub, beginning with the last state and catalog.
    fn adopt(&mut self, peer: Peer) {
        let first = self.state.iter().chain(self.catalog.iter());
        if first
            .into_iter()
            .all(|line| peer.lines.try_send(line.clone()).is_ok())
        {
            self.peers.push(peer);
        }
    }

    /// Queues `line` for every hub; one whose queue is full or closed is dropped.
    fn broadcast(&mut self, line: &str) {
        self.peers
            .retain(|peer| peer.lines.try_send(line.to_owned()).is_ok());
    }

    /// Hubs that connected since the last look.
    fn adopt_arrivals(&mut self) {
        while let Ok(peer) = self.arrivals.try_recv() {
            self.adopt(peer);
        }
    }

    /// Stops listening and removes the socket and the registration.
    async fn shutdown(self) {
        self.accepting.abort();
        for peer in &self.peers {
            peer.writer.abort();
        }
        let _ = tokio::fs::remove_file(&self.socket).await;
        let pid = self.registration.pid;
        let _ = tokio::fs::remove_file(self.run.join(format!("{pid}.json.tmp"))).await;
        self.registration.remove(&self.run).await;
    }
}

/// This terminal UI's bridge; does nothing when remote control is off or could not start.
pub(crate) struct Bridge {
    live: Option<Live>,
    /// Why the bridge is off, until the interface shows it.
    note: Option<String>,
}

impl Bridge {
    /// Listens for hubs under `directory`, unless `enabled` is false. A failure leaves a bridge
    /// that is off, with a note saying why.
    pub(crate) async fn start(directory: &Path, enabled: bool) -> Bridge {
        if !enabled {
            return Bridge {
                live: None,
                note: None,
            };
        }
        match Live::open(directory).await {
            Ok(live) => Bridge {
                live: Some(live),
                note: None,
            },
            Err(error) => Bridge {
                live: None,
                note: Some(format!("Remote control is off: {error}")),
            },
        }
    }

    /// The note for the interface to show, once.
    pub(crate) fn take_note(&mut self) -> Option<String> {
        self.note.take()
    }

    /// The interface connected a session journaling to `journal`: registers it, and sends the
    /// state.
    pub(crate) async fn connected(&mut self, app: &App, config: &Config, journal: &Path) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let registration = &mut live.registration;
        if !registration.journals.iter().any(|known| known == journal) {
            registration.journals.push(journal.to_path_buf());
        }
        registration.workspace.clone_from(&config.cwd);
        describe(registration, app);
        self.write_registration().await;
        self.publish(app);
    }

    /// Rewrites the registration when the model, mode or vendor session changed.
    pub(crate) async fn refresh(&mut self, app: &App) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let known = &live.registration;
        if known.engine == app.conn.engine.as_str()
            && known.model == app.conn.model
            && known.mode == app.conn.mode.label()
            && known.vendor_session == app.conn.session
        {
            return;
        }
        describe(&mut live.registration, app);
        self.write_registration().await;
    }

    /// Sends the state and the catalog to every hub, each only if it changed.
    pub(crate) fn publish(&mut self, app: &App) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        live.adopt_arrivals();
        let state = frame_line(&protocol::bridge_state_frame(&state_json(app)));
        if live.state.as_ref() != Some(&state) {
            live.broadcast(&state);
            live.state = Some(state);
        }
        let catalog = frame_line(&protocol::bridge_catalog_frame(&catalog_json(app)));
        if live.catalog.as_ref() != Some(&catalog) {
            live.broadcast(&catalog);
            live.catalog = Some(catalog);
        }
    }

    /// The next command from a hub. Never ready while the bridge is off. Cancel-safe.
    pub(crate) async fn next_command(&mut self) -> Remote {
        let Some(live) = self.live.as_mut() else {
            return std::future::pending().await;
        };
        loop {
            tokio::select! {
                Some(peer) = live.arrivals.recv() => live.adopt(peer),
                Some(remote) = live.commands.recv() => return remote,
                else => return std::future::pending().await,
            }
        }
    }

    /// Tells the hubs how command `req` went: `Ok` carries what was done, `Err` the reason.
    pub(crate) fn reply(&mut self, req: u64, result: Result<String, String>) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let frame = match result {
            Ok(message) => protocol::ack(req, &message),
            Err(reason) => protocol::refused(req, &reason),
        };
        live.broadcast(&frame_line(&frame));
    }

    /// Closes the socket and removes it and the registration.
    pub(crate) async fn stop(self) {
        if let Some(live) = self.live {
            live.shutdown().await;
        }
    }

    /// Writes the registration. A failure turns the bridge off.
    async fn write_registration(&mut self) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        if let Err(error) = live.registration.write(&live.run).await {
            if let Some(live) = self.live.take() {
                live.shutdown().await;
            }
            self.note = Some(format!("Remote control is off: {error}"));
        }
    }
}

/// Fills in what the registration says about the session.
fn describe(registration: &mut Registration, app: &App) {
    app.conn
        .engine
        .as_str()
        .clone_into(&mut registration.engine);
    registration.model.clone_from(&app.conn.model);
    app.conn.mode.label().clone_into(&mut registration.mode);
    registration.vendor_session.clone_from(&app.conn.session);
}

/// The `StateInfo` of `app`: what only the interface knows.
pub(crate) fn state_json(app: &App) -> Value {
    let phase = match app.conn.phase {
        ConnPhase::Connecting => "connecting",
        ConnPhase::Idle => "idle",
        ConnPhase::Running { .. } => "running",
        ConnPhase::Stopped => "stopped",
    };
    let queue: Vec<String> = app
        .composer
        .queue
        .iter()
        .map(|prompt| {
            let first = prompt.display().lines().next().unwrap_or_default();
            first.chars().take(QUEUE_LINE).collect()
        })
        .collect();
    let approvals: Vec<Value> = app
        .overlay
        .approvals
        .iter()
        .map(|asked| {
            json!({"id": asked.id, "summary": {
                "action": asked.summary.action, "command": asked.summary.command,
                "files": asked.summary.files, "cwd": asked.summary.cwd,
                "scope": asked.summary.scope}})
        })
        .collect();
    let mut state = json!({
        "phase": phase,
        "queue": queue,
        "queued": app.composer.queue.len() as u64,
        "now": app.now_line(),
        "mode": app.conn.mode.label(),
        "approvals": approvals,
    });
    if let Some(goal) = app.goals.goal() {
        state["goal"] = json!({
            "objective": goal.objective(), "status": goal.status().as_str(),
            "turns": goal.turns(), "max": octet_core::goal::MAX_GOAL_TURNS});
    }
    state
}

/// The `CatalogInfo` of `app`: the modes and every provider's models.
pub(crate) fn catalog_json(app: &App) -> Value {
    let models: Vec<Value> = app
        .model_entries()
        .into_iter()
        .map(|(engine, model)| {
            json!({"engine": engine.as_str(), "selection": model.selection,
                "name": model.name, "id": model.id.unwrap_or_default()})
        })
        .collect();
    let mut catalog = json!({
        "modes": Mode::ALL.iter().map(|mode| mode.label()).collect::<Vec<_>>(),
        "current_mode": app.conn.mode.label(),
        "models": models,
    });
    if let Some((engine, selection)) = app.current_model() {
        catalog["current"] = json!({"engine": engine.as_str(), "selection": selection});
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_core::remote::{
        protocol,
        run::{registrations, run_dir},
    };
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
    use tokio::net::{UnixStream, unix::OwnedReadHalf};
    use tokio::time::timeout;

    type Reader = Lines<BufReader<OwnedReadHalf>>;

    async fn next_line(lines: &mut Reader) -> String {
        timeout(Duration::from_secs(2), lines.next_line())
            .await
            .expect("a line within two seconds")
            .unwrap()
            .expect("the connection is open")
    }

    /// Publishes until the bridge has adopted the connection and a line arrives.
    async fn first_line(bridge: &mut Bridge, app: &App, lines: &mut Reader) -> Value {
        for _ in 0..100 {
            bridge.publish(app);
            if let Ok(Ok(Some(line))) = timeout(Duration::from_millis(20), lines.next_line()).await
            {
                return serde_json::from_str(&line).unwrap();
            }
        }
        panic!("no frame arrived");
    }

    fn demo() -> (App, octet_core::Config) {
        (
            crate::test_support::app_for(octet_core::Engine::DEMO),
            octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp"),
        )
    }

    async fn connect(path: &std::path::Path) -> (Reader, tokio::net::unix::OwnedWriteHalf) {
        let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
        (BufReader::new(read).lines(), write)
    }

    #[tokio::test]
    async fn a_bridge_registers_listens_and_cleans_up() {
        let temp = octet_testkit::TempDir::new("octet-bridge");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true).await;
        let (app, config) = demo();
        bridge
            .connected(&app, &config, &dir.join("session-1.jsonl"))
            .await;
        let run = run_dir(&dir);
        let regs = registrations(&run).await;
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].journals, [dir.join("session-1.jsonl")]);
        assert_eq!(regs[0].engine, "demo");
        assert_eq!(regs[0].workspace, std::path::Path::new("/tmp"));
        let mode = std::fs::metadata(&run).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // A hub connects and is sent state and catalog at once.
        let (mut lines, _write) = connect(&regs[0].socket).await;
        let first = first_line(&mut bridge, &app, &mut lines).await;
        assert!(matches!(first["type"].as_str(), Some("state" | "catalog")));
        let second: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        let mut kinds = [first["type"].clone(), second["type"].clone()];
        kinds.sort_by_key(ToString::to_string);
        assert_eq!(kinds, ["catalog", "state"]);
        bridge.stop().await;
        assert!(registrations(&run).await.is_empty());
        assert!(!regs[0].socket.exists());
    }

    #[tokio::test]
    async fn reconnecting_keeps_one_registration_and_lists_every_journal() {
        let temp = octet_testkit::TempDir::new("octet-bridge-journals");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true).await;
        let (app, config) = demo();
        for name in ["a.jsonl", "b.jsonl", "b.jsonl"] {
            bridge.connected(&app, &config, &dir.join(name)).await;
        }
        let regs = registrations(&run_dir(&dir)).await;
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].journals, [dir.join("a.jsonl"), dir.join("b.jsonl")]);
        bridge.stop().await;
    }

    #[tokio::test]
    async fn state_frames_go_only_when_state_changes() {
        let temp = octet_testkit::TempDir::new("octet-bridge-changes");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true).await;
        let (mut app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, _write) = connect(&regs[0].socket).await;
        // The new connection gets the last state and catalog.
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        bridge.publish(&app);
        bridge.publish(&app);
        let quiet = timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(quiet.is_err(), "nothing changed, nothing is sent");
        app.composer
            .queue
            .push_back(crate::vendor::Prompt::plain("next thing".into()));
        bridge.publish(&app);
        bridge.publish(&app);
        let sent: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(sent["type"], "state");
        assert_eq!(sent["state"]["queue"], json!(["next thing"]));
        let more = timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(more.is_err(), "one change, one frame");
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_hub_that_stops_reading_is_dropped() {
        let temp = octet_testkit::TempDir::new("octet-bridge-slow");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true).await;
        let (mut app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, _write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        let flood = async {
            for n in 0..10_000 {
                app.composer.queue.clear();
                app.composer
                    .queue
                    .push_back(crate::vendor::Prompt::plain(format!(
                        "{n} {}",
                        "x".repeat(200)
                    )));
                bridge.publish(&app);
            }
        };
        timeout(Duration::from_secs(1), flood)
            .await
            .expect("the bridge never waits for a hub");
        // The buffered lines come out, then the end of the stream.
        let mut seen = 0;
        loop {
            match timeout(Duration::from_secs(5), lines.next_line()).await {
                Ok(Ok(Some(_))) => seen += 1,
                Ok(Ok(None)) => break,
                other => panic!("expected the stream to end, got {other:?}"),
            }
        }
        assert!(seen > 0);
        bridge.stop().await;
    }

    #[tokio::test]
    async fn commands_arrive_and_replies_go_back() {
        let temp = octet_testkit::TempDir::new("octet-bridge-commands");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true).await;
        let (app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, mut write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        let command = protocol::BridgeCommand::Prompt { text: "hi".into() };
        let line = format!(
            "{}\n",
            protocol::bridge_command_json(7, "Gemini 10", &command)
        );
        write.write_all(line.as_bytes()).await.unwrap();
        let remote = timeout(Duration::from_secs(2), bridge.next_command())
            .await
            .unwrap();
        assert_eq!(remote.req, 7);
        assert_eq!(remote.by, "Gemini 10");
        assert_eq!(remote.command, command);
        bridge.reply(7, Ok("Sent".into()));
        bridge.reply(8, Err("A turn is running".into()));
        let ack: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(
            (ack["type"].as_str(), ack["req"].as_u64()),
            (Some("ack"), Some(7))
        );
        let refused: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(refused["type"], "refused");
        assert_eq!(refused["reason"], "A turn is running");
        // A bad line is refused by its req, and never reaches the loop.
        write
            .write_all(b"{\"v\":1,\"req\":9,\"type\":\"nope\"}\n")
            .await
            .unwrap();
        write.write_all(b"not json\n").await.unwrap();
        let bad = timeout(Duration::from_millis(300), bridge.next_command()).await;
        assert!(bad.is_err());
        let bad: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(
            (bad["type"].as_str(), bad["req"].as_u64()),
            (Some("refused"), Some(9))
        );
        let worse: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(
            (worse["type"].as_str(), worse["req"].as_u64()),
            (Some("refused"), Some(0))
        );
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_bridge_that_cannot_start_or_is_off_stays_out_of_the_way() {
        let temp = octet_testkit::TempDir::new("octet-bridge-off");
        let dir = temp.path().to_path_buf();
        let mut off = Bridge::start(&dir, false).await;
        assert!(off.take_note().is_none());
        assert!(!run_dir(&dir).exists());
        let (app, config) = demo();
        off.connected(&app, &config, &dir.join("j.jsonl")).await;
        off.publish(&app);
        off.reply(1, Ok("x".into()));
        assert!(
            timeout(Duration::from_millis(50), off.next_command())
                .await
                .is_err()
        );
        off.stop().await;
        // The data directory is a file: the run directory cannot be made.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&blocked, "").unwrap();
        let mut broken = Bridge::start(&blocked, true).await;
        let note = broken.take_note().expect("a note says why");
        assert!(note.starts_with("Remote control is off: "), "{note}");
        assert!(broken.take_note().is_none());
        broken.connected(&app, &config, &dir.join("j.jsonl")).await;
        broken.stop().await;
    }

    #[test]
    fn state_and_catalog_describe_the_session() {
        use octet_core::Event;
        let (mut app, _) = demo();
        crate::test_support::idle(&mut app);
        app.composer
            .queue
            .push_back(crate::vendor::Prompt::plain("first line\nsecond".into()));
        app.goals.set_goal(Some(
            octet_core::goal::Goal::new("Ship the release").unwrap(),
        ));
        app.event(crate::test_support::approval(1, "cargo test"));
        let state = state_json(&app);
        assert_eq!(state["phase"], "idle");
        assert_eq!(state["queue"], json!(["first line"]));
        assert_eq!(state["queued"], 1);
        assert_eq!(state["goal"]["objective"], "Ship the release");
        assert_eq!(state["goal"]["status"], "active");
        assert_eq!(state["goal"]["turns"], 0);
        assert_eq!(state["goal"]["max"], octet_core::goal::MAX_GOAL_TURNS);
        assert_eq!(state["now"], "Waiting for approval");
        assert_eq!(state["mode"], "ask");
        assert_eq!(state["approvals"][0]["id"], 1);
        assert_eq!(
            state["approvals"][0]["summary"]["action"],
            "Run a shell command"
        );
        conforms("StateInfo", &state);
        app.event(Event::ApprovalClosed(1));
        app.goals.set_goal(None);
        let state = state_json(&app);
        assert!(state.get("goal").is_none());
        conforms("StateInfo", &state);

        let catalog = catalog_json(&app);
        let labels: Vec<&str> = octet_core::Mode::ALL.iter().map(|m| m.label()).collect();
        assert_eq!(catalog["modes"], json!(labels));
        assert_eq!(catalog["current_mode"], "ask");
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), app.model_entries().len());
        conforms("CatalogInfo", &catalog);
        match app.current_model() {
            Some((engine, selection)) => assert_eq!(
                catalog["current"],
                json!({"engine": engine.as_str(), "selection": selection})
            ),
            None => assert!(catalog.get("current").is_none()),
        }
    }

    /// The model list and current choice appear as the app has them.
    #[test]
    fn the_catalog_lists_models_and_marks_the_current_one() {
        let (mut app, config) = demo();
        app.connection(&config, "journal".into());
        app.conn.models = vec![octet_core::ModelInfo {
            selection: "default".into(),
            name: "Demo".into(),
            id: Some("demo-1".into()),
            description: String::new(),
        }];
        let catalog = catalog_json(&app);
        assert_eq!(
            catalog["models"][0],
            json!({"engine": "demo", "selection": "default", "name": "Demo", "id": "demo-1"})
        );
        assert_eq!(
            catalog["current"],
            json!({"engine": "demo", "selection": "default"})
        );
    }

    /// Checks `value` against the protocol's shape `name`: required fields present, no others.
    fn conforms(name: &str, value: &Value) {
        let shape = protocol::OBJECTS.iter().find(|s| s.name == name).unwrap();
        let map = value.as_object().unwrap();
        for field in shape.fields {
            assert!(
                field.optional || map.get(field.name).is_some_and(|v| !v.is_null()),
                "{name} lacks {}",
                field.name
            );
        }
        for key in map.keys() {
            assert!(
                shape.fields.iter().any(|f| f.name == key),
                "{name} has an unknown {key}"
            );
        }
    }
}
