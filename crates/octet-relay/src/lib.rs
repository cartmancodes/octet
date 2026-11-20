//! The relay: it splices one phone WebSocket to one hub WebSocket, and never sees plaintext (the
//! hub and the phone run Noise end to end over the binary messages it passes on).
//!
//! # Paths
//!
//! - `GET /v1/hub/<channel>`: a hub's control connection. The relay sends `{"challenge": <32
//!   random bytes>}`; the hub answers within [`Limits::unspliced`] with `{"key": <Ed25519 public
//!   key>, "signature": <64 bytes>}`, the signature over `octet-relay-v1` and the challenge, and
//!   the key's SHA-256 must be the channel id. The relay answers `{"registered": true}` and holds
//!   the connection as that channel's hub: a second registration replaces it (closed with 4409).
//! - `GET /v1/phone/<channel>`: a phone. With no hub registered it is closed with 4404;
//!   otherwise the relay makes a connection id and sends the hub `{"connect": <id>}`.
//! - `GET /v1/hub/<channel>/<connection>`: the hub's data connection for that phone, opened
//!   within [`Limits::unspliced`] of the phone's arrival. The relay splices the two and forwards
//!   binary messages unchanged both ways until either side closes, then closes the other. An id
//!   is good once, and only while its phone waits: an unknown, used or expired one is 404.
//! - `GET /healthz`: `200 ok`. Any other path is 404, and the three `/v1` paths need a
//!   WebSocket upgrade (426 without one).
//!
//! Ids are lowercase hex: a channel id 64 characters, a connection id 32. Control messages are
//! JSON text of at most 1 KiB, and an unknown one is ignored; phone and data sockets carry binary
//! only.
//!
//! # Closing
//!
//! Close codes: 4401 a bad registration, 4404 no hub, 4408 not spliced or not answered in time
//! (a challenge, or two pings), 4409 replaced by a newer registration, 4429 a limit. A phone or
//! data socket whose registration ends is closed with 1001; a text message where binary belongs,
//! or binary on a control connection, with 1003; a message over its cap with 1009. Before the
//! upgrade: 404 for an unknown path or connection id, 403 for an `Origin` naming another host
//! than `Host`, 503 at the socket ceiling.
//!
//! # Limits
//!
//! See [`Limits`]. Every limit refuses the newcomer: none drops an established splice. Every
//! connection the relay reads from is held to [`Limits::bytes_per_second`], each message it sends
//! counting (control text, pings and pongs too); past the rate the relay reads later and drops
//! nothing. Nothing is buffered for a side that is not there: a phone is not read until its hub data connection
//! arrives, and a message for a side that has gone goes with the connection.
//!
//! # Logging
//!
//! Counts and errors only, through [`Config::log`]: never a channel id, a connection id or a
//! client address.
#![forbid(unsafe_code)]
mod limits;
mod probe;

pub use limits::Limits;
pub use probe::probe;

use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use limits::{Addresses, ByteRate};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt::{self, Write as _},
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::{
        Instant, Interval, MissedTickBehavior, interval_at, sleep, sleep_until, timeout, timeout_at,
    },
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Error as WsError, Message,
        handshake::derive_accept_key,
        protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode},
    },
};

/// What a hub signs ahead of the 32 challenge bytes.
pub(crate) const SIGNED_PREFIX: &[u8] = b"octet-relay-v1";
/// The largest control message, in bytes.
const CONTROL_BYTES: usize = 1024;
/// The largest HTTP request head the relay reads.
const MAX_HEAD: usize = 8 * 1024;
/// The most header lines a request may carry.
const MAX_HEADERS: usize = 64;
/// How long the closing handshake, or writing an HTTP answer, may take.
const CLOSE_WITHIN: Duration = Duration::from_secs(2);
/// Unanswered pings after which a connection is dropped.
const MISSED_PONGS: u32 = 2;
/// How long accepting waits after a failure (out of descriptors, say) before trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// How often addresses whose per-minute allowance is whole again are forgotten.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// How often the relay logs its counts.
const REPORT_EVERY: Duration = Duration::from_secs(300);
/// The longest client address kept as a rate-limit key.
const MAX_ADDRESS: usize = 64;

/// Close code: the registration's signature or key is wrong.
const BAD_REGISTRATION: u16 = 4401;
/// Close code: no hub is registered for the channel.
const NO_HUB: u16 = 4404;
/// Close code: not spliced, or not answered, in time.
const TOO_LATE: u16 = 4408;
/// Close code: a newer registration replaced this one.
const REPLACED: u16 = 4409;
/// Close code: a limit refused the connection.
const LIMIT: u16 = 4429;
/// Close code: going away (the registration a socket belonged to has ended).
const GOING_AWAY: u16 = 1001;
/// Close code: data of the wrong type (text where binary belongs, or the reverse).
const WRONG_TYPE: u16 = 1003;
/// Close code: a message over its cap.
const TOO_BIG: u16 = 1009;
/// Close code: the relay failed (no random bytes).
const RELAY_ERROR: u16 = 1011;

/// Where the relay writes its log lines: standard error unless a test (or an embedder) hands it
/// a sink of its own.
#[derive(Clone)]
pub struct Log(Arc<dyn Fn(&str) + Send + Sync>);

impl Log {
    /// A log that hands each line to `sink`.
    pub fn new(sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }
}

impl Default for Log {
    /// Standard error, one line each.
    fn default() -> Self {
        Self::new(|line| eprintln!("{line}"))
    }
}

impl fmt::Debug for Log {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Log")
    }
}

/// How the relay runs.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// What the relay allows.
    pub limits: Limits,
    /// The request header that carries the client's address behind a proxy (the last address,
    /// when it holds several). Unset, the socket's peer address is used. Set, the relay's port
    /// must be reachable only through the proxy, or anyone could write the header.
    pub client_address_header: Option<String>,
    /// Where log lines go.
    pub log: Log,
}

/// Serves the relay on `listener` until `shutdown` resolves, then drops every connection.
///
/// # Errors
///
/// When the listener's own address cannot be read; failures to accept are logged and retried.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let port = listener.local_addr()?.port();
    let shared = Arc::new(Shared::new(config));
    shared.log(&format!("octet-relay: listening on port {port}"));
    let mut tasks = JoinSet::new();
    let mut sweeps = every(SWEEP_EVERY);
    let mut reports = every(REPORT_EVERY);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    tasks.spawn(connection(Arc::clone(&shared), stream, peer));
                }
                Err(error) => {
                    shared.log(&format!("octet-relay: accept failed: {error}"));
                    sleep(ACCEPT_BACKOFF).await;
                }
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            _ = sweeps.tick() => shared.sweep(),
            _ = reports.tick() => shared.report(),
        }
    }
    tasks.shutdown().await;
    shared.report();
    shared.log("octet-relay: stopped");
    Ok(())
}

/// An interval whose first tick is `period` from now, and which runs late rather than in bursts.
fn every(period: Duration) -> Interval {
    let mut interval = interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// The lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// The `N` bytes that `text`, exactly `2 * N` lowercase hex digits, spells.
pub(crate) fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if !is_hex(text, N * 2) {
        return None;
    }
    let mut bytes = [0; N];
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

/// Whether `text` is exactly `length` lowercase hex digits.
fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// A WebSocket the relay accepted.
type Socket = WebSocketStream<TcpStream>;

/// The relay's state, shared by every connection.
struct Shared {
    limits: Limits,
    address_header: Option<String>,
    log: Log,
    state: Mutex<State>,
}

/// What the relay holds in memory.
struct State {
    /// WebSocket connections open.
    sockets: usize,
    /// The registered hubs, by channel id.
    channels: HashMap<String, Registration>,
    /// Phone sockets per client address.
    connects: Addresses,
    /// Hub control sockets per client address.
    registrations: Addresses,
    /// The number the last registration was given.
    last_registration: u64,
    /// What happened since the last report.
    counts: Counts,
}

/// One hub's registration of a channel.
struct Registration {
    /// Tells this registration from an earlier or later one of the same channel.
    number: u64,
    /// The connection ids its control connection is to announce.
    connects: mpsc::Sender<String>,
    /// Its phone sockets, waiting and spliced, by connection id.
    phones: HashMap<String, Phone>,
    /// Dropped with the registration, which tells its spliced sockets to close.
    ended: watch::Sender<()>,
}

/// A phone socket of a registration.
struct Phone {
    /// Where its hub data connection goes; taken by the first data connection that names it.
    hub: Option<oneshot::Sender<Upgraded>>,
}

/// A hub data connection on its way to its phone, with the place it holds.
struct Upgraded {
    socket: Socket,
    seat: Seat,
}

/// What happened since the last report, for the log.
#[derive(Debug, Default)]
struct Counts {
    registered: u64,
    replaced: u64,
    bad_registrations: u64,
    unanswered: u64,
    phones: u64,
    no_hub: u64,
    unspliced: u64,
    spliced: u64,
    limited: u64,
    full: u64,
    silent: u64,
    broken: u64,
}

impl fmt::Display for Counts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "registered {}, replaced {}, bad registrations {}, unanswered challenges {}, phones {}, \
             phones with no hub {}, unspliced {}, spliced {}, refused by a limit {}, refused at the \
             socket ceiling {}, silent {}, broken {}",
            self.registered,
            self.replaced,
            self.bad_registrations,
            self.unanswered,
            self.phones,
            self.no_hub,
            self.unspliced,
            self.spliced,
            self.limited,
            self.full,
            self.silent,
            self.broken,
        )
    }
}

impl Shared {
    fn new(config: Config) -> Self {
        let state = State {
            sockets: 0,
            channels: HashMap::new(),
            connects: Addresses::new(config.limits.connects_per_minute),
            registrations: Addresses::new(config.limits.registrations_per_minute),
            last_registration: 0,
            counts: Counts::default(),
        };
        Self {
            limits: config.limits,
            address_header: config.client_address_header,
            log: config.log,
            state: Mutex::new(state),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, line: &str) {
        (self.log.0)(line);
    }

    /// Adds one to the count `pick` names.
    fn count(&self, pick: impl FnOnce(&mut Counts) -> &mut u64) {
        *pick(&mut self.state().counts) += 1;
    }

    /// Forgets addresses whose allowance is whole again.
    fn sweep(&self) {
        let now = Instant::now();
        let mut state = self.state();
        state.connects.sweep(now);
        state.registrations.sweep(now);
    }

    /// Logs the counts, and starts them again.
    fn report(&self) {
        let line = {
            let mut state = self.state();
            let counts = std::mem::take(&mut state.counts);
            format!(
                "octet-relay: sockets {} of {}, channels {} of {}; since the last report: {counts}",
                state.sockets,
                self.limits.sockets,
                state.channels.len(),
                self.limits.channels,
            )
        };
        self.log(&line);
    }

    /// Whether `address` may open another phone socket (`registration` false) or control
    /// socket (true) this minute.
    fn admit(&self, address: &str, registration: bool) -> bool {
        let now = Instant::now();
        let mut state = self.state();
        let admitted = if registration {
            state.registrations.admit(address, now)
        } else {
            state.connects.admit(address, now)
        };
        if !admitted {
            state.counts.limited += 1;
        }
        admitted
    }

    /// Makes `channel` this hub's, replacing any registration it had: the guard that ends it,
    /// and where the connection ids to announce come from. `None` at the channel ceiling.
    fn register(self: &Arc<Self>, channel: &str) -> Option<(Held, mpsc::Receiver<String>)> {
        let (connects, ids) = mpsc::channel(self.limits.phones_per_channel.max(1));
        let (ended, _) = watch::channel(());
        let mut state = self.state();
        if !state.channels.contains_key(channel) && state.channels.len() >= self.limits.channels {
            state.counts.limited += 1;
            return None;
        }
        state.last_registration += 1;
        let number = state.last_registration;
        let registration = Registration {
            number,
            connects,
            phones: HashMap::new(),
            ended,
        };
        let replaced = state.channels.insert(channel.to_owned(), registration);
        state.counts.registered += 1;
        if replaced.is_some() {
            state.counts.replaced += 1;
        }
        drop(state);
        // Dropping the old registration ends its control connection (4409) and its sockets.
        drop(replaced);
        let held = Held {
            shared: Arc::clone(self),
            channel: channel.to_owned(),
            number,
        };
        Some((held, ids))
    }

    /// Takes a place for a phone on `channel` and asks its hub to connect: the place, where
    /// the hub data connection will come, and a receiver that ends with the registration. The
    /// close code to refuse the phone with otherwise.
    fn join(self: &Arc<Self>, channel: &str) -> Result<Joined, u16> {
        let mut id = [0; 16];
        let random = getrandom::fill(&mut id);
        let mut state = self.state();
        let State {
            channels, counts, ..
        } = &mut *state;
        let Some(registration) = channels.get_mut(channel) else {
            counts.no_hub += 1;
            return Err(NO_HUB);
        };
        if registration.phones.len() >= self.limits.phones_per_channel {
            counts.limited += 1;
            return Err(LIMIT);
        }
        if random.is_err() {
            return Err(RELAY_ERROR);
        }
        let id = hex(&id);
        if registration.phones.contains_key(&id)
            || registration.connects.try_send(id.clone()).is_err()
        {
            counts.limited += 1;
            return Err(LIMIT);
        }
        let (hub, arrived) = oneshot::channel();
        registration
            .phones
            .insert(id.clone(), Phone { hub: Some(hub) });
        counts.phones += 1;
        Ok(Joined {
            place: Place {
                shared: Arc::clone(self),
                channel: channel.to_owned(),
                number: registration.number,
                id,
            },
            arrived,
            ended: registration.ended.subscribe(),
        })
    }

    /// The way to the phone waiting under `id` on `channel`, taken so no other connection can
    /// have it. `None` when no phone waits there: the id is unknown, used, or expired.
    fn claim(&self, channel: &str, id: &str) -> Option<oneshot::Sender<Upgraded>> {
        let mut state = self.state();
        let phone = state.channels.get_mut(channel)?.phones.get_mut(id)?;
        phone.hub.take().filter(|hub| !hub.is_closed())
    }
}

/// A hub's hold on its channel; dropping it ends the registration (unless a newer one has
/// replaced it), which closes its phone and data sockets.
struct Held {
    shared: Arc<Shared>,
    channel: String,
    number: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut state = self.shared.state();
        let ours = state
            .channels
            .get(&self.channel)
            .is_some_and(|held| held.number == self.number);
        let ended = if ours {
            state.channels.remove(&self.channel)
        } else {
            None
        };
        drop(state);
        drop(ended);
    }
}

/// A phone's place in its registration; dropping it gives the place back and makes its
/// connection id unknown.
struct Place {
    shared: Arc<Shared>,
    channel: String,
    number: u64,
    id: String,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut state = self.shared.state();
        let left = match state.channels.get_mut(&self.channel) {
            Some(registration) if registration.number == self.number => {
                registration.phones.remove(&self.id)
            }
            _ => None,
        };
        drop(state);
        drop(left);
    }
}

/// A phone that has a place: see [`Shared::join`].
struct Joined {
    place: Place,
    arrived: oneshot::Receiver<Upgraded>,
    ended: watch::Receiver<()>,
}

/// One open WebSocket, counted against [`Limits::sockets`] until dropped.
struct Seat(Arc<Shared>);

impl Seat {
    fn take(shared: &Arc<Shared>) -> Option<Self> {
        let mut state = shared.state();
        if state.sockets >= shared.limits.sockets {
            state.counts.full += 1;
            return None;
        }
        state.sockets += 1;
        Some(Self(Arc::clone(shared)))
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.sockets = state.sockets.saturating_sub(1);
    }
}

/// What a request asks for.
enum Target {
    /// `/healthz`.
    Health,
    /// One of the three `/v1` WebSocket paths.
    Socket(Route),
}

/// A WebSocket path.
enum Route {
    /// A hub's control connection for a channel.
    Hub(String),
    /// A phone for a channel.
    Phone(String),
    /// A hub's data connection for a channel and connection id.
    Data(String, String),
}

/// The target `path` names, if it names one.
fn route(path: &str) -> Option<Target> {
    if path == "/healthz" {
        return Some(Target::Health);
    }
    let parts: Vec<&str> = path.strip_prefix("/v1/")?.split('/').collect();
    let route = match parts.as_slice() {
        ["hub", channel] if is_hex(channel, 64) => Route::Hub((*channel).to_owned()),
        ["phone", channel] if is_hex(channel, 64) => Route::Phone((*channel).to_owned()),
        ["hub", channel, id] if is_hex(channel, 64) && is_hex(id, 32) => {
            Route::Data((*channel).to_owned(), (*id).to_owned())
        }
        _ => return None,
    };
    Some(Target::Socket(route))
}

/// What to do with a request, decided from its head alone.
enum Decision {
    /// Answer with this status and close.
    Answer(u16),
    /// Answer `/healthz`, with no body for `HEAD`.
    Health { head_only: bool },
    /// Upgrade to a WebSocket for `route`, `key` being the client's `Sec-WebSocket-Key`, and
    /// count it under `address`.
    Upgrade {
        route: Route,
        key: String,
        address: String,
    },
}

/// The result of reading a request head.
enum Head {
    /// The head, up to and including its blank line (and anything that came with it).
    Complete(Vec<u8>),
    /// No blank line within [`MAX_HEAD`] bytes.
    TooLarge,
    /// The connection ended or failed first.
    Gone,
}

/// A WebSocket connection once its route has been checked.
enum Upgrading {
    Hub(String),
    Phone(String),
    /// A data connection, and the way to the phone it claimed.
    Data(oneshot::Sender<Upgraded>),
}

/// One accepted TCP connection, from its HTTP request to its end.
async fn connection(shared: Arc<Shared>, mut stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let head = match timeout(shared.limits.unspliced, read_head(&mut stream)).await {
        Ok(Head::Complete(head)) => head,
        Ok(Head::TooLarge) => return respond(stream, 400, false).await,
        Ok(Head::Gone) | Err(_) => return,
    };
    let (route, key, address) = match decide(&head, peer, shared.address_header.as_deref()) {
        Decision::Answer(status) => return respond(stream, status, false).await,
        Decision::Health { head_only } => return respond(stream, 200, head_only).await,
        Decision::Upgrade {
            route,
            key,
            address,
        } => (route, key, address),
    };
    let Some(seat) = Seat::take(&shared) else {
        return respond(stream, 503, false).await;
    };
    let upgrading = match route {
        Route::Hub(channel) => Upgrading::Hub(channel),
        Route::Phone(channel) => Upgrading::Phone(channel),
        Route::Data(channel, id) => match shared.claim(&channel, &id) {
            Some(hub) => Upgrading::Data(hub),
            None => return respond(stream, 404, false).await,
        },
    };
    let Some(socket) = upgrade(stream, &key, &shared.limits).await else {
        return;
    };
    match upgrading {
        Upgrading::Hub(channel) => control(&shared, socket, &channel, &address).await,
        Upgrading::Phone(channel) => phone(&shared, socket, &channel, &address).await,
        Upgrading::Data(hub) => {
            if let Err(gone) = hub.send(Upgraded { socket, seat }) {
                close(gone.socket, GOING_AWAY, "the phone went").await;
            }
            return;
        }
    }
    drop(seat);
}

/// Reads an HTTP request head, up to its blank line.
async fn read_head(stream: &mut TcpStream) -> Head {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0; 1024];
    loop {
        let read = match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Head::Gone,
            Ok(read) => read,
        };
        let searched = head.len().saturating_sub(3);
        head.extend_from_slice(&chunk[..read]);
        if head[searched..]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            return Head::Complete(head);
        }
        if head.len() > MAX_HEAD {
            return Head::TooLarge;
        }
    }
}

/// What to do with the request whose head is `head`, from `peer`; `address_header` names the
/// header that carries the client's address, if one does.
fn decide(head: &[u8], peer: SocketAddr, address_header: Option<&str>) -> Decision {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    // A head with bytes after its blank line came with data before the upgrade: refused.
    if !matches!(request.parse(head), Ok(httparse::Status::Complete(length)) if length == head.len())
    {
        return Decision::Answer(400);
    }
    let (Some(method), Some(path)) = (request.method, request.path) else {
        return Decision::Answer(400);
    };
    let headers = &*request.headers;
    let Some(target) = route(path) else {
        return Decision::Answer(404);
    };
    let route = match target {
        Target::Health => {
            return match method {
                "GET" => Decision::Health { head_only: false },
                "HEAD" => Decision::Health { head_only: true },
                _ => Decision::Answer(405),
            };
        }
        Target::Socket(route) => route,
    };
    if method != "GET" {
        return Decision::Answer(405);
    }
    let Some(key) = websocket_key(headers) else {
        return Decision::Answer(426);
    };
    if foreign_origin(headers) {
        return Decision::Answer(403);
    }
    let address = client_address(headers, peer, address_header);
    Decision::Upgrade {
        route,
        key,
        address,
    }
}

/// The values of the headers called `name` (any case), in order, that are text.
fn values<'a>(
    headers: &'a [httparse::Header<'a>],
    name: &'a str,
) -> impl DoubleEndedIterator<Item = &'a str> {
    headers
        .iter()
        .filter(move |header| header.name.eq_ignore_ascii_case(name))
        .filter_map(|header| std::str::from_utf8(header.value).ok())
}

/// The `Sec-WebSocket-Key` of a well-formed WebSocket upgrade request.
fn websocket_key(headers: &[httparse::Header<'_>]) -> Option<String> {
    let names = |name, token: &str| {
        values(headers, name).any(|value| {
            value
                .split(',')
                .any(|item| item.trim().eq_ignore_ascii_case(token))
        })
    };
    if !names("Upgrade", "websocket")
        || !names("Connection", "upgrade")
        || !names("Sec-WebSocket-Version", "13")
    {
        return None;
    }
    values(headers, "Sec-WebSocket-Key")
        .next()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
}

/// Whether a web page of another site may have opened this socket: an `Origin` is there and its
/// host (any case, any port) is not the host the request's `Host` names. A browser sends
/// `Origin` with every WebSocket and cannot set `Host`.
fn foreign_origin(headers: &[httparse::Header<'_>]) -> bool {
    let Some(origin) = headers
        .iter()
        .rev()
        .find(|header| header.name.eq_ignore_ascii_case("Origin"))
    else {
        return false;
    };
    let Some(origin) = std::str::from_utf8(origin.value)
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .map(|(_, rest)| host_of(rest.split('/').next().unwrap_or_default()))
    else {
        return true;
    };
    !values(headers, "Host")
        .next_back()
        .map(host_of)
        .is_some_and(|host| !host.is_empty() && host.eq_ignore_ascii_case(origin))
}

/// The host of `authority` (`host`, `host:port` or `[v6]:port`), without its port.
fn host_of(authority: &str) -> &str {
    let authority = authority.trim();
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
}

/// The address the per-address limits count this request under: the last address in the last
/// `address_header` when that is set and present, else the socket's peer.
fn client_address(
    headers: &[httparse::Header<'_>],
    peer: SocketAddr,
    address_header: Option<&str>,
) -> String {
    let named = address_header
        .and_then(|name| values(headers, name).next_back())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|address| !address.is_empty());
    let Some(named) = named else {
        return address_key(peer.ip());
    };
    match named.parse::<IpAddr>() {
        Ok(ip) => address_key(ip),
        Err(_) => match named.parse::<SocketAddr>() {
            Ok(socket) => address_key(socket.ip()),
            Err(_) => named.chars().take(MAX_ADDRESS).collect(),
        },
    }
}

/// The rate-limit key of `ip`: an IPv4 address as it is, and an IPv6 one by its /64, which is
/// what one subscriber is usually given.
fn address_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.to_string();
            }
            let [a, b, c, d, ..] = v6.segments();
            format!("{a:x}:{b:x}:{c:x}:{d:x}::/64")
        }
    }
}

/// Answers a request that is not upgraded with `status`, then closes.
async fn respond(mut stream: TcpStream, status: u16, head_only: bool) {
    let (reason, body) = match status {
        200 => ("OK", "ok"),
        400 => ("Bad Request", "bad request"),
        403 => ("Forbidden", "forbidden"),
        404 => ("Not Found", "not found"),
        405 => ("Method Not Allowed", "method not allowed"),
        426 => ("Upgrade Required", "a WebSocket upgrade is required"),
        _ => ("Service Unavailable", "too many connections"),
    };
    let upgrade = if status == 426 {
        "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n"
    } else {
        ""
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         {upgrade}Connection: close\r\n\r\n{}",
        body.len(),
        if head_only { "" } else { body },
    );
    let _ = timeout(CLOSE_WITHIN, async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    })
    .await;
}

/// Accepts the WebSocket upgrade whose key is `key`.
async fn upgrade(mut stream: TcpStream, key: &str, limits: &Limits) -> Option<Socket> {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        derive_accept_key(key.as_bytes()),
    );
    timeout(CLOSE_WITHIN, stream.write_all(response.as_bytes()))
        .await
        .ok()?
        .ok()?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.message_bytes))
        .max_frame_size(Some(limits.message_bytes));
    Some(WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await)
}

/// A close frame with `code` and `reason`.
fn frame(code: u16, reason: &str) -> CloseFrame {
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }
}

/// Closes `socket` with `code`, waiting a moment for the other side's answer.
async fn close(socket: Socket, code: u16, reason: &str) {
    close_with(socket, Some(frame(code, reason))).await;
}

/// Closes `socket` with `frame` (none: a close with no code), waiting a moment for the other
/// side's answer.
async fn close_with(mut socket: Socket, frame: Option<CloseFrame>) {
    let _ = timeout(CLOSE_WITHIN, async {
        if socket.close(frame).await.is_ok() {
            while let Some(Ok(_)) = socket.next().await {}
        }
    })
    .await;
}

/// Finishes a closing handshake the other side began: sends the answer tungstenite queued.
async fn finish(mut socket: Socket) {
    let _ = timeout(CLOSE_WITHIN, async {
        while let Some(Ok(_)) = socket.next().await {}
    })
    .await;
}

/// How long one write may take: as long as two pings may go unanswered.
fn write_within(limits: &Limits) -> Duration {
    limits.ping_every * MISSED_PONGS
}

/// Sends `message`, within [`write_within`]; whether it went.
async fn send(socket: &mut Socket, message: Message, limits: &Limits) -> bool {
    matches!(
        timeout(write_within(limits), socket.send(message)).await,
        Ok(Ok(()))
    )
}

/// What a message on a control connection is.
enum Heard {
    /// A registration attempt: a JSON object with a `key` or a `signature`.
    Registration(Value),
    /// A pong, which answers the last ping.
    Pong,
    /// Anything else that may be ignored: an unknown message, a ping.
    Other,
    /// The other side's close.
    Closed,
    /// A message that closes the connection with this code.
    Bad(u16),
}

fn hear(message: Message) -> Heard {
    match message {
        Message::Text(text) if text.len() > CONTROL_BYTES => Heard::Bad(TOO_BIG),
        Message::Text(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.get("key").is_some() || value.get("signature").is_some() => {
                Heard::Registration(value)
            }
            _ => Heard::Other,
        },
        Message::Binary(_) => Heard::Bad(WRONG_TYPE),
        Message::Pong(_) => Heard::Pong,
        Message::Ping(_) | Message::Frame(_) => Heard::Other,
        Message::Close(_) => Heard::Closed,
    }
}

/// Whether `answer` registers `channel`: its key hashes to the channel id, and its signature
/// over [`SIGNED_PREFIX`] and `challenge` is good.
fn verify(channel: &str, challenge: &[u8; 32], answer: &Value) -> bool {
    let key = answer
        .get("key")
        .and_then(Value::as_str)
        .and_then(unhex::<32>);
    let signature = answer
        .get("signature")
        .and_then(Value::as_str)
        .and_then(unhex::<64>);
    let (Some(key), Some(signature)) = (key, signature) else {
        return false;
    };
    if hex(&Sha256::digest(key)) != channel {
        return false;
    }
    let Ok(key) = VerifyingKey::from_bytes(&key) else {
        return false;
    };
    let signed = [SIGNED_PREFIX, challenge.as_slice()].concat();
    key.verify_strict(&signed, &Signature::from_bytes(&signature))
        .is_ok()
}

/// How a control connection ended, once registered.
enum ControlEnd {
    /// A newer registration replaced it.
    Replaced,
    /// It left two pings unanswered.
    Silent,
    /// It sent a message that closes it with this code.
    Bad(u16),
    /// The hub closed it.
    Closed,
    /// It broke, or a write to it stalled.
    Gone,
}

/// A hub's control connection to `channel` from `address`: the challenge, the registration,
/// then the `connect`s for its phones until it ends.
async fn control(shared: &Arc<Shared>, mut socket: Socket, channel: &str, address: &str) {
    let limits = &shared.limits;
    if !shared.admit(address, true) {
        return close(socket, LIMIT, "too many registrations").await;
    }
    let mut challenge = [0; 32];
    if let Err(error) = getrandom::fill(&mut challenge) {
        shared.log(&format!("octet-relay: no random bytes: {error}"));
        return close(socket, RELAY_ERROR, "").await;
    }
    let ask = json!({"challenge": hex(&challenge)}).to_string();
    if !send(&mut socket, Message::Text(ask.into()), limits).await {
        return;
    }
    let deadline = Instant::now() + limits.unspliced;
    let mut rate = ByteRate::new(limits, Instant::now());
    let answer = loop {
        let ready_at = rate.ready_at();
        let read = async {
            sleep_until(ready_at).await;
            socket.next().await
        };
        let message = match timeout_at(deadline, read).await {
            Err(_) => {
                shared.count(|counts| &mut counts.unanswered);
                return close(socket, TOO_LATE, "no answer in time").await;
            }
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(WsError::Capacity(_)))) => return close(socket, TOO_BIG, "").await,
            Ok(_) => return,
        };
        rate.take(message.len(), Instant::now());
        match hear(message) {
            Heard::Registration(answer) => break answer,
            Heard::Pong | Heard::Other => {}
            Heard::Closed => return finish(socket).await,
            Heard::Bad(code) => return close(socket, code, "").await,
        }
    };
    if !verify(channel, &challenge, &answer) {
        shared.count(|counts| &mut counts.bad_registrations);
        return close(socket, BAD_REGISTRATION, "bad registration").await;
    }
    let Some((held, ids)) = shared.register(channel) else {
        return close(socket, LIMIT, "too many channels").await;
    };
    let registered = json!({"registered": true}).to_string();
    let end = if send(&mut socket, Message::Text(registered.into()), limits).await {
        announce(limits, &mut socket, ids, rate).await
    } else {
        ControlEnd::Gone
    };
    // The registration ends before the closing handshake, so its phones go at once.
    drop(held);
    match end {
        ControlEnd::Replaced => close(socket, REPLACED, "replaced by a newer registration").await,
        ControlEnd::Silent => {
            shared.count(|counts| &mut counts.silent);
            close(socket, TOO_LATE, "no pong").await;
        }
        ControlEnd::Bad(code) => {
            shared.count(|counts| &mut counts.broken);
            close(socket, code, "").await;
        }
        ControlEnd::Closed => finish(socket).await,
        ControlEnd::Gone => {}
    }
}

/// A registered control connection: sends `{"connect": <id>}` for each phone, pings, and
/// ignores what the hub says until it ends, reading it no faster than `rate` allows.
async fn announce(
    limits: &Limits,
    socket: &mut Socket,
    mut ids: mpsc::Receiver<String>,
    mut rate: ByteRate,
) -> ControlEnd {
    let mut pings = every(limits.ping_every);
    let mut missed = 0;
    loop {
        let ready_at = rate.ready_at();
        let ready = ready_at <= Instant::now();
        tokio::select! {
            id = ids.recv() => {
                // The ids stop only when the registration is dropped, which a newer one does.
                let Some(id) = id else {
                    return ControlEnd::Replaced;
                };
                let connect = json!({"connect": id}).to_string();
                if !send(socket, Message::Text(connect.into()), limits).await {
                    return ControlEnd::Gone;
                }
            }
            _ = pings.tick() => {
                if missed >= MISSED_PONGS {
                    return ControlEnd::Silent;
                }
                missed += 1;
                if !send(socket, Message::Ping(Vec::new().into()), limits).await {
                    return ControlEnd::Gone;
                }
            }
            () = sleep_until(ready_at), if !ready => {}
            message = socket.next(), if ready => match message {
                Some(Ok(message)) => {
                    rate.take(message.len(), Instant::now());
                    match hear(message) {
                        Heard::Pong => missed = 0,
                        Heard::Registration(_) | Heard::Other => {}
                        Heard::Closed => return ControlEnd::Closed,
                        Heard::Bad(code) => return ControlEnd::Bad(code),
                    }
                }
                Some(Err(WsError::Capacity(_))) => return ControlEnd::Bad(TOO_BIG),
                Some(Err(_)) | None => return ControlEnd::Gone,
            },
        }
    }
}

/// A phone socket on `channel` from `address`: a place in the registration, a `connect` to the
/// hub, and, once the hub's data connection arrives, the splice. The phone is not read before
/// then, so nothing it sends is taken in for a hub that is not there yet.
async fn phone(shared: &Arc<Shared>, socket: Socket, channel: &str, address: &str) {
    if !shared.admit(address, false) {
        return close(socket, LIMIT, "too many connections").await;
    }
    let Joined {
        place,
        arrived,
        ended,
    } = match shared.join(channel) {
        Ok(joined) => joined,
        Err(NO_HUB) => return close(socket, NO_HUB, "no hub").await,
        Err(RELAY_ERROR) => {
            shared.log("octet-relay: no random bytes for a connection id");
            return close(socket, RELAY_ERROR, "").await;
        }
        Err(code) => return close(socket, code, "too many phones").await,
    };
    let arrived = tokio::select! {
        arrived = arrived => arrived.ok(),
        () = sleep(shared.limits.unspliced) => {
            drop(place);
            shared.count(|counts| &mut counts.unspliced);
            return close(socket, TOO_LATE, "the hub did not connect in time").await;
        }
    };
    let Some(Upgraded {
        socket: hub,
        seat: hub_seat,
    }) = arrived
    else {
        drop(place);
        return close(socket, GOING_AWAY, "the hub went").await;
    };
    shared.count(|counts| &mut counts.spliced);
    let mut sockets = [socket, hub];
    let end = pass(&shared.limits, &mut sockets, ended).await;
    drop(place);
    let [phone, hub] = sockets;
    let (this, other) = match end.side() {
        Some(HUB) => (hub, phone),
        _ => (phone, hub),
    };
    match end {
        SpliceEnd::Ended => {
            let went = "the hub went";
            tokio::join!(
                close(this, GOING_AWAY, went),
                close(other, GOING_AWAY, went)
            );
        }
        SpliceEnd::Closed(_, frame) => {
            tokio::join!(finish(this), close_with(other, frame));
        }
        SpliceEnd::Silent(_) => {
            shared.count(|counts| &mut counts.silent);
            tokio::join!(
                close(this, TOO_LATE, "no pong"),
                close(other, GOING_AWAY, "the other side went")
            );
        }
        SpliceEnd::Bad(_, code) => {
            shared.count(|counts| &mut counts.broken);
            tokio::join!(
                close(this, code, ""),
                close(other, GOING_AWAY, "the other side went")
            );
        }
        SpliceEnd::Gone(_) => {
            drop(this);
            close(other, GOING_AWAY, "the other side went").await;
        }
    }
    drop(hub_seat);
}

/// The phone's side of a splice, as an index into its sockets.
const PHONE: usize = 0;
/// The hub's side of a splice.
const HUB: usize = 1;

/// How a splice ended.
enum SpliceEnd {
    /// The registration ended.
    Ended,
    /// This side closed with this frame, which goes on to the other.
    Closed(usize, Option<CloseFrame>),
    /// This side left two pings unanswered.
    Silent(usize),
    /// This side sent something that closes it with this code.
    Bad(usize, u16),
    /// This side broke, went without a close, or stalled a write.
    Gone(usize),
}

impl SpliceEnd {
    /// The side that ended it, if one did.
    fn side(&self) -> Option<usize> {
        match self {
            Self::Ended => None,
            Self::Closed(side, _) | Self::Silent(side) | Self::Bad(side, _) | Self::Gone(side) => {
                Some(*side)
            }
        }
    }
}

/// Passes binary messages both ways between the phone and hub `sockets`, unchanged, reading
/// each side only as fast as its [`ByteRate`] allows (every message it sends counts), until one
/// side ends or `ended` (the registration) does.
async fn pass(
    limits: &Limits,
    sockets: &mut [Socket; 2],
    mut ended: watch::Receiver<()>,
) -> SpliceEnd {
    let start = Instant::now();
    let mut rates = [ByteRate::new(limits, start), ByteRate::new(limits, start)];
    let mut missed = [0; 2];
    let mut pings = every(limits.ping_every);
    loop {
        let now = Instant::now();
        let ready = [rates[PHONE].ready_at() <= now, rates[HUB].ready_at() <= now];
        let wake = rates
            .iter()
            .map(ByteRate::ready_at)
            .filter(|at| *at > now)
            .min();
        let [phone, hub] = &mut *sockets;
        let read = tokio::select! {
            // Nothing is ever sent on it: it ends when the registration is dropped.
            _ = ended.changed() => return SpliceEnd::Ended,
            _ = pings.tick() => None,
            message = phone.next(), if ready[PHONE] => Some((PHONE, message)),
            message = hub.next(), if ready[HUB] => Some((HUB, message)),
            () = sleep_until(wake.unwrap_or(now)), if wake.is_some() => continue,
        };
        let Some((side, message)) = read else {
            for side in [PHONE, HUB] {
                if missed[side] >= MISSED_PONGS {
                    return SpliceEnd::Silent(side);
                }
                missed[side] += 1;
                if !send(&mut sockets[side], Message::Ping(Vec::new().into()), limits).await {
                    return SpliceEnd::Gone(side);
                }
            }
            continue;
        };
        let other = 1 - side;
        // Every message counts, pings and pongs too, so control frames are no way round the rate.
        if let Some(Ok(message)) = &message {
            rates[side].take(message.len(), Instant::now());
        }
        match message {
            Some(Ok(Message::Binary(bytes))) => {
                if !send(&mut sockets[other], Message::Binary(bytes), limits).await {
                    return SpliceEnd::Gone(other);
                }
            }
            Some(Ok(Message::Text(_))) => return SpliceEnd::Bad(side, WRONG_TYPE),
            Some(Ok(Message::Pong(_))) => missed[side] = 0,
            Some(Ok(Message::Ping(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(frame))) => return SpliceEnd::Closed(side, frame),
            Some(Err(WsError::Capacity(_))) => return SpliceEnd::Bad(side, TOO_BIG),
            Some(Err(_)) | None => return SpliceEnd::Gone(side),
        }
    }
}
