//! A hub's world in a test: an in-process relay, a phone that reaches the hub through it the way
//! the app does (the Noise `IK` initiator, then frames in chunks, answering the hub's encrypted
//! pings), the pairing link it scans, and the hub's control socket, where the owner's answer to
//! a held pairing is given. Nothing here is hardened: a step that fails panics, as a test's
//! failure report.
use futures_util::{SinkExt, StreamExt};
use octet_relay::{Config, Limits, serve};
use serde_json::{Value, json};
use snow::{
    Builder, TransportState,
    params::DHChoice,
    resolvers::{CryptoResolver, DefaultResolver},
};
use std::{fmt::Write as _, future::poll_fn, net::SocketAddr, path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

/// How long any one step of a phone or the control socket may take before the test fails.
pub const WAIT: Duration = Duration::from_secs(10);
/// How long [`Phone::quiet`] listens for a frame.
const QUIET: Duration = Duration::from_millis(400);
/// The Noise protocol the phone and the hub speak.
const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
/// Hashed into the handshake on both sides.
const PROLOGUE: &[u8] = b"octet-link-v1";
/// The hub's handshake answer, in bytes.
const ANSWER: usize = 48;
/// The most bytes of a frame one chunk carries.
const CHUNK: usize = 65_000;
/// Type byte: a chunk with more to follow.
const MORE: u8 = 0;
/// Type byte: a frame's last chunk.
const LAST: u8 = 1;
/// Type byte: a ping.
const PING: u8 = 2;
/// Type byte: a pong.
const PONG: u8 = 3;
/// What encryption adds to a plaintext.
const TAG: usize = 16;

/// A WebSocket a phone talks over: one it opened, or one a relay played by a test accepted (on a
/// plain stream).
pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The relay's default limits, less those a test that is not about them would trip: the
/// per-address ones (every test connects from `127.0.0.1`) and the byte rate (a test may replay
/// megabytes).
pub fn roomy() -> Limits {
    Limits {
        connects_per_minute: 1_000_000,
        registrations_per_minute: 1_000_000,
        bytes_per_second: 1 << 40,
        burst_bytes: 1 << 40,
        ..Limits::default()
    }
}

/// A relay serving on loopback in this process until it is stopped or dropped.
#[derive(Debug)]
pub struct Relay {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    served: Option<JoinHandle<()>>,
}

impl Relay {
    /// A relay with [`roomy`] limits on a free port.
    pub async fn start() -> Self {
        Self::start_with(roomy()).await
    }

    /// A relay with `limits` on a free port.
    ///
    /// # Panics
    ///
    /// If no loopback port can be bound.
    pub async fn start_with(limits: Limits) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        Self::serve_on(listener, limits)
    }

    /// A relay with `limits` on `addr`, as one restarted where it was: `None` while the port is
    /// still taken.
    pub async fn start_at(addr: SocketAddr, limits: Limits) -> Option<Self> {
        let listener = TcpListener::bind(addr).await.ok()?;
        Some(Self::serve_on(listener, limits))
    }

    /// Serves `listener`, logging nowhere.
    fn serve_on(listener: TcpListener, limits: Limits) -> Self {
        let addr = listener.local_addr().expect("the relay's address");
        let (stop, stopped) = oneshot::channel::<()>();
        let config = Config {
            limits,
            log: octet_relay::Log::new(|_| {}),
            ..Config::default()
        };
        let served = tokio::spawn(async move {
            let _ = serve(listener, config, async {
                let _ = stopped.await;
            })
            .await;
        });
        Self {
            addr,
            stop: Some(stop),
            served: Some(served),
        }
    }

    /// Its address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Its URL, `ws://127.0.0.1:<port>`, as a hub dials it.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Stops it and waits until every connection it held is gone and its port is free.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(served) = self.served.take() {
            let _ = served.await;
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// A phone's static X25519 key pair: what the app makes at pairing.
#[derive(Clone)]
pub struct PhoneKey {
    private: [u8; 32],
    public: [u8; 32],
}

impl std::fmt::Debug for PhoneKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhoneKey")
            .field("public", &hex(&self.public))
            .finish_non_exhaustive()
    }
}

impl Default for PhoneKey {
    fn default() -> Self {
        Self::new()
    }
}

impl PhoneKey {
    /// A fresh key pair.
    ///
    /// # Panics
    ///
    /// If the system has no random bytes or no X25519.
    pub fn new() -> Self {
        let mut private = [0; 32];
        getrandom::fill(&mut private).expect("random bytes");
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .expect("X25519");
        dh.set(&private);
        let public = <[u8; 32]>::try_from(dh.pubkey()).expect("a 32-byte key");
        Self { private, public }
    }

    /// The public key, as the hub keeps it: 64 lowercase hex digits.
    pub fn public_hex(&self) -> String {
        hex(&self.public)
    }
}

/// A pairing link, `octet://pair/<base64url>`, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Whether the relay is reached over `wss` (version byte 1) rather than `ws` (0).
    pub secure: bool,
    /// The hub's X25519 identity public key.
    pub identity: [u8; 32],
    /// The channel id, as 64 lowercase hex digits.
    pub channel: String,
    /// The one-time secret, as 64 lowercase hex digits.
    pub secret: String,
    /// The relay's `host` or `host:port`.
    pub authority: String,
    /// The opening it came from, as `pair_secret` numbered it, which `pair_wait` names; 0 for a
    /// link only parsed from its text.
    pub attempt: u64,
}

impl Link {
    /// The parts of `text`; `None` if it is not a pairing link.
    pub fn parse(text: &str) -> Option<Self> {
        let payload = unbase64url(text.strip_prefix("octet://pair/")?)?;
        let (&version, rest) = payload.split_first()?;
        let secure = match version {
            1 => true,
            0 => false,
            _ => return None,
        };
        let identity = <[u8; 32]>::try_from(rest.get(..32)?).ok()?;
        let channel = hex(rest.get(32..64)?);
        let secret = hex(rest.get(64..96)?);
        let authority = String::from_utf8(rest.get(96..)?.to_vec()).ok()?;
        (!authority.is_empty()).then_some(Self {
            secure,
            identity,
            channel,
            secret,
            authority,
            attempt: 0,
        })
    }

    /// The relay's URL, as the app dials it.
    pub fn relay(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{}", self.authority)
    }
}

/// The bytes of unpadded base64url `text`; `None` for any other character.
fn unbase64url(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut held) = (0_u32, 0_u32);
    for c in text.chars() {
        let value = u32::try_from(ALPHABET.find(c)?).ok()?;
        bits = (bits << 6 | value) & 0xff_ffff;
        held += 6;
        if held >= 8 {
            held -= 8;
            bytes.push(u8::try_from(bits >> held & 0xff).ok()?);
        }
    }
    Some(bytes)
}

/// One request on the hub's control socket in the run directory `run`, and its answer; `None`
/// while no hub answers there.
pub async fn control(run: &Path, request: &Value) -> Option<Value> {
    let ask = async {
        let stream = UnixStream::connect(run.join("hub.sock")).await.ok()?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.ok()?;
        serde_json::from_str(&line).ok()
    };
    timeout(WAIT, ask).await.ok().flatten()
}

/// Waits until the hub whose run directory is `run` answers `status` with `relay` in `state`
/// (`off`, `connecting` or `registered`), and returns that answer.
///
/// # Panics
///
/// If it is not so within [`WAIT`].
pub async fn relay_state(run: &Path, state: &str) -> Value {
    timeout(WAIT, async {
        loop {
            if let Some(status) = control(run, &json!({"type": "status"})).await
                && status["relay"] == state
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the hub's relay is never {state}"))
}

/// A pairing link from the hub whose run directory is `run`, taken apart.
///
/// # Panics
///
/// If the hub gives none.
pub async fn pair_link(run: &Path) -> Link {
    let answer = control(run, &json!({"type": "pair_secret"}))
        .await
        .expect("the hub answers pair_secret");
    let link = answer["link"]
        .as_str()
        .unwrap_or_else(|| panic!("{answer}"));
    let attempt = answer["attempt"]
        .as_u64()
        .unwrap_or_else(|| panic!("no attempt: {answer}"));
    let link = Link::parse(link).unwrap_or_else(|| panic!("not a pairing link: {link}"));
    Link { attempt, ..link }
}

/// Asks the hub whose run directory is `run` for a pairing secret without waiting for its answer
/// (which comes once the relay has the hub): the pairing is open, and the hub dials, as soon as
/// the hub has read the request. The task ends with the answer.
pub fn open_pairing(run: &Path) -> JoinHandle<Option<Value>> {
    let run = run.to_owned();
    tokio::spawn(async move { control(&run, &json!({"type": "pair_secret"})).await })
}

/// The `pair` frame a phone called `name` sends with `secret`, as `req` 1.
pub fn pair_frame(secret: &str, name: &str) -> Value {
    json!({"v": 1, "req": 1, "type": "pair", "secret": secret, "name": name, "platform": "ios"})
}

/// The answer to `pair_wait` for the opening `attempt` on the hub whose run directory is `run`.
///
/// # Panics
///
/// If the hub gives none within [`WAIT`].
pub async fn pair_wait(run: &Path, attempt: u64) -> Value {
    control(run, &json!({"type": "pair_wait", "attempt": attempt}))
        .await
        .expect("the hub answers pair_wait")
}

/// The answer to `pair_answer` for the pairing `pending` on the hub whose run directory is `run`.
///
/// # Panics
///
/// If the hub gives none within [`WAIT`].
pub async fn pair_answer(run: &Path, pending: u64, accept: bool) -> Value {
    let request = json!({"type": "pair_answer", "pending": pending, "accept": accept});
    control(run, &request)
        .await
        .expect("the hub answers pair_answer")
}

/// A phone the hub holds for the owner's answer.
#[derive(Debug)]
pub struct Held {
    /// The link it scanned.
    pub link: Link,
    /// Its key.
    pub key: PhoneKey,
    /// Its connection, past the `pair_code` frame.
    pub phone: Phone,
    /// The `pair_code` frame it was sent.
    pub code: Value,
}

/// A phone called `name` that sent `pair` to the hub whose run directory is `run` with a new
/// pairing secret, once its relay has it, and was sent its code: the hub holds it for the
/// owner's answer.
///
/// # Panics
///
/// If any step fails.
pub async fn hold(run: &Path, name: &str) -> Held {
    let link = pair_link(run).await;
    hold_with(&link, name).await
}

/// A phone called `name` that sent `pair` with `link`'s secret and was sent its code.
///
/// # Panics
///
/// If any step fails.
pub async fn hold_with(link: &Link, name: &str) -> Held {
    let key = PhoneKey::new();
    let mut phone = Phone::connect(link, &key)
        .await
        .expect("the phone connects");
    phone.send(&pair_frame(&link.secret, name)).await;
    let code = phone.recv().await;
    assert_eq!(code["type"], "pair_code", "{code}");
    Held {
        link: link.clone(),
        key,
        phone,
        code,
    }
}

/// A phone called `name` paired with the hub whose run directory is `run`, once its relay has
/// it, the owner saying yes at the Mac: the link it scanned, its key, and the `paired` frame.
///
/// # Panics
///
/// If any step fails.
pub async fn pair(run: &Path, name: &str) -> (Link, PhoneKey, Value) {
    let Held {
        link,
        key,
        mut phone,
        ..
    } = hold(run, name).await;
    let question = pair_wait(run, link.attempt).await;
    let pending = question["pending"]
        .as_u64()
        .unwrap_or_else(|| panic!("{question}"));
    let answer = pair_answer(run, pending, true).await;
    assert_eq!(answer, json!({"paired": true, "device": name}));
    let paired = phone.recv().await;
    assert_eq!(paired["type"], "paired", "{paired}");
    (link, key, paired)
}

/// A phone's connection to a hub through the relay, past the handshake.
pub struct Phone {
    ws: Box<Socket>,
    noise: Box<TransportState>,
    /// The handshake's first message, as it sent it.
    first: Vec<u8>,
    /// The chunks of the frame being joined.
    joining: Vec<u8>,
    /// The bytes of a hub's ping whose pong is not sent yet.
    owed: Option<[u8; 8]>,
    /// Whether it answers the hub's pings.
    answers: bool,
    /// The hub's pings so far.
    pings: usize,
    /// Whether the channel is over.
    closed: bool,
}

impl std::fmt::Debug for Phone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phone")
            .field("pings", &self.pings)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

/// Why a phone did not get past its handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The relay or the hub closed the socket with this code.
    Closed(u16),
    /// The socket ended otherwise, or the upgrade failed.
    Failed(String),
}

impl Phone {
    /// Connects to the hub `link` names, as the phone whose key is `key`, and runs the handshake.
    ///
    /// # Errors
    ///
    /// How it ended before the hub answered.
    pub async fn connect(link: &Link, key: &PhoneKey) -> Result<Self, Refused> {
        Self::connect_to(&link.relay(), &link.channel, &link.identity, key).await
    }

    /// Connects to the hub with `identity` on `channel` through the relay at `relay`.
    ///
    /// # Errors
    ///
    /// How it ended before the hub answered.
    pub async fn connect_to(
        relay: &str,
        channel: &str,
        identity: &[u8; 32],
        key: &PhoneKey,
    ) -> Result<Self, Refused> {
        let url = format!("{relay}/v1/phone/{channel}");
        let (ws, _) = timeout(WAIT, Box::pin(connect_async(url)))
            .await
            .map_err(|_| Refused::Failed("the upgrade took too long".into()))?
            .map_err(|error| Refused::Failed(error.to_string()))?;
        Self::over(ws, identity, key).await
    }

    /// Runs the handshake with the hub with `identity` over `ws`, as the phone whose key is
    /// `key`: a socket the phone opened, or the hub's data connection to a relay played by the
    /// test.
    ///
    /// # Errors
    ///
    /// How it ended before the hub answered.
    ///
    /// # Panics
    ///
    /// If the Noise pattern cannot be built.
    pub async fn over(ws: Socket, identity: &[u8; 32], key: &PhoneKey) -> Result<Self, Refused> {
        // Boxed, as is the handshake: a TLS stream and Noise's state are large to carry around.
        let mut ws = Box::new(ws);
        let mut noise = Builder::new(PATTERN.parse().expect("the pattern"))
            .local_private_key(&key.private)
            .and_then(|b| b.remote_public_key(identity))
            .and_then(|b| b.prologue(PROLOGUE))
            .and_then(Builder::build_initiator)
            .map(Box::new)
            .expect("the initiator");
        let mut first = vec![0; 128];
        let len = noise.write_message(&[], &mut first).expect("message 1");
        first.truncate(len);
        // A relay with no hub may have closed already; its close says why.
        let _ = ws.send(Message::binary(first.clone())).await;
        let answer = timeout(WAIT, async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Binary(bytes))) => return Ok(bytes),
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(Some(frame)))) => {
                        return Err(Refused::Closed(u16::from(frame.code)));
                    }
                    other => return Err(Refused::Failed(format!("{other:?}"))),
                }
            }
        })
        .await
        .map_err(|_| Refused::Failed("no answer to the handshake".into()))??;
        if answer.len() != ANSWER {
            return Err(Refused::Failed(format!("an answer of {}", answer.len())));
        }
        let mut payload = [0; ANSWER];
        noise
            .read_message(&answer, &mut payload)
            .map_err(|error| Refused::Failed(error.to_string()))?;
        let noise = noise
            .into_transport_mode()
            .map(Box::new)
            .map_err(|error| Refused::Failed(error.to_string()))?;
        Ok(Self {
            ws,
            noise,
            first,
            joining: Vec::new(),
            owed: None,
            answers: true,
            pings: 0,
            closed: false,
        })
    }

    /// From now on the hub's pings go unanswered (the socket is still read, so WebSocket pings
    /// are).
    pub fn ignore_pings(&mut self) {
        self.answers = false;
    }

    /// How many pings the hub has sent.
    pub fn pings(&self) -> usize {
        self.pings
    }

    /// Sends `frame` as JSON.
    pub async fn send(&mut self, frame: &Value) {
        self.send_text(&frame.to_string()).await;
    }

    /// Sends `text` as one frame, in chunks.
    ///
    /// # Panics
    ///
    /// If a chunk cannot be encrypted or sent in time.
    pub async fn send_text(&mut self, text: &str) {
        for message in self.seal_text(text) {
            timeout(WAIT, self.ws.send(Message::binary(message)))
                .await
                .expect("sent in time")
                .expect("sent");
        }
    }

    /// The handshake's first message, as the phone sent it: what a relay could copy and replay.
    pub fn first_message(&self) -> &[u8] {
        &self.first
    }

    /// The encrypted messages that would carry `frame`, one a chunk, made but not sent (the
    /// phone's next messages are then the ones after them): what a relay could copy and replay.
    pub fn seal_frame(&mut self, frame: &Value) -> Vec<Vec<u8>> {
        self.seal_text(&frame.to_string())
    }

    /// The encrypted messages that carry `text` as one frame, in chunks.
    fn seal_text(&mut self, text: &str) -> Vec<Vec<u8>> {
        let count = text.len().div_ceil(CHUNK);
        text.as_bytes()
            .chunks(CHUNK)
            .enumerate()
            .map(|(index, chunk)| {
                let kind = if index + 1 == count { LAST } else { MORE };
                let mut plain = Vec::with_capacity(chunk.len() + 1);
                plain.push(kind);
                plain.extend_from_slice(chunk);
                self.seal(&plain)
            })
            .collect()
    }

    /// Sends an encrypted message of `plain` exactly as given: a test of what the hub refuses.
    ///
    /// # Panics
    ///
    /// If it cannot be sent.
    pub async fn send_plain(&mut self, plain: &[u8]) {
        let message = self.seal(plain);
        timeout(WAIT, self.ws.send(Message::binary(message)))
            .await
            .expect("sent in time")
            .expect("sent");
    }

    /// `plain`, encrypted as the next message.
    fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut message = vec![0; plain.len() + TAG];
        let len = self
            .noise
            .write_message(plain, &mut message)
            .expect("encrypted");
        message.truncate(len);
        message
    }

    /// The next frame from the hub, answering its pings on the way; `None` once the channel is
    /// over. Cancel safe: dropped while it waits, it loses nothing.
    ///
    /// # Panics
    ///
    /// If the hub sends a frame that is not JSON, or a message the format does not allow.
    pub async fn next(&mut self) -> Option<Value> {
        loop {
            if self.closed {
                return None;
            }
            if let Some(bytes) = self.owed {
                if poll_fn(|cx| self.ws.poll_ready_unpin(cx)).await.is_err() {
                    self.closed = true;
                    continue;
                }
                self.owed = None;
                let mut plain = vec![PONG];
                plain.extend_from_slice(&bytes);
                let pong = self.seal(&plain);
                let sent = self.ws.start_send_unpin(Message::binary(pong)).is_ok()
                    && poll_fn(|cx| self.ws.poll_flush_unpin(cx)).await.is_ok();
                self.closed = !sent;
                continue;
            }
            let message = match self.ws.next().await {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                _ => {
                    self.closed = true;
                    continue;
                }
            };
            let mut plain = vec![0; message.len()];
            let Ok(len) = self.noise.read_message(&message, &mut plain) else {
                self.closed = true;
                continue;
            };
            plain.truncate(len);
            match plain.split_first() {
                Some((&MORE, chunk)) => self.joining.extend_from_slice(chunk),
                Some((&LAST, chunk)) => {
                    self.joining.extend_from_slice(chunk);
                    let frame = std::mem::take(&mut self.joining);
                    return Some(serde_json::from_slice(&frame).expect("a JSON frame"));
                }
                Some((&PING, bytes)) => {
                    self.pings += 1;
                    if self.answers {
                        self.owed = Some(<[u8; 8]>::try_from(bytes).expect("8 bytes"));
                    }
                }
                _ => panic!("the hub sent {plain:?}"),
            }
        }
    }

    /// The next frame, within [`WAIT`].
    ///
    /// # Panics
    ///
    /// If none comes or the channel ends.
    pub async fn recv(&mut self) -> Value {
        timeout(WAIT, self.next())
            .await
            .expect("a frame arrives")
            .expect("a frame, not the end")
    }

    /// The next frame of type `kind`, passing over others (such as `sessions`).
    pub async fn recv_kind(&mut self, kind: &str) -> Value {
        loop {
            let frame = self.recv().await;
            if frame["type"] == kind {
                return frame;
            }
        }
    }

    /// Whether the channel ends within [`WAIT`]; frames before the end are passed over.
    pub async fn closes(&mut self) -> bool {
        self.closes_within(WAIT).await
    }

    /// Whether the channel ends within `wait`; frames before the end are passed over.
    pub async fn closes_within(&mut self, wait: Duration) -> bool {
        timeout(wait, async { while self.next().await.is_some() {} })
            .await
            .is_ok()
    }

    /// Whether no frame arrives for a while (and the channel stays up).
    pub async fn quiet(&mut self) -> bool {
        timeout(QUIET, self.next()).await.is_err()
    }

    /// Closes the socket, as the app does in the background.
    pub async fn close(&mut self) {
        self.closed = true;
        let _ = timeout(WAIT, self.ws.close(None)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_comes_apart() {
        // Version 0, identity 7s, channel 1s, secret 2s, then "relay.example:8443".
        let mut payload = vec![0];
        payload.extend([7; 32]);
        payload.extend([1; 32]);
        payload.extend([2; 32]);
        payload.extend(b"relay.example:8443");
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut text = String::from("octet://pair/");
        for group in payload.chunks(3) {
            let mut word = [0_u8; 3];
            word[..group.len()].copy_from_slice(group);
            let bits = u32::from(word[0]) << 16 | u32::from(word[1]) << 8 | u32::from(word[2]);
            for at in 0..=group.len() {
                let sextet = (bits >> (18 - 6 * at)) & 63;
                text.push(char::from(alphabet[usize::try_from(sextet).unwrap()]));
            }
        }
        let link = Link::parse(&text).unwrap();
        assert!(!link.secure);
        assert_eq!(link.identity, [7; 32]);
        assert_eq!(link.channel, "01".repeat(32));
        assert_eq!(link.secret, "02".repeat(32));
        assert_eq!(link.relay(), "ws://relay.example:8443");
        assert_eq!(Link::parse("octet://pair/!"), None);
    }
}
