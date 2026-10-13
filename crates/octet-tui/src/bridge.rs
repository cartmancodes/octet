//! The bridge: shares this terminal UI's session with a hub over a private Unix socket. It lives
//! across reconnects, keeps its registration in the run directory up to date, and sends `state`
//! and `catalog` frames to every connected hub. It never makes the interface wait: each hub has
//! a bounded queue, and one that falls behind is disconnected. A bridge that cannot start or
//! breaks turns itself off with one note; the interface goes on.
use crate::{
    Action, Exit,
    app::{APPROVAL_ARM, App, CANCELLING, ConnPhase, StatusKind},
    commands::{mode_command, model_command, steer_with},
    input::{answer_approval, cancel_turn, send_remote_prompt},
    registry::TURN_OPEN,
    vendor::Vendor,
};
use octet_core::remote::{
    protocol::{self, BridgeCommand, Code, MAX_BRIDGE_LINE, MAX_MESSAGE},
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
    sync::{mpsc, oneshot},
    task::{AbortHandle, JoinHandle},
};

/// Lines waiting to be written to one hub before it counts as too slow.
const PEER_QUEUE: usize = 256;
/// Commands read from hubs and not yet run by the interface.
const COMMAND_QUEUE: usize = 64;
/// How long one write to a hub may take before the hub is let go.
const WRITE_STALL: Duration = Duration::from_secs(30);
/// The most a reader takes for one line: a whole line, its newline, and one byte over.
const LINE_LIMIT: u64 = MAX_BRIDGE_LINE as u64 + 2;
/// The longest an approval's command is in a `state` frame, in bytes.
const COMMAND_BYTES: usize = 4096;
/// The longest an approval's working directory is, in bytes.
const CWD_BYTES: usize = 1024;
/// How many of an approval's files are listed.
const FILES_SHOWN: usize = 20;
/// The longest one listed file is, in bytes.
const FILE_BYTES: usize = 512;
/// The longest an approval's action or scope is, in bytes.
const ACTION_BYTES: usize = 512;
/// The longest a device's name is in a desk note, in characters.
const DEVICE_NAME: usize = 40;
/// The longest a queued prompt's first line is in a `state` frame, in characters.
const QUEUE_LINE: usize = 200;
/// The refusal of an end or reconnect while the desk's external editor is open.
const DESK_EDITING: &str = "The desk is editing a prompt";
/// The refusal of an end or reconnect while a `!` command runs at the desk.
const DESK_SHELL: &str = "A ! command is running at the desk";
/// The refusal of an end while the desk holds a draft, `!` output or images not yet sent.
const DESK_DRAFT: &str = "The desk has an unsent draft";
/// The refusal of an answer to an approval younger than the arm delay, as the desk refuses a key.
const APPROVAL_OPENING: &str = "The approval just opened";

/// A command from a hub, for the interface to run.
#[derive(Debug)]
pub(crate) struct Remote {
    /// The connection it came on, which alone gets the reply.
    pub(crate) peer: u64,
    /// The hub's id for the command, echoed in the reply.
    pub(crate) req: u64,
    /// The device the command came from.
    pub(crate) by: String,
    /// What it asks.
    pub(crate) command: BridgeCommand,
}

/// One connected hub: the queue its writer drains, and its tasks.
struct Peer {
    id: u64,
    lines: mpsc::Sender<String>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Peer {
    /// Starts the tasks that read commands from `stream` and write the queued lines to it.
    fn spawn(stream: UnixStream, id: u64, commands: mpsc::Sender<Remote>) -> Self {
        let (read, write) = stream.into_split();
        let (lines, queue) = mpsc::channel(PEER_QUEUE);
        // Dropped when the reader ends, however it ends: the writer then closes the link.
        let (reading, reader_gone) = oneshot::channel::<()>();
        let writer = tokio::spawn(write_peer(write, queue, reader_gone)).abort_handle();
        let reader = tokio::spawn(read_peer(read, id, lines.clone(), commands, reading));
        let reader = reader.abort_handle();
        Self {
            id,
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

/// Writes queued lines until the queue closes, the hub goes, or a write stalls. Once the hub's
/// reader has ended (it went, or sent a line over the limit), the lines already queued, such as
/// that refusal, are written and the connection closes, so the hub knows to connect again rather
/// than send commands nobody reads.
async fn write_peer(
    mut write: OwnedWriteHalf,
    mut queue: mpsc::Receiver<String>,
    mut reader_gone: oneshot::Receiver<()>,
) {
    let send = async |write: &mut OwnedWriteHalf, line: String| {
        let written = tokio::time::timeout(WRITE_STALL, write.write_all(line.as_bytes())).await;
        matches!(written, Ok(Ok(())))
    };
    loop {
        tokio::select! {
            biased;
            _ = &mut reader_gone => {
                while let Ok(line) = queue.try_recv() {
                    if !send(&mut write, line).await {
                        return;
                    }
                }
                let _ = write.shutdown().await;
                return;
            }
            line = queue.recv() => {
                let Some(line) = line else { return };
                if !send(&mut write, line).await {
                    return;
                }
            }
        }
    }
}

/// The `req` of a line the bridge could not use, so the refusal names it; 0 when there is none.
/// [`protocol::req_of`] stops at a phone message's size, and a hub's line may be a little longer.
fn bad_line_req(text: &str) -> u64 {
    if text.len() <= MAX_MESSAGE {
        return protocol::req_of(text);
    }
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("req")?.as_u64())
        .unwrap_or_default()
}

/// A parse refusal as the bridge says it. The protocol's parser words a version mismatch for the
/// hub refusing a phone ("this hub speaks"); here the bridge refuses the hub.
fn bridge_words(reason: String) -> String {
    match reason.strip_prefix("Unsupported protocol version; this hub ") {
        Some(rest) => format!("Unsupported protocol version; the hub {rest}"),
        None => reason,
    }
}

/// Reads a hub's command lines. A bad line is refused to that hub alone; a good one goes to the
/// interface. Ends when the hub goes or sends a message over the limit; `reading` is dropped then.
async fn read_peer(
    read: OwnedReadHalf,
    peer: u64,
    replies: mpsc::Sender<String>,
    commands: mpsc::Sender<Remote>,
    reading: oneshot::Sender<()>,
) {
    let _reading = reading;
    let refuse = |req: u64, code: Code, reason: &str| {
        let _ = replies.try_send(frame_line(&protocol::refused(req, code, reason)));
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
        if buffer.last() != Some(&b'\n') && buffer.len() > MAX_BRIDGE_LINE {
            refuse(
                0,
                Code::TooLarge,
                &format!("Message too large; the limit is {MAX_BRIDGE_LINE} bytes"),
            );
            return;
        }
        let Ok(text) = std::str::from_utf8(&buffer) else {
            refuse(0, Code::Invalid, "Message must be UTF-8 text");
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match protocol::parse_bridge_command(text) {
            Ok((req, by, command)) => {
                if commands
                    .send(Remote {
                        peer,
                        req,
                        by,
                        command,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(reason) => refuse(bad_line_req(text), Code::Invalid, &bridge_words(reason)),
        }
    }
}

/// Hands each new connection to the bridge.
async fn accept(
    listener: UnixListener,
    arrivals: mpsc::Sender<Peer>,
    commands: mpsc::Sender<Remote>,
) {
    let mut next_id = 0;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                next_id += 1;
                if arrivals
                    .send(Peer::spawn(stream, next_id, commands.clone()))
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

/// Runs one remote command as the matching key or slash command would; the reply's text, or the
/// refusal, and what the interface then does. `editing` says the desk's external editor has the
/// terminal.
pub(crate) async fn handle_remote(
    app: &mut App,
    vendor: &dyn Vendor,
    remote: Remote,
    editing: bool,
) -> (Result<String, String>, Action) {
    let mut action = Action::Continue;
    // Ending or reconnecting would kill the editor or the `!` command, and lose its work.
    let desk = if editing {
        Some(DESK_EDITING)
    } else if app.shell.is_some() {
        Some(DESK_SHELL)
    } else {
        None
    };
    let (seq, status) = (app.last_seq(), app.status_line.clone());
    let done = run_remote(app, vendor, remote.command, desk, &mut action).await;
    let by = device_name(&remote.by);
    match done {
        Ok((what, reply)) => {
            app.note(format!("{what} from {by}"));
            // The note must not hide that a cancel is under way.
            if app.conn.is_cancelling() {
                app.status(StatusKind::Cancelling, CANCELLING);
            }
            (Ok(reply), action)
        }
        Err(reason) => {
            // The desk's own refusal text, put there by the command run for the phone, says who
            // asked: the desk user typed none of it.
            let refused = format!("Refused from {by}: ");
            app.prefix_since(seq, &refused);
            if app.status_line != status {
                let kind = app.status_kind;
                app.status(kind, &format!("{refused}{}", app.status_line));
            }
            (Err(reason), Action::Continue)
        }
    }
}

/// The desk holds work an end would throw away: a draft, or `!` output or images waiting for it.
fn desk_has_draft(app: &App) -> bool {
    let composer = &app.composer;
    !composer.editor.text().is_empty()
        || !composer.attachments.is_empty()
        || !composer.images.is_empty()
}

/// `text` is something to send as a prompt: not blank, and not what the desk would run as a slash
/// command.
fn plain_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err("Nothing to send".into())
    } else if crate::input::is_command_line(text.trim()) {
        Err("Slash commands run on the desk".into())
    } else {
        Ok(())
    }
}

/// The code of a refusal, from its words: each is one of the interface's own refusals.
fn refusal_code(reason: &str) -> Code {
    if reason == TURN_OPEN {
        Code::TurnOpen
    } else if reason == crate::registry::NOT_CONNECTED {
        Code::NotConnected
    } else if reason == DESK_EDITING
        || reason == DESK_SHELL
        || reason == DESK_DRAFT
        || reason == APPROVAL_OPENING
        || reason.starts_with("The queue is full")
        || reason.starts_with("Mode change pending")
    {
        Code::Busy
    } else if reason.starts_with("Full access needs confirm") {
        Code::NotAllowed
    } else if reason.starts_with("Message too large") {
        Code::TooLarge
    } else {
        Code::Invalid
    }
}

/// The desk note for an interface a phone started in full access (spec §3); `None` otherwise.
pub(crate) fn started_note(tmux: Option<&str>, by: Option<&str>, mode: Mode) -> Option<String> {
    (tmux.is_some() && mode == Mode::FullAccess).then(|| {
        format!(
            "⚠ Started in full access from {}",
            device_name(by.unwrap_or_default())
        )
    })
}

/// A hub-supplied device name made safe for a desk note: one line, at most [`DEVICE_NAME`] characters.
fn device_name(name: &str) -> String {
    let one_line = crate::text::clean(name)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.is_empty() {
        return "a remote device".into();
    }
    if one_line.chars().count() <= DEVICE_NAME {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(DEVICE_NAME - 1).collect();
    cut.push('…');
    cut
}

/// What a remote command did: `(what the desk note says, the reply)`, or why it was refused.
/// `desk` is why the desk cannot be ended or reconnected now, if it cannot.
async fn run_remote(
    app: &mut App,
    vendor: &dyn Vendor,
    command: BridgeCommand,
    desk: Option<&'static str>,
    action: &mut Action,
) -> Result<(String, String), String> {
    match command {
        BridgeCommand::Prompt { text } => {
            plain_text(&text)?;
            let waiting = app.composer.queue.len();
            if !send_remote_prompt(app, vendor, text) {
                return Err(app.status_line.clone());
            }
            // Queued, the status line says how many wait, as Enter's does.
            let reply = if app.composer.queue.len() > waiting {
                app.status_line.clone()
            } else {
                "Sent".into()
            };
            Ok(("Prompt".into(), reply))
        }
        BridgeCommand::Answer { approval, allow } => {
            // A double tap meant for the last approval must not answer this one.
            if app
                .overlay
                .approvals
                .iter()
                .any(|asked| asked.id == approval && asked.arrived.elapsed() < APPROVAL_ARM)
            {
                return Err(APPROVAL_OPENING.into());
            }
            answer_approval(app, vendor, approval, allow)?;
            let what = if allow { "Allowed" } else { "Denied" };
            Ok((what.into(), what.into()))
        }
        BridgeCommand::Cancel => {
            if !app.is_busy() {
                return Err("No turn is running".into());
            }
            cancel_turn(app, vendor).await;
            Ok(("Cancelled".into(), "Cancelling".into()))
        }
        BridgeCommand::Steer { text } => {
            plain_text(&text)?;
            let (waiting, running) = (app.composer.queue.len(), app.conn.is_running());
            if !steer_with(app, vendor, text, send_remote_prompt) {
                return Err(app.status_line.clone());
            }
            // Queued, the status line has the desk's words for why.
            let reply = if app.composer.queue.len() > waiting {
                app.status_line.clone()
            } else if running {
                "Steering".into()
            } else {
                "Sent as a prompt".into()
            };
            Ok(("Steer".into(), reply))
        }
        BridgeCommand::SetMode { mode, confirm } => {
            remote_mode(app, vendor, &mode, confirm.as_deref(), desk, action)
        }
        BridgeCommand::SetModel { selection } => remote_model(app, &selection, desk, action),
        BridgeCommand::End => {
            if let Some(busy) = desk {
                return Err(busy.into());
            }
            if app.turn_open() {
                return Err(TURN_OPEN.into());
            }
            if desk_has_draft(app) {
                return Err(DESK_DRAFT.into());
            }
            *action = Action::Exit(Exit::Quit);
            Ok(("Ended".into(), "Ending".into()))
        }
    }
}

/// A remote `set_mode`, as `/mode` runs it; `desk` refuses one that would reconnect.
fn remote_mode(
    app: &mut App,
    vendor: &dyn Vendor,
    mode: &str,
    confirm: Option<&str>,
    desk: Option<&'static str>,
    action: &mut Action,
) -> Result<(String, String), String> {
    let Some(target) = Mode::parse(mode) else {
        let shown: String = mode.chars().take(40).collect();
        return Err(format!(
            "Unknown mode {}. Use ask, accept-edits, auto or full-access.",
            crate::text::clean(&shown)
        ));
    };
    if !protocol::full_access_confirmed(target.label(), confirm) {
        return Err(format!(
            "Full access needs confirm: \"{}\"",
            protocol::FULL_ACCESS_CONFIRM
        ));
    }
    let was_pending = app.conn.mode_pending.is_some();
    *action = mode_command(app, vendor, target.label());
    let label = target.label();
    let reply = match action {
        // The reconnect has not begun: nothing was changed on the way to it.
        Action::Exit(_) if let Some(busy) = desk => {
            *action = Action::Continue;
            return Err(busy.into());
        }
        Action::Exit(_) => format!("Reconnecting in {label}"),
        _ if !was_pending && app.conn.mode_pending.is_some() => format!("Switching to {label}"),
        _ => return Err(app.status_line.clone()),
    };
    let what = if target == Mode::FullAccess {
        "⚠ Full access".into()
    } else {
        format!("Mode {label}")
    };
    Ok((what, reply))
}

/// A remote `set_model`, as `/model NAME` runs it; `desk` refuses it, since it always reconnects.
fn remote_model(
    app: &mut App,
    selection: &str,
    desk: Option<&'static str>,
    action: &mut Action,
) -> Result<(String, String), String> {
    let one_token = !selection.is_empty()
        && !selection.starts_with('-')
        && !selection
            .chars()
            .any(|c| c.is_whitespace() || c.is_control());
    // `list` and `refresh` are the desk's own picker commands, not models.
    if !one_token || matches!(selection, "list" | "refresh") {
        return Err("Name one model, without spaces".into());
    }
    if let Some(busy) = desk {
        return Err(busy.into());
    }
    *action = model_command(app, selection);
    if matches!(action, Action::Exit(_)) {
        Ok((
            format!("Model {selection}"),
            format!("Reconnecting with {selection}"),
        ))
    } else {
        Err(app.status_line.clone())
    }
}

/// `frame` as one line.
fn frame_line(frame: &Value) -> String {
    format!("{frame}\n")
}

/// The data directory is too deep for a socket in it: a Unix socket's path has at most 104 bytes
/// on macOS and 108 on Linux.
#[derive(Debug, thiserror::Error)]
#[error("the socket path {} is too long; a shorter data directory turns it on", .0.display())]
struct SocketPathTooLong(PathBuf);

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
    /// The state the `state` line says, to tell a change before formatting anything.
    state_value: Option<Value>,
    /// Fingerprint of what the catalog lists, to rebuild it only when that changes.
    catalog_key: Option<u64>,
}

impl Live {
    /// Makes the run directory and listens on `<run>/<pid>.sock`; `tmux`, the session a phone
    /// started this interface in, is registered.
    async fn open(directory: &Path, tmux: Option<String>) -> std::io::Result<Self> {
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
        let listener = UnixListener::bind(&socket).map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidInput {
                // Only the path's length makes a bind of a path this short of NULs invalid.
                std::io::Error::new(error.kind(), SocketPathTooLong(socket.clone()))
            } else {
                error
            }
        })?;
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
                from_phone: tmux.is_some(),
                tmux,
            },
            run,
            socket,
            peers: Vec::new(),
            arrivals,
            commands,
            accepting: tokio::spawn(accept(listener, arrivals_in, commands_in)),
            state: None,
            catalog: None,
            state_value: None,
            catalog_key: None,
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

    /// Removes the socket and the registration files without waiting; the files are gone twice
    /// over after a normal `shutdown`, which is harmless.
    fn remove_files(&self) {
        let pid = self.registration.pid;
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.run.join(format!("{pid}.json")));
        let _ = std::fs::remove_file(self.run.join(format!("{pid}.json.tmp")));
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

impl Drop for Live {
    /// A panic or an early return that skipped `Bridge::stop` still leaves no stale files.
    fn drop(&mut self) {
        self.accepting.abort();
        self.remove_files();
    }
}

/// This terminal UI's bridge; does nothing when remote control is off or could not start.
pub(crate) struct Bridge {
    live: Option<Live>,
    /// Why the bridge is off, until the interface shows it.
    note: Option<String>,
    /// Why the bridge is off, for `/remote-control`; `None` while it runs or was turned off.
    off: Option<String>,
}

impl Bridge {
    /// Listens for hubs under `directory`, unless `enabled` is false. A failure leaves a bridge
    /// that is off, with a note saying why; a data directory too deep for a socket is said only
    /// by `/remote-control`, since the bridge is on by default and the user may never want it.
    /// `tmux` names the tmux session a hub's `start` launched this interface in.
    pub(crate) async fn start(directory: &Path, enabled: bool, tmux: Option<String>) -> Bridge {
        let mut bridge = Bridge {
            live: None,
            note: None,
            off: None,
        };
        if !enabled {
            return bridge;
        }
        match Live::open(directory, tmux).await {
            Ok(live) => bridge.live = Some(live),
            Err(error) => {
                let too_long =
                    matches!(error.get_ref(), Some(inner) if inner.is::<SocketPathTooLong>());
                let why = format!("Remote control is off: {error}");
                bridge.note = (!too_long).then(|| why.clone());
                bridge.off = Some(why);
            }
        }
        bridge
    }

    /// The note for the interface to show, once.
    pub(crate) fn take_note(&mut self) -> Option<String> {
        self.note.take()
    }

    /// Shows the note, once, and keeps why the bridge is off where `/remote-control` finds it.
    pub(crate) fn tell(&mut self, app: &mut App) {
        if let Some(note) = self.note.take() {
            app.note(note);
        }
        if app.remote_off != self.off {
            app.remote_off.clone_from(&self.off);
        }
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
        let state = state_json(app);
        if live.state_value.as_ref() != Some(&state) {
            let line = state_line(&state);
            live.broadcast(&line);
            live.state = Some(line);
            live.state_value = Some(state);
        }
        let key = catalog_key(app);
        if live.catalog_key != Some(key) {
            live.catalog_key = Some(key);
            let line = catalog_line(&catalog_json(app));
            if live.catalog.as_ref() != Some(&line) {
                live.broadcast(&line);
                live.catalog = Some(line);
            }
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

    /// Tells hub `peer`, and no other, how its command `req` went: `Ok` carries what was done,
    /// `Err` the reason.
    pub(crate) fn reply(&mut self, peer: u64, req: u64, result: Result<String, String>) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        // The command may have beaten its hub's adoption.
        live.adopt_arrivals();
        let frame = match result {
            Ok(message) => protocol::ack(req, &message),
            Err(reason) => protocol::refused(req, refusal_code(&reason), &reason),
        };
        let line = frame_line(&frame);
        live.peers
            .retain(|hub| hub.id != peer || hub.lines.try_send(line.clone()).is_ok());
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
        if let Err(error) = write_registration(&live.registration, &live.run).await {
            if let Some(live) = self.live.take() {
                live.shutdown().await;
            }
            let why = format!("Remote control is off: {error}");
            self.note = Some(why.clone());
            self.off = Some(why);
        }
    }
}

/// Writes `<run>/<pid>.json` atomically: an owner-only temp file, synced once, then a rename, so a
/// hub reads the old registration or the new one, never part of one. Unlike
/// [`Registration::write`] the directory is not synced. The interface waits for this write on a
/// mode, model or session change, and on macOS each sync is a full flush of the disk (about 5 ms,
/// more on a busy disk). Syncing the directory as well would buy nothing: the file describes a
/// live process, so after a power loss there is nothing left for it to describe.
async fn write_registration(registration: &Registration, run: &Path) -> std::io::Result<()> {
    let temp = run.join(format!("{}.json.tmp", registration.pid));
    match tokio::fs::remove_file(&temp).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .await?;
    file.write_all(registration.to_json().to_string().as_bytes())
        .await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temp, run.join(format!("{}.json", registration.pid))).await
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

/// `text` cut to at most `max` bytes at a character boundary, ending in "…" when cut.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The first files of an approval, each clipped, then "+N more" for the rest, as the dialog says.
fn clipped_files(files: &[String]) -> Vec<String> {
    let mut shown: Vec<String> = files
        .iter()
        .take(FILES_SHOWN)
        .map(|file| clip(file, FILE_BYTES))
        .collect();
    if files.len() > FILES_SHOWN {
        shown.push(format!("+{} more", files.len() - FILES_SHOWN));
    }
    shown
}

/// The `state` frame line for `state`. One that would not fit in a message is rebuilt without
/// the queue, with each approval reduced to its action and scope, and without the goal if
/// that is still too much, so a hub always accepts it and the approval ids still get through.
fn state_line(state: &Value) -> String {
    let line = frame_line(&protocol::bridge_state_frame(state));
    if line.len() <= MAX_MESSAGE {
        return line;
    }
    let mut reduced = state.clone();
    reduced["queue"] = json!([]);
    if let Some(approvals) = reduced["approvals"].as_array_mut() {
        for approval in approvals {
            let summary = &approval["summary"];
            approval["summary"] =
                json!({"action": summary["action"], "files": [], "scope": summary["scope"]});
        }
    }
    let mut line = frame_line(&protocol::bridge_state_frame(&reduced));
    if line.len() > MAX_MESSAGE {
        if let Some(map) = reduced.as_object_mut() {
            map.remove("goal");
        }
        line = frame_line(&protocol::bridge_state_frame(&reduced));
    }
    line
}

/// The `catalog` frame line for `catalog`; models are dropped from the end until it fits.
fn catalog_line(catalog: &Value) -> String {
    let mut catalog = catalog.clone();
    loop {
        let line = frame_line(&protocol::bridge_catalog_frame(&catalog));
        let models = catalog["models"].as_array_mut();
        match models {
            Some(models) if line.len() > MAX_MESSAGE && !models.is_empty() => {
                models.truncate(models.len() / 2);
            }
            _ => return line,
        }
    }
}

/// A fingerprint of everything `catalog_json` lists, taken without building it.
fn catalog_key(app: &App) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let conn = &app.conn;
    (conn.engine.as_str(), conn.mode.label()).hash(&mut hasher);
    (&conn.requested_model, &conn.resolved_model).hash(&mut hasher);
    let listed = |hasher: &mut std::collections::hash_map::DefaultHasher,
                  models: &[octet_core::ModelInfo]| {
        models.len().hash(hasher);
        for model in models {
            (&model.selection, &model.name, &model.id).hash(hasher);
        }
    };
    listed(&mut hasher, &conn.models);
    for engine in octet_core::Engine::ALL.iter().filter(|e| e.is_vendor()) {
        if let Some(cached) = app.models.catalogs.get(*engine) {
            listed(&mut hasher, &cached.models);
        }
    }
    hasher.finish()
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
            let summary = &asked.summary;
            json!({"id": asked.id, "summary": {
                "action": clip(&summary.action, ACTION_BYTES),
                "command": summary.command.as_deref().map(|c| clip(c, COMMAND_BYTES)),
                "files": clipped_files(&summary.files),
                "cwd": summary.cwd.as_deref().map(|c| clip(c, CWD_BYTES)),
                "scope": clip(&summary.scope, ACTION_BYTES)}})
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
        let mut bridge = Bridge::start(&dir, true, None).await;
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
    async fn the_registration_is_replaced_whole_and_kept_private() {
        let temp = octet_testkit::TempDir::new("octet-bridge-write");
        let run = run_dir(temp.path());
        std::fs::create_dir_all(&run).unwrap();
        let mut registration = Registration {
            id: "4242-1790000000".into(),
            pid: 4242,
            started: 1_790_000_000,
            socket: run.join("4242.sock"),
            engine: "codex".into(),
            workspace: "/work".into(),
            model: "gpt".into(),
            mode: "ask".into(),
            vendor_session: String::new(),
            journals: vec!["/j/a.jsonl".into()],
            from_phone: false,
            tmux: None,
        };
        write_registration(&registration, &run).await.unwrap();
        // A stale temp file is no obstacle, and a rewrite replaces the whole file.
        std::fs::write(run.join("4242.json.tmp"), "stale").unwrap();
        registration.mode = "auto".into();
        registration.vendor_session = "thread-1".into();
        write_registration(&registration, &run).await.unwrap();
        assert_eq!(
            registrations(&run).await,
            std::slice::from_ref(&registration)
        );
        let written = std::fs::read(run.join("4242.json")).unwrap();
        assert_eq!(written, registration.to_json().to_string().into_bytes());
        let mode = std::fs::metadata(run.join("4242.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!run.join("4242.json.tmp").exists());
        // A run directory that is gone is an error, which turns the bridge off.
        assert!(
            write_registration(&registration, &run.join("gone"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_interface_started_from_the_phone_registers_as_such() {
        let temp = octet_testkit::TempDir::new("octet-bridge-phone");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, Some("octet-abc123".into())).await;
        let (app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        assert_eq!(regs.len(), 1);
        assert!(regs[0].from_phone);
        assert_eq!(regs[0].tmux.as_deref(), Some("octet-abc123"));
        bridge.stop().await;
    }

    #[test]
    fn full_access_started_from_a_phone_is_noted_on_the_desk() {
        let full = octet_core::Mode::FullAccess;
        assert_eq!(
            started_note(Some("octet-abc123"), Some("Gemini\u{1b}[2J 10"), full).as_deref(),
            Some("⚠ Started in full access from Gemini 10")
        );
        assert_eq!(
            started_note(Some("octet-abc123"), None, full).as_deref(),
            Some("⚠ Started in full access from a remote device")
        );
        assert!(started_note(Some("octet-abc123"), Some("p"), octet_core::Mode::Ask).is_none());
        assert!(
            started_note(None, Some("p"), full).is_none(),
            "a desk start says nothing"
        );
    }

    #[test]
    fn refusals_carry_the_code_a_phone_acts_on() {
        use protocol::Code;
        for (reason, code) in [
            (TURN_OPEN, Code::TurnOpen),
            (crate::registry::NOT_CONNECTED, Code::NotConnected),
            (DESK_EDITING, Code::Busy),
            (DESK_SHELL, Code::Busy),
            (DESK_DRAFT, Code::Busy),
            (APPROVAL_OPENING, Code::Busy),
            (
                "The queue is full (8 prompts); wait for the turn or press Esc",
                Code::Busy,
            ),
            (
                "Mode change pending; wait for the vendor to confirm",
                Code::Busy,
            ),
            (
                "Full access needs confirm: \"full access\"",
                Code::NotAllowed,
            ),
            (
                "Message too large; the limit is 99328 bytes",
                Code::TooLarge,
            ),
            ("No approval 9 is waiting", Code::Invalid),
            ("Slash commands run on the desk", Code::Invalid),
        ] {
            assert_eq!(refusal_code(reason), code, "{reason}");
        }
    }

    #[tokio::test]
    async fn reconnecting_keeps_one_registration_and_lists_every_journal() {
        let temp = octet_testkit::TempDir::new("octet-bridge-journals");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
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
        let mut bridge = Bridge::start(&dir, true, None).await;
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
        let mut bridge = Bridge::start(&dir, true, None).await;
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
        let mut bridge = Bridge::start(&dir, true, None).await;
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
        // A second hub is connected and sees none of the replies.
        let (mut other, _other_write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut other).await;
        let _ = next_line(&mut other).await;
        bridge.reply(remote.peer, 7, Ok("Sent".into()));
        bridge.reply(remote.peer, 8, Err(TURN_OPEN.into()));
        let ack: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(
            (ack["type"].as_str(), ack["req"].as_u64()),
            (Some("ack"), Some(7))
        );
        let refused: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(refused["type"], "refused");
        assert_eq!(refused["reason"], TURN_OPEN);
        assert_eq!(refused["code"], "turn_open");
        let overheard = timeout(Duration::from_millis(100), other.next_line()).await;
        assert!(overheard.is_err(), "a reply goes to the hub that asked");
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
        assert_eq!(bad["code"], "invalid");
        let worse: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(
            (worse["type"].as_str(), worse["req"].as_u64()),
            (Some("refused"), Some(0))
        );
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_hub_that_sends_an_oversized_line_is_disconnected() {
        let temp = octet_testkit::TempDir::new("octet-bridge-oversized");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
        let (mut app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, mut write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        write
            .write_all(&vec![b'x'; MAX_BRIDGE_LINE + 10])
            .await
            .unwrap();
        let refused: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(refused["code"], "too_large", "{refused}");
        // The state keeps changing, but the link is closed: the hub sees the end and reconnects.
        let ended = async {
            for n in 0.. {
                app.composer.queue.clear();
                app.composer
                    .queue
                    .push_back(crate::vendor::Prompt::plain(format!("{n}")));
                bridge.publish(&app);
                if let Ok(Ok(None) | Err(_)) =
                    timeout(Duration::from_millis(20), lines.next_line()).await
                {
                    return;
                }
            }
        };
        timeout(Duration::from_secs(2), ended)
            .await
            .expect("the bridge closes the link");
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_bad_line_over_a_phone_message_is_refused_by_its_req() {
        let temp = octet_testkit::TempDir::new("octet-bridge-bad-long");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
        let (app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, mut write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        let line = format!(
            "{}\n",
            json!({"v": 1, "req": 7, "type": "nope", "pad": "p".repeat(MAX_MESSAGE + 200)})
        );
        assert!(line.len() > MAX_MESSAGE && line.len() < MAX_BRIDGE_LINE);
        write.write_all(line.as_bytes()).await.unwrap();
        let refused: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(
            (refused["type"].as_str(), refused["req"].as_u64()),
            (Some("refused"), Some(7)),
            "{refused}"
        );
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_hub_on_another_protocol_version_is_told_which_the_hub_speaks() {
        let temp = octet_testkit::TempDir::new("octet-bridge-version");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
        let (app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, mut write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        write
            .write_all(b"{\"v\":2,\"req\":4,\"type\":\"cancel\",\"by\":\"p\"}\n")
            .await
            .unwrap();
        let refused: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(refused["req"], 4, "{refused}");
        assert_eq!(
            refused["reason"],
            "Unsupported protocol version; the hub speaks version 1"
        );
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_command_a_little_over_a_phone_message_still_arrives() {
        let temp = octet_testkit::TempDir::new("octet-bridge-slack");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
        let (app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, mut write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        // The hub wraps a phone's largest prompt with the device's name: a little over the limit.
        let command = protocol::BridgeCommand::Prompt {
            text: "x".repeat(MAX_MESSAGE),
        };
        let line = format!(
            "{}\n",
            protocol::bridge_command_json(3, "A phone", &command)
        );
        assert!(line.len() > MAX_MESSAGE + 2);
        write.write_all(line.as_bytes()).await.unwrap();
        let remote = timeout(Duration::from_secs(2), bridge.next_command())
            .await
            .expect("the command arrives");
        assert_eq!(remote.command, command);
        bridge.stop().await;
    }

    #[tokio::test]
    async fn a_bridge_that_cannot_start_or_is_off_stays_out_of_the_way() {
        let temp = octet_testkit::TempDir::new("octet-bridge-off");
        let dir = temp.path().to_path_buf();
        let mut off = Bridge::start(&dir, false, None).await;
        assert!(off.take_note().is_none());
        assert!(!run_dir(&dir).exists());
        let (app, config) = demo();
        off.connected(&app, &config, &dir.join("j.jsonl")).await;
        off.publish(&app);
        off.reply(1, 1, Ok("x".into()));
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
        let mut broken = Bridge::start(&blocked, true, None).await;
        let note = broken.take_note().expect("a note says why");
        assert!(note.starts_with("Remote control is off: "), "{note}");
        assert!(broken.take_note().is_none());
        broken.connected(&app, &config, &dir.join("j.jsonl")).await;
        broken.stop().await;
    }

    #[tokio::test]
    async fn a_data_dir_too_long_for_a_socket_turns_the_bridge_off_quietly() {
        let temp = octet_testkit::TempDir::new("octet-bridge-long");
        // Far past the 104 bytes (macOS) or 108 (Linux) a socket's path may have.
        let deep = temp.path().join("d".repeat(200));
        let mut bridge = Bridge::start(&deep, true, None).await;
        assert!(bridge.take_note().is_none(), "nothing is said at start");
        let (mut app, config) = demo();
        crate::connect(&mut app, &mut bridge, &config, &deep.join("j.jsonl")).await;
        assert!(
            !app.entries_text().contains("Remote control"),
            "{}",
            app.entries_text()
        );
        let off = app.remote_off.clone().expect("/remote-control can say why");
        assert!(off.starts_with("Remote control is off: "), "{off}");
        assert!(off.contains("too long"), "{off}");
        bridge.stop().await;
        // Any other failure is still said at once, and kept for /remote-control too.
        let blocked = temp.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let mut broken = Bridge::start(&blocked, true, None).await;
        let mut app = demo().0;
        crate::connect(&mut app, &mut broken, &config, &deep.join("j.jsonl")).await;
        assert!(broken.take_note().is_none(), "shown once, by connect");
        assert!(app.entries_text().contains("Remote control is off: "));
        assert!(app.remote_off.is_some());
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

    #[test]
    fn a_huge_approval_is_clipped_so_the_state_line_fits() {
        let (mut app, _) = demo();
        crate::test_support::idle(&mut app);
        app.event(octet_core::Event::Approval {
            id: 4,
            detail: "d".into(),
            summary: octet_core::ApprovalSummary {
                command: Some("x".repeat(200_000)),
                files: (0..5_000)
                    .map(|n| format!("{n}-{}", "f".repeat(2_000)))
                    .collect(),
                cwd: Some("/w".repeat(5_000)),
                ..octet_core::ApprovalSummary::new("Edit 5000 files")
            },
        });
        let state = state_json(&app);
        let summary = &state["approvals"][0]["summary"];
        assert!(summary["command"].as_str().unwrap().ends_with('…'));
        assert!(summary["command"].as_str().unwrap().len() <= 4096);
        assert!(summary["cwd"].as_str().unwrap().len() <= 1024);
        let files = summary["files"].as_array().unwrap();
        assert_eq!(files.len(), 21);
        assert_eq!(files[20], "+4980 more");
        assert!(files[..20].iter().all(|f| f.as_str().unwrap().len() <= 512));
        conforms("StateInfo", &state);
        let line = state_line(&state);
        assert!(line.len() <= MAX_MESSAGE, "{}", line.len());
        assert!(line.contains("\"id\":4") && line.contains("Edit 5000 files"));
    }

    #[test]
    fn a_state_that_still_does_not_fit_is_reduced() {
        let (mut app, _) = demo();
        crate::test_support::idle(&mut app);
        for id in 0..40 {
            app.event(octet_core::Event::Approval {
                id,
                detail: String::new(),
                summary: octet_core::ApprovalSummary {
                    command: Some("c".repeat(4_000)),
                    ..octet_core::ApprovalSummary::new("Run a shell command")
                },
            });
        }
        let state = state_json(&app);
        assert!(frame_line(&protocol::bridge_state_frame(&state)).len() > MAX_MESSAGE);
        let line = state_line(&state);
        assert!(line.len() <= MAX_MESSAGE);
        let reduced: Value = serde_json::from_str(&line).unwrap();
        let approvals = reduced["state"]["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), 40);
        assert_eq!(approvals[3]["summary"]["action"], "Run a shell command");
        assert!(approvals[3]["summary"].get("command").is_none());
        conforms("StateInfo", &reduced["state"]);
        conforms("ApprovalSummary", &approvals[3]["summary"]);
    }

    #[tokio::test]
    async fn a_dropped_bridge_leaves_no_socket_or_registration() {
        let temp = octet_testkit::TempDir::new("octet-bridge-drop");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
        let (app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let run = run_dir(&dir);
        let reg = registrations(&run).await.remove(0);
        assert!(reg.socket.exists());
        drop(bridge);
        assert!(!reg.socket.exists());
        assert!(registrations(&run).await.is_empty());
    }

    #[test]
    fn the_catalog_key_follows_what_the_catalog_lists() {
        let (mut app, config) = demo();
        app.connection(&config, "journal".into());
        let before = catalog_key(&app);
        assert_eq!(before, catalog_key(&app));
        app.conn.models = vec![octet_core::ModelInfo {
            selection: "default".into(),
            name: "Demo".into(),
            id: None,
            description: String::new(),
        }];
        let listed = catalog_key(&app);
        assert_ne!(before, listed);
        app.conn.mode = octet_core::Mode::AcceptEdits;
        assert_ne!(listed, catalog_key(&app));
    }

    #[tokio::test]
    async fn publish_sends_the_catalog_again_when_the_models_change() {
        let temp = octet_testkit::TempDir::new("octet-bridge-catalog");
        let dir = temp.path().to_path_buf();
        let mut bridge = Bridge::start(&dir, true, None).await;
        let (mut app, config) = demo();
        bridge.connected(&app, &config, &dir.join("j.jsonl")).await;
        let regs = registrations(&run_dir(&dir)).await;
        let (mut lines, _write) = connect(&regs[0].socket).await;
        let _ = first_line(&mut bridge, &app, &mut lines).await;
        let _ = next_line(&mut lines).await;
        app.conn.models = vec![octet_core::ModelInfo {
            selection: "fast".into(),
            name: "Fast".into(),
            id: Some("fast-1".into()),
            description: String::new(),
        }];
        bridge.publish(&app);
        let sent: Value = serde_json::from_str(&next_line(&mut lines).await).unwrap();
        assert_eq!(sent["type"], "catalog");
        assert_eq!(sent["catalog"]["models"][0]["selection"], "fast");
        bridge.stop().await;
    }

    /// Checks `value` against the protocol's shape `name`: required fields present, no others,
    /// and a closed set's values from that set.
    fn conforms(name: &str, value: &Value) {
        let shape = protocol::OBJECTS.iter().find(|s| s.name == name).unwrap();
        let map = value.as_object().unwrap();
        for field in shape.fields {
            assert!(
                field.optional || map.get(field.name).is_some_and(|v| !v.is_null()),
                "{name} lacks {}",
                field.name
            );
            let choices = |set: &str| {
                protocol::CHOICES
                    .iter()
                    .find(|c| c.name == set)
                    .unwrap()
                    .values
            };
            match (field.ty, map.get(field.name)) {
                (protocol::Ty::Choice(set), Some(Value::String(text))) => {
                    assert!(
                        choices(set).contains(&text.as_str()),
                        "{name}.{}",
                        field.name
                    );
                }
                (protocol::Ty::ChoiceList(set), Some(Value::Array(list))) => {
                    for item in list {
                        let text = item.as_str().unwrap_or_default();
                        assert!(choices(set).contains(&text), "{name}.{}", field.name);
                    }
                }
                _ => {}
            }
        }
        for key in map.keys() {
            assert!(
                shape.fields.iter().any(|f| f.name == key),
                "{name} has an unknown {key}"
            );
        }
    }
}
