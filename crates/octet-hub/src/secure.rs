//! The hub's end of the encrypted channel to a phone: a Noise `IK` responder over one WebSocket,
//! then frames in chunks and encrypted pings.
//!
//! The phone starts (it knows the hub's static key from the pairing link); the hub learns the
//! phone's static key, encrypted, from the first message, and that key is the [`Peer`]. Each
//! WebSocket binary message is one Noise message. The relay's WebSocket pings, pongs and closes
//! are left to tungstenite; a text message, a message that does not decrypt, or a plaintext the
//! format below does not allow ends the channel, and once ended it stays ended.
//!
//! A transport plaintext is one type byte and its body:
//! - [`MORE`] or [`LAST`]: 1 to [`CHUNK`] bytes of a UTF-8 frame; [`LAST`] ends the frame.
//! - [`PING`] or [`PONG`]: exactly 8 bytes; a pong carries its ping's bytes back.
//!
//! The hub refuses a frame over [`MAX_MESSAGE`], but sends frames of any size (the phone takes
//! up to 256 KiB). A ping or pong may arrive between the chunks of a phone's frame; the hub
//! never puts anything between the chunks of its own.

use futures_util::{SinkExt, StreamExt};
use octet_core::remote::protocol::MAX_MESSAGE;
use snow::{
    Builder, HandshakeState, TransportState,
    params::{CipherChoice, DHChoice, HashChoice, NoiseParams},
    resolvers::{CryptoResolver, DefaultResolver},
    types::{Cipher, Dh, Hash, Random},
};
use std::future::poll_fn;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

/// The Noise protocol name, both sides.
pub(crate) const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
/// Hashed into the handshake on both sides, so a later incompatible version fails it.
pub(crate) const PROLOGUE: &[u8] = b"octet-link-v1";
/// The largest Noise message, so the largest WebSocket message the channel can use; the
/// caller may cap its socket's message size here.
pub(crate) const MAX_NOISE: usize = 65_535;
/// The phone's handshake message: its ephemeral key, its static key encrypted, an empty payload.
const FIRST: usize = 96;
/// The hub's handshake message: its ephemeral key and an empty payload.
const SECOND: usize = 48;
/// What encryption adds to a plaintext.
const TAG: usize = 16;
/// The most bytes of a frame one chunk carries.
const CHUNK: usize = 65_000;
/// Type byte: a chunk with more to follow.
const MORE: u8 = 0;
/// Type byte: a frame's last chunk.
const LAST: u8 = 1;
/// Type byte: a ping of 8 bytes.
const PING: u8 = 2;
/// Type byte: a pong, carrying its ping's 8 bytes.
const PONG: u8 = 3;

/// The phone at the other end, as its handshake proved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Peer {
    /// The phone's static X25519 public key.
    pub key: [u8; 32],
}

/// What [`Secure::next`] read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Incoming {
    /// A whole frame, valid UTF-8, at most [`MAX_MESSAGE`] bytes.
    Text(String),
    /// The answer to the last ping sent.
    Pong,
    /// Nothing to hand on: a chunk that did not finish a frame, or a peer's ping, answered.
    Skip,
    /// The channel is over; every later call says so too.
    Closed,
}

/// The channel to one phone after its handshake: frames in, frames out, pings both ways.
pub(crate) struct Secure<S> {
    /// The WebSocket, through the relay.
    ws: WebSocketStream<S>,
    /// The keys and counters of both directions.
    noise: TransportState,
    /// The frame being joined, and the ping awaiting its pong.
    inbound: Inbound,
    /// The bytes of a peer's ping whose pong is not handed to the socket yet.
    owed: Option<[u8; 8]>,
    /// Set once the channel failed or closed, or while a frame's chunks are going out (so one
    /// cut short leaves it set): nothing more passes.
    dead: bool,
}

impl<S> std::fmt::Debug for Secure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secure")
            .field("dead", &self.dead)
            .finish_non_exhaustive()
    }
}

/// Answers a phone's handshake on `ws` as the hub whose X25519 private key is `identity`:
/// reads one first message of exactly 96 bytes (WebSocket pings and pongs aside), writes the
/// 48-byte answer, and returns the channel and the phone's key. `None` for anything else: a
/// text message, a close, another size, another prologue, a message that does not decrypt, an
/// all-zero shared secret. Nothing here bounds time; the caller does, and may drop the future.
/// The handshake's state is consumed into the channel's keys before this returns.
pub(crate) async fn respond<S>(
    mut ws: WebSocketStream<S>,
    identity: &[u8; 32],
) -> Option<(Secure<S>, Peer)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let noise = builder()?
        .local_private_key(identity)
        .ok()?
        .build_responder()
        .ok()?;
    let (noise, peer) = handshake(&mut ws, noise).await?;
    let secure = Secure {
        ws,
        noise,
        inbound: Inbound::default(),
        owed: None,
        dead: false,
    };
    Some((secure, peer))
}

/// The two handshake messages, read and written on `ws`; the keys after and the phone's key.
async fn handshake<S>(
    ws: &mut WebSocketStream<S>,
    mut noise: HandshakeState,
) -> Option<(TransportState, Peer)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let first = loop {
        match ws.next().await?.ok()? {
            Message::Binary(bytes) => break bytes,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Text(_) | Message::Close(_) => return None,
        }
    };
    if first.len() != FIRST {
        return None;
    }
    let mut payload = [0; FIRST];
    if noise.read_message(&first, &mut payload).ok()? != 0 {
        return None;
    }
    let key = <[u8; 32]>::try_from(noise.get_remote_static()?).ok()?;
    let mut second = [0; FIRST];
    let len = noise.write_message(&[], &mut second).ok()?;
    if len != SECOND {
        return None;
    }
    let second = second.get(..len)?.to_vec();
    ws.send(Message::binary(second)).await.ok()?;
    Some((noise.into_transport_mode().ok()?, Peer { key }))
}

impl<S: AsyncRead + AsyncWrite + Unpin> Secure<S> {
    /// Reads the next Noise message and says what it meant. A peer's ping is answered here.
    ///
    /// Cancel safe, so it can sit in a `select!`: dropped at any point, it loses no message
    /// and no part of a frame, and a pong it owed goes out at the next call.
    pub(crate) async fn next(&mut self) -> Incoming {
        loop {
            if self.dead || !self.answer().await {
                return self.fail();
            }
            let message = match self.ws.next().await {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Text(_) | Message::Close(_)) | Err(_)) | None => {
                    return self.fail();
                }
            };
            let Some(plain) = self.unseal(&message) else {
                return self.fail();
            };
            return match self.inbound.open(&plain) {
                Opened::Frame(text) => Incoming::Text(text),
                Opened::More => Incoming::Skip,
                Opened::Ping(bytes) => {
                    self.owed = Some(bytes);
                    if self.answer().await {
                        Incoming::Skip
                    } else {
                        self.fail()
                    }
                }
                Opened::Pong => Incoming::Pong,
                Opened::Refused => self.fail(),
            };
        }
    }

    /// Sends `frame` in chunks of at most [`CHUNK`] bytes, all of them before returning, so
    /// frames never interleave; false if the channel is over, the frame is empty (no chunk can
    /// carry it), or writing failed. Not cancel safe: dropped part way, it leaves the channel
    /// over.
    pub(crate) async fn send(&mut self, frame: &str) -> bool {
        if self.dead || frame.is_empty() || !self.answer().await {
            return false;
        }
        self.dead = true;
        for (kind, chunk) in chunks(frame.as_bytes()) {
            let mut plain = Vec::with_capacity(1 + chunk.len());
            plain.push(kind);
            plain.extend_from_slice(chunk);
            if !self.ready().await || !self.start(&plain) {
                return false;
            }
        }
        if !self.flush().await {
            return false;
        }
        self.dead = false;
        true
    }

    /// Sends an encrypted ping of 8 random bytes; [`Incoming::Pong`] is its answer. False if the
    /// channel is over, writing failed, or the last ping is still unanswered (the phone is
    /// gone, and the channel is to be closed).
    pub(crate) async fn ping(&mut self) -> bool {
        if self.dead || self.inbound.awaiting.is_some() || !self.answer().await {
            return false;
        }
        let mut bytes = [0; 8];
        if getrandom::fill(&mut bytes).is_err() {
            return false;
        }
        self.inbound.awaiting = Some(bytes);
        self.ready().await && self.start(&control(PING, bytes)) && self.flush().await
    }

    /// Closes the WebSocket and ends the channel. The caller bounds how long this waits.
    pub(crate) async fn close(&mut self) {
        self.dead = true;
        let _ = self.ws.close(None).await;
    }

    /// Ends the channel.
    fn fail(&mut self) -> Incoming {
        self.dead = true;
        Incoming::Closed
    }

    /// Hands the socket the pong owed to the peer's last ping, if one is; false if that failed.
    async fn answer(&mut self) -> bool {
        let Some(bytes) = self.owed else {
            return true;
        };
        if !self.ready().await {
            return false;
        }
        // Encrypted only once the socket takes it, so a drop before never spends a nonce.
        self.owed = None;
        self.start(&control(PONG, bytes)) && self.flush().await
    }

    /// Waits until the socket will take a message; false if it failed.
    async fn ready(&mut self) -> bool {
        let ready = poll_fn(|cx| self.ws.poll_ready_unpin(cx)).await.is_ok();
        self.dead |= !ready;
        ready
    }

    /// Encrypts `plain` and hands it to the socket, which is ready; false if that failed.
    fn start(&mut self, plain: &[u8]) -> bool {
        let mut message = vec![0; plain.len() + TAG];
        let started = match self.noise.write_message(plain, &mut message) {
            Ok(len) => {
                message.truncate(len);
                self.ws.start_send_unpin(Message::binary(message)).is_ok()
            }
            Err(_) => false,
        };
        self.dead |= !started;
        started
    }

    /// Writes out what the socket holds; false if that failed.
    async fn flush(&mut self) -> bool {
        let flushed = poll_fn(|cx| self.ws.poll_flush_unpin(cx)).await.is_ok();
        self.dead |= !flushed;
        flushed
    }

    /// The plaintext of one Noise message, or `None` if it does not decrypt as the next one.
    fn unseal(&mut self, message: &[u8]) -> Option<Vec<u8>> {
        if message.len() > MAX_NOISE {
            return None;
        }
        let mut plain = vec![0; message.len()];
        let len = self.noise.read_message(message, &mut plain).ok()?;
        plain.truncate(len);
        Some(plain)
    }
}

/// The hub's Noise builder: [`PATTERN`], [`PROLOGUE`], and snow's primitives with a
/// Diffie-Hellman that refuses an all-zero result.
fn builder<'a>() -> Option<Builder<'a>> {
    let params: NoiseParams = PATTERN.parse().ok()?;
    Builder::with_resolver(params, Box::new(Resolver))
        .prologue(PROLOGUE)
        .ok()
}

/// `frame` as chunks of at most [`CHUNK`] bytes, each with its type byte: [`MORE`], then
/// [`LAST`] for the last. Nothing for an empty frame.
fn chunks(frame: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let count = frame.len().div_ceil(CHUNK);
    frame
        .chunks(CHUNK)
        .enumerate()
        .map(move |(index, chunk)| (if index + 1 == count { LAST } else { MORE }, chunk))
}

/// The plaintext of a ping or pong.
fn control(kind: u8, bytes: [u8; 8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(9);
    plain.push(kind);
    plain.extend_from_slice(&bytes);
    plain
}

/// What the hub keeps between messages from a phone.
#[derive(Default)]
struct Inbound {
    /// The chunks of the frame being joined, never more than [`MAX_MESSAGE`] bytes.
    frame: Vec<u8>,
    /// The bytes of the last ping the hub sent, until its pong.
    awaiting: Option<[u8; 8]>,
}

/// What one decrypted message meant.
#[derive(Debug, PartialEq, Eq)]
enum Opened {
    /// The last chunk of a frame, which is whole.
    Frame(String),
    /// A chunk with more to follow.
    More,
    /// A ping, with its bytes.
    Ping([u8; 8]),
    /// The pong to the hub's last ping.
    Pong,
    /// Anything the format does not allow: the channel ends.
    Refused,
}

impl Inbound {
    /// Reads one plaintext. A frame is refused before it grows past [`MAX_MESSAGE`].
    fn open(&mut self, plain: &[u8]) -> Opened {
        match plain.split_first() {
            Some((&kind @ (MORE | LAST), chunk)) if (1..=CHUNK).contains(&chunk.len()) => {
                if self.frame.len() + chunk.len() > MAX_MESSAGE {
                    return Opened::Refused;
                }
                self.frame.extend_from_slice(chunk);
                if kind == MORE {
                    return Opened::More;
                }
                String::from_utf8(std::mem::take(&mut self.frame))
                    .map_or(Opened::Refused, Opened::Frame)
            }
            Some((&PING, bytes)) => {
                <[u8; 8]>::try_from(bytes).map_or(Opened::Refused, Opened::Ping)
            }
            Some((&PONG, bytes)) => match (<[u8; 8]>::try_from(bytes), self.awaiting) {
                (Ok(bytes), Some(sent)) if bytes == sent => {
                    self.awaiting = None;
                    Opened::Pong
                }
                _ => Opened::Refused,
            },
            _ => Opened::Refused,
        }
    }
}

/// snow's own primitives, but a Diffie-Hellman whose result is all zero (a peer key of low
/// order) fails the handshake: snow 0.10 does not check.
struct Resolver;

impl CryptoResolver for Resolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        DefaultResolver.resolve_rng()
    }

    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        let dh: Box<dyn Dh> = Box::new(NonZero(DefaultResolver.resolve_dh(choice)?));
        Some(dh)
    }

    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        DefaultResolver.resolve_hash(choice)
    }

    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        DefaultResolver.resolve_cipher(choice)
    }
}

/// A Diffie-Hellman that refuses an all-zero shared secret, around snow's own.
struct NonZero(Box<dyn Dh>);

impl Dh for NonZero {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn pub_len(&self) -> usize {
        self.0.pub_len()
    }

    fn priv_len(&self) -> usize {
        self.0.priv_len()
    }

    fn set(&mut self, privkey: &[u8]) {
        self.0.set(privkey);
    }

    fn generate(&mut self, rng: &mut dyn Random) -> Result<(), snow::Error> {
        self.0.generate(rng)
    }

    fn pubkey(&self) -> &[u8] {
        self.0.pubkey()
    }

    fn privkey(&self) -> &[u8] {
        self.0.privkey()
    }

    fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> Result<(), snow::Error> {
        self.0.dh(pubkey, out)?;
        let shared = out.get(..self.0.dh_len()).ok_or(snow::Error::Dh)?;
        // Every byte looked at, so the time taken does not depend on where a non-zero one is.
        if shared.iter().fold(0, |any, byte| any | byte) == 0 {
            return Err(snow::Error::Dh);
        }
        Ok(())
    }

    fn dh_len(&self) -> usize {
        self.0.dh_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::SinkExt;
    use octet_core::remote::protocol::MAX_MESSAGE;
    use snow::HandshakeState;
    use std::fmt::Write as _;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::tungstenite::{Message, protocol::Role};

    /// A fixed key: `seed`, then each byte `step` more than the last.
    const fn key(seed: u8, step: u8) -> [u8; 32] {
        let mut key = [0; 32];
        let mut byte = seed;
        let mut index = 0;
        while index < 32 {
            key[index] = byte;
            byte = byte.wrapping_add(step);
            index += 1;
        }
        key
    }

    /// The hub's identity in these tests and the vectors.
    const HUB: [u8; 32] = key(0x01, 0x07);
    /// The phone's static key.
    const PHONE: [u8; 32] = key(0x80, 0x0b);
    /// The hub's ephemeral key in the vectors.
    const HUB_EPHEMERAL: [u8; 32] = key(0x33, 0x05);
    /// The phone's ephemeral key in the vectors.
    const PHONE_EPHEMERAL: [u8; 32] = key(0xc4, 0x0d);
    /// The bytes of the hub's ping in the vectors.
    const HUB_PING: [u8; 8] = [0x68, 0x75, 0x62, 0x2d, 0x70, 0x69, 0x6e, 0x67];
    /// The bytes of the phone's ping in the vectors.
    const PHONE_PING: [u8; 8] = [0x70, 0x68, 0x6f, 0x6e, 0x65, 0x2d, 0x70, 0x69];
    /// Where the vectors live.
    const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/noise-vectors.json");

    /// The public key of `private`.
    fn public(private: &[u8; 32]) -> [u8; 32] {
        let mut dh = DefaultResolver.resolve_dh(&DHChoice::Curve25519).unwrap();
        dh.set(private);
        dh.pubkey().try_into().unwrap()
    }

    /// Lowercase hex of `bytes`.
    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// `kind` then `bytes`.
    fn plain(kind: u8, bytes: &[u8]) -> Vec<u8> {
        let mut plain = vec![kind];
        plain.extend_from_slice(bytes);
        plain
    }

    /// The phone's handshake, under `prologue`.
    fn initiator(prologue: &[u8]) -> HandshakeState {
        let hub = public(&HUB);
        Builder::new(PATTERN.parse().unwrap())
            .local_private_key(&PHONE)
            .unwrap()
            .remote_public_key(&hub)
            .unwrap()
            .prologue(prologue)
            .unwrap()
            .build_initiator()
            .unwrap()
    }

    /// The first handshake message of a phone under `prologue`, and its handshake.
    fn first(prologue: &[u8]) -> (Vec<u8>, HandshakeState) {
        let mut noise = initiator(prologue);
        let mut message = vec![0; 200];
        let len = noise.write_message(&[], &mut message).unwrap();
        message.truncate(len);
        assert_eq!(message.len(), 96);
        (message, noise)
    }

    /// Two WebSocket streams joined in memory: the hub's and the phone's.
    async fn sockets() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (hub, phone) = tokio::io::duplex(1 << 22);
        let hub = WebSocketStream::from_raw_socket(hub, Role::Server, None).await;
        let phone = WebSocketStream::from_raw_socket(phone, Role::Client, None).await;
        (hub, phone)
    }

    /// The next binary message on `ws`.
    async fn binary(ws: &mut WebSocketStream<DuplexStream>) -> Vec<u8> {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => return bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("not a binary message: {other:?}"),
            }
        }
    }

    /// What the hub makes of a first message of `bytes`.
    async fn answer_to(message: Message) -> Option<Peer> {
        let (hub, mut phone) = sockets().await;
        phone.send(message).await.unwrap();
        respond(hub, &HUB).await.map(|(_, peer)| peer)
    }

    /// A test phone past its handshake.
    struct Phone {
        /// Its socket.
        ws: WebSocketStream<DuplexStream>,
        /// Its keys.
        noise: TransportState,
    }

    impl Phone {
        /// `plain`, encrypted.
        fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
            let mut message = vec![0; plain.len() + 16];
            let len = self.noise.write_message(plain, &mut message).unwrap();
            message.truncate(len);
            message
        }

        /// Sends `plain`, encrypted.
        async fn put(&mut self, plain: &[u8]) {
            let message = self.seal(plain);
            self.raw(message).await;
        }

        /// Sends `bytes` as they are.
        async fn raw(&mut self, bytes: Vec<u8>) {
            self.ws.send(Message::binary(bytes)).await.unwrap();
        }

        /// The next message from the hub, decrypted.
        async fn take(&mut self) -> Vec<u8> {
            let message = binary(&mut self.ws).await;
            let mut plain = vec![0; message.len()];
            let len = self.noise.read_message(&message, &mut plain).unwrap();
            plain.truncate(len);
            plain
        }
    }

    /// A hub and a phone past a good handshake.
    async fn connect() -> (Secure<DuplexStream>, Phone) {
        let (hub, mut ws) = sockets().await;
        let (message, mut noise) = first(PROLOGUE);
        ws.send(Message::binary(message)).await.unwrap();
        let (secure, peer) = respond(hub, &HUB)
            .await
            .expect("the hub accepts a good handshake");
        assert_eq!(peer.key, public(&PHONE));
        let second = binary(&mut ws).await;
        assert_eq!(second.len(), 48);
        let mut payload = [0; 64];
        assert_eq!(noise.read_message(&second, &mut payload).unwrap(), 0);
        let noise = noise.into_transport_mode().unwrap();
        (secure, Phone { ws, noise })
    }

    /// Asserts the channel reads as closed, and stays so.
    async fn closes(secure: &mut Secure<DuplexStream>) {
        assert_eq!(secure.next().await, Incoming::Closed);
        assert_eq!(secure.next().await, Incoming::Closed);
    }

    /// The vectors file's text, made afresh with fixed keys.
    fn vectors() -> String {
        let hub_public = public(&HUB);
        let mut hub = builder()
            .unwrap()
            .local_private_key(&HUB)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&HUB_EPHEMERAL)
            .build_responder()
            .unwrap();
        let mut phone = Builder::new(PATTERN.parse().unwrap())
            .local_private_key(&PHONE)
            .unwrap()
            .remote_public_key(&hub_public)
            .unwrap()
            .prologue(PROLOGUE)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&PHONE_EPHEMERAL)
            .build_initiator()
            .unwrap();
        let mut scratch = vec![0; 200];
        let len = phone.write_message(&[], &mut scratch).unwrap();
        let message1 = scratch[..len].to_vec();
        assert_eq!(message1.len(), 96);
        assert_eq!(message1[..32], public(&PHONE_EPHEMERAL));
        assert_eq!(hub.read_message(&message1, &mut scratch).unwrap(), 0);
        let len = hub.write_message(&[], &mut scratch).unwrap();
        let message2 = scratch[..len].to_vec();
        assert_eq!(message2.len(), 48);
        assert_eq!(message2[..32], public(&HUB_EPHEMERAL));
        assert_eq!(phone.read_message(&message2, &mut scratch).unwrap(), 0);
        let hash = hub.get_handshake_hash().to_vec();
        assert_eq!(hash, phone.get_handshake_hash());
        let mut hub = hub.into_transport_mode().unwrap();
        let mut phone = phone.into_transport_mode().unwrap();

        // The hub reads what the phone sends as the hub would.
        let mut at_hub = Inbound {
            awaiting: Some(HUB_PING),
            ..Inbound::default()
        };
        let mut at_phone = Inbound {
            awaiting: Some(PHONE_PING),
            ..Inbound::default()
        };
        let mut transport = Vec::new();
        for (from, plain, opened) in transport_plaintexts() {
            let (sender, receiver, reader) = if from == "phone" {
                (&mut phone, &mut hub, &mut at_hub)
            } else {
                (&mut hub, &mut phone, &mut at_phone)
            };
            let mut cipher = vec![0; plain.len() + 16];
            let len = sender.write_message(&plain, &mut cipher).unwrap();
            cipher.truncate(len);
            let mut back = vec![0; cipher.len()];
            let len = receiver.read_message(&cipher, &mut back).unwrap();
            assert_eq!(back[..len], plain);
            assert_eq!(reader.open(&plain), opened);
            transport.push((from, plain, cipher));
        }

        let fields = [
            ("pattern", PATTERN.to_owned()),
            ("prologue", hex(PROLOGUE)),
            ("hub_static_private", hex(&HUB)),
            ("hub_static_public", hex(&hub_public)),
            ("phone_static_private", hex(&PHONE)),
            ("phone_static_public", hex(&public(&PHONE))),
            ("hub_ephemeral_private", hex(&HUB_EPHEMERAL)),
            ("phone_ephemeral_private", hex(&PHONE_EPHEMERAL)),
            ("message1", hex(&message1)),
            ("message2", hex(&message2)),
            ("handshake_hash", hex(&hash)),
        ];
        render(&fields, &transport)
    }

    /// The vectors' transport plaintexts in the order they are sent, each with its sender and
    /// what its receiver reads it as: a one-chunk frame, a two-chunk frame, a ping and its
    /// pong, each way.
    fn transport_plaintexts() -> Vec<(&'static str, Vec<u8>, Opened)> {
        // A frame whose first chunk ends inside a three-byte character.
        let long = format!("{{\"t\":\"{}\"}}", "€".repeat(21_668));
        assert_eq!(long.len(), 65_012);
        let mut list: Vec<(&'static str, Vec<u8>, Opened)> = Vec::new();
        list.push((
            "phone",
            plain(LAST, br#"{"type":"hello"}"#),
            Opened::Frame(r#"{"type":"hello"}"#.into()),
        ));
        list.push((
            "hub",
            plain(LAST, br#"{"type":"welcome"}"#),
            Opened::Frame(r#"{"type":"welcome"}"#.into()),
        ));
        let parts: Vec<_> = chunks(long.as_bytes()).collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].1.len(), CHUNK);
        for from in ["phone", "hub"] {
            list.push((from, plain(parts[0].0, parts[0].1), Opened::More));
            list.push((
                from,
                plain(parts[1].0, parts[1].1),
                Opened::Frame(long.clone()),
            ));
        }
        list.push(("hub", control(PING, HUB_PING), Opened::Ping(HUB_PING)));
        list.push(("phone", control(PONG, HUB_PING), Opened::Pong));
        list.push(("phone", control(PING, PHONE_PING), Opened::Ping(PHONE_PING)));
        list.push(("hub", control(PONG, PHONE_PING), Opened::Pong));
        list
    }

    /// The vectors as JSON: `fields` in their order, then `transport`, one entry a line.
    fn render(fields: &[(&str, String)], transport: &[(&str, Vec<u8>, Vec<u8>)]) -> String {
        let mut out = String::from("{\n");
        for (name, value) in fields {
            writeln!(out, "  \"{name}\": \"{value}\",").unwrap();
        }
        out.push_str("  \"transport\": [\n");
        let count = transport.len();
        for (index, (from, plain, cipher)) in transport.iter().enumerate() {
            let comma = if index + 1 < count { "," } else { "" };
            let (plain, cipher) = (hex(plain), hex(cipher));
            let entry =
                format!(r#"{{ "from": "{from}", "plain": "{plain}", "cipher": "{cipher}" }}"#);
            writeln!(out, "    {entry}{comma}").unwrap();
        }
        out.push_str("  ]\n}\n");
        out
    }

    #[test]
    fn the_vectors_file_matches_snow() {
        let made = vectors();
        if std::env::var_os("UPDATE_VECTORS").is_some_and(|value| value == "1") {
            std::fs::write(VECTORS, &made).unwrap();
        }
        let committed = std::fs::read_to_string(VECTORS).expect("noise-vectors.json is committed");
        let parsed: serde_json::Value = serde_json::from_str(&committed).unwrap();
        assert_eq!(parsed["pattern"], PATTERN);
        assert_eq!(parsed["prologue"], hex(b"octet-link-v1"));
        assert_eq!(parsed["transport"].as_array().unwrap().len(), 10);
        assert!(
            committed == made,
            "noise-vectors.json is stale: run with UPDATE_VECTORS=1"
        );
    }

    #[test]
    fn snow_alone_takes_an_all_zero_shared_secret_and_this_resolver_refuses_it() {
        let mut out = [0xaa; 56];
        let mut theirs = DefaultResolver.resolve_dh(&DHChoice::Curve25519).unwrap();
        theirs.set(&HUB);
        assert_eq!(theirs.dh(&[0; 32], &mut out), Ok(()));
        assert_eq!(out[..32], [0; 32]);
        let mut ours = Resolver.resolve_dh(&DHChoice::Curve25519).unwrap();
        ours.set(&HUB);
        assert_eq!(ours.dh(&[0; 32], &mut out), Err(snow::Error::Dh));
        assert_eq!(ours.dh(&public(&PHONE), &mut out), Ok(()));
        assert_ne!(out[..32], [0; 32]);
    }

    #[test]
    fn a_first_message_with_a_low_order_ephemeral_fails_its_diffie_hellman() {
        let (mut message, _) = first(PROLOGUE);
        message[..32].fill(0);
        let mut payload = [0; 96];
        let mut ours = builder()
            .unwrap()
            .local_private_key(&HUB)
            .unwrap()
            .build_responder()
            .unwrap();
        assert_eq!(
            ours.read_message(&message, &mut payload),
            Err(snow::Error::Dh)
        );
        let mut theirs = Builder::new(PATTERN.parse().unwrap())
            .local_private_key(&HUB)
            .unwrap()
            .prologue(PROLOGUE)
            .unwrap()
            .build_responder()
            .unwrap();
        assert_eq!(
            theirs.read_message(&message, &mut payload),
            Err(snow::Error::Decrypt)
        );
    }

    #[tokio::test]
    async fn a_first_message_of_any_other_size_is_refused() {
        let (good, _) = first(PROLOGUE);
        let short = good[..95].to_vec();
        let mut long = good.clone();
        long.push(0);
        assert_eq!(answer_to(Message::binary(short)).await, None);
        assert_eq!(answer_to(Message::binary(long)).await, None);
        assert_eq!(answer_to(Message::binary(Vec::new())).await, None);
        assert_eq!(answer_to(Message::text(hex(&good))).await, None);
        let mut random = vec![0; 96];
        getrandom::fill(&mut random).unwrap();
        assert_eq!(answer_to(Message::binary(random)).await, None);
        assert_eq!(
            answer_to(Message::binary(good)).await,
            Some(Peer {
                key: public(&PHONE)
            })
        );
    }

    #[tokio::test]
    async fn a_first_message_under_another_prologue_is_refused() {
        let (message, _) = first(b"octet-link-v2");
        assert_eq!(answer_to(Message::binary(message)).await, None);
    }

    #[tokio::test]
    async fn a_closed_socket_before_the_first_message_is_refused() {
        let (hub, phone) = sockets().await;
        drop(phone);
        assert!(respond(hub, &HUB).await.is_none());
    }

    #[tokio::test]
    async fn websocket_pings_are_left_to_tungstenite() {
        let (hub, mut ws) = sockets().await;
        let (message, mut noise) = first(PROLOGUE);
        ws.send(Message::Ping(b"relay".to_vec().into()))
            .await
            .unwrap();
        ws.send(Message::binary(message)).await.unwrap();
        let (mut secure, _) = respond(hub, &HUB).await.unwrap();
        let second = binary(&mut ws).await;
        let mut payload = [0; 64];
        noise.read_message(&second, &mut payload).unwrap();
        let mut phone = Phone {
            ws,
            noise: noise.into_transport_mode().unwrap(),
        };
        phone
            .ws
            .send(Message::Ping(b"relay".to_vec().into()))
            .await
            .unwrap();
        phone
            .ws
            .send(Message::Pong(b"relay".to_vec().into()))
            .await
            .unwrap();
        phone.put(&plain(LAST, b"{}")).await;
        assert_eq!(secure.next().await, Incoming::Text("{}".into()));
    }

    #[tokio::test]
    async fn a_text_message_after_the_handshake_closes() {
        let (mut secure, mut phone) = connect().await;
        phone.ws.send(Message::text("{}")).await.unwrap();
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_frame_over_65000_bytes_goes_as_two_chunks_and_comes_back_whole() {
        let (mut secure, mut phone) = connect().await;
        let frame = "€".repeat(MAX_MESSAGE / 3);
        assert_eq!(frame.len(), MAX_MESSAGE);
        assert!(secure.send(&frame).await);
        let one = phone.take().await;
        let two = phone.take().await;
        assert_eq!((one[0], one.len()), (MORE, 1 + CHUNK));
        assert_eq!((two[0], two.len()), (LAST, 1 + MAX_MESSAGE - CHUNK));
        assert_eq!([&one[1..], &two[1..]].concat(), frame.as_bytes());
        phone.put(&one).await;
        phone.put(&two).await;
        assert_eq!(secure.next().await, Incoming::Skip);
        assert_eq!(secure.next().await, Incoming::Text(frame));
        phone.put(&plain(LAST, b"{}")).await;
        assert_eq!(secure.next().await, Incoming::Text("{}".into()));
    }

    #[tokio::test]
    async fn a_200_kib_frame_is_sent_in_four_chunks() {
        let (mut secure, mut phone) = connect().await;
        let frame = "x".repeat(200 * 1024);
        assert!(secure.send(&frame).await);
        let mut joined = Vec::new();
        for (kind, len) in [
            (MORE, CHUNK),
            (MORE, CHUNK),
            (MORE, CHUNK),
            (LAST, 200 * 1024 - 3 * CHUNK),
        ] {
            let chunk = phone.take().await;
            assert_eq!((chunk[0], chunk.len() - 1), (kind, len));
            joined.extend_from_slice(&chunk[1..]);
        }
        assert_eq!(joined, frame.as_bytes());
        assert!(secure.send("{}").await);
        assert_eq!(phone.take().await, plain(LAST, b"{}"));
    }

    #[tokio::test]
    async fn an_empty_frame_is_not_sent() {
        let (mut secure, _phone) = connect().await;
        assert!(!secure.send("").await);
    }

    #[tokio::test]
    async fn a_frame_past_max_message_closes() {
        let (mut secure, mut phone) = connect().await;
        phone.put(&plain(MORE, &vec![b'a'; CHUNK])).await;
        phone
            .put(&plain(LAST, &vec![b'a'; MAX_MESSAGE + 1 - CHUNK]))
            .await;
        assert_eq!(secure.next().await, Incoming::Skip);
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_frame_of_exactly_max_message_is_taken() {
        let (mut secure, mut phone) = connect().await;
        phone.put(&plain(MORE, &vec![b'a'; CHUNK])).await;
        phone
            .put(&plain(LAST, &vec![b'a'; MAX_MESSAGE - CHUNK]))
            .await;
        assert_eq!(secure.next().await, Incoming::Skip);
        assert_eq!(secure.next().await, Incoming::Text("a".repeat(MAX_MESSAGE)));
    }

    #[tokio::test]
    async fn a_chunk_over_65000_bytes_closes() {
        let (mut secure, mut phone) = connect().await;
        phone.put(&plain(LAST, &vec![b'a'; CHUNK + 1])).await;
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_tampered_message_closes() {
        let (mut secure, mut phone) = connect().await;
        let mut message = phone.seal(&plain(LAST, b"{}"));
        message[3] ^= 1;
        phone.raw(message).await;
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_message_sent_twice_closes() {
        let (mut secure, mut phone) = connect().await;
        let message = phone.seal(&plain(LAST, b"{}"));
        phone.raw(message.clone()).await;
        phone.raw(message).await;
        assert_eq!(secure.next().await, Incoming::Text("{}".into()));
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn messages_out_of_order_close() {
        let (mut secure, mut phone) = connect().await;
        let one = phone.seal(&plain(LAST, b"1"));
        let two = phone.seal(&plain(LAST, b"2"));
        phone.raw(two).await;
        phone.raw(one).await;
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_message_too_short_to_hold_a_tag_closes() {
        let (mut secure, mut phone) = connect().await;
        phone.raw(vec![7; 15]).await;
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn chunks_never_end_closes() {
        let (mut secure, mut phone) = connect().await;
        let mut taken = 0;
        loop {
            phone.put(&plain(MORE, &[b'a'; 1000])).await;
            match secure.next().await {
                Incoming::Skip => taken += 1000,
                Incoming::Closed => break,
                other => panic!("a chunk read as {other:?}"),
            }
            assert!(taken <= MAX_MESSAGE, "chunks past MAX_MESSAGE were kept");
        }
        assert_eq!(taken, MAX_MESSAGE / 1000 * 1000);
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn an_unknown_type_byte_closes() {
        for kind in [4, 9, 0x7f, 0xff] {
            let (mut secure, mut phone) = connect().await;
            phone.put(&plain(kind, b"{}")).await;
            closes(&mut secure).await;
        }
    }

    #[tokio::test]
    async fn an_empty_chunk_closes() {
        for message in [plain(MORE, b""), plain(LAST, b""), Vec::new()] {
            let (mut secure, mut phone) = connect().await;
            phone.put(&message).await;
            closes(&mut secure).await;
        }
    }

    #[tokio::test]
    async fn a_frame_that_is_not_utf8_closes() {
        let (mut secure, mut phone) = connect().await;
        phone.put(&plain(LAST, &[b'{', 0xff, 0xfe, b'}'])).await;
        closes(&mut secure).await;
        // A character split across chunks is whole once they are joined.
        let (mut secure, mut phone) = connect().await;
        phone.put(&plain(MORE, &[0xe2, 0x82])).await;
        phone.put(&plain(LAST, &[0xac])).await;
        assert_eq!(secure.next().await, Incoming::Skip);
        assert_eq!(secure.next().await, Incoming::Text("€".into()));
    }

    #[tokio::test]
    async fn a_ping_of_seven_bytes_closes() {
        for bytes in [&[1; 7][..], &[1; 9][..], &[][..]] {
            let (mut secure, mut phone) = connect().await;
            phone.put(&plain(PING, bytes)).await;
            closes(&mut secure).await;
        }
    }

    #[tokio::test]
    async fn a_ping_is_answered_with_its_bytes_and_reads_as_skip() {
        let (mut secure, mut phone) = connect().await;
        phone.put(&control(PING, PHONE_PING)).await;
        assert_eq!(secure.next().await, Incoming::Skip);
        assert_eq!(phone.take().await, control(PONG, PHONE_PING));
        // Between a frame's chunks too, and the frame still comes whole.
        phone.put(&plain(MORE, b"{")).await;
        phone.put(&control(PING, HUB_PING)).await;
        phone.put(&plain(LAST, b"}")).await;
        assert_eq!(secure.next().await, Incoming::Skip);
        assert_eq!(secure.next().await, Incoming::Skip);
        assert_eq!(phone.take().await, control(PONG, HUB_PING));
        assert_eq!(secure.next().await, Incoming::Text("{}".into()));
    }

    #[tokio::test]
    async fn the_pong_to_my_ping_reads_as_pong() {
        let (mut secure, mut phone) = connect().await;
        for _ in 0..3 {
            assert!(secure.ping().await);
            let ping = phone.take().await;
            assert_eq!((ping.len(), ping[0]), (9, PING));
            phone.put(&plain(PONG, &ping[1..])).await;
            assert_eq!(secure.next().await, Incoming::Pong);
        }
    }

    #[tokio::test]
    async fn two_pings_carry_different_bytes() {
        let (mut secure, mut phone) = connect().await;
        assert!(secure.ping().await);
        let one = phone.take().await;
        phone.put(&plain(PONG, &one[1..])).await;
        assert_eq!(secure.next().await, Incoming::Pong);
        assert!(secure.ping().await);
        assert_ne!(phone.take().await, one);
    }

    #[tokio::test]
    async fn a_ping_while_the_last_is_unanswered_is_refused() {
        let (mut secure, mut phone) = connect().await;
        assert!(secure.ping().await);
        assert!(!secure.ping().await);
        let ping = phone.take().await;
        phone.put(&plain(PONG, &ping[1..])).await;
        assert_eq!(secure.next().await, Incoming::Pong);
        assert!(secure.ping().await);
    }

    #[tokio::test]
    async fn a_pong_with_other_bytes_closes() {
        let (mut secure, mut phone) = connect().await;
        assert!(secure.ping().await);
        let mut ping = phone.take().await;
        ping[1] ^= 1;
        phone.put(&plain(PONG, &ping[1..])).await;
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_pong_nobody_asked_for_closes() {
        let (mut secure, mut phone) = connect().await;
        phone.put(&control(PONG, HUB_PING)).await;
        closes(&mut secure).await;
        // Nor is one pong good for two.
        let (mut secure, mut phone) = connect().await;
        assert!(secure.ping().await);
        let ping = phone.take().await;
        phone.put(&plain(PONG, &ping[1..])).await;
        phone.put(&plain(PONG, &ping[1..])).await;
        assert_eq!(secure.next().await, Incoming::Pong);
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn a_pong_of_seven_bytes_closes() {
        let (mut secure, mut phone) = connect().await;
        assert!(secure.ping().await);
        let ping = phone.take().await;
        phone.put(&plain(PONG, &ping[1..8])).await;
        closes(&mut secure).await;
    }

    #[tokio::test]
    async fn nothing_is_sent_once_closed() {
        let (mut secure, mut phone) = connect().await;
        phone.raw(vec![0; 40]).await;
        closes(&mut secure).await;
        assert!(!secure.send("{}").await);
        assert!(!secure.ping().await);
    }

    #[tokio::test]
    async fn close_sends_a_websocket_close() {
        let (mut secure, mut phone) = connect().await;
        secure.close().await;
        assert!(matches!(phone.ws.next().await, Some(Ok(Message::Close(_)))));
        assert!(!secure.send("{}").await);
    }

    #[tokio::test]
    async fn the_phone_going_away_reads_as_closed() {
        let (mut secure, phone) = connect().await;
        drop(phone);
        closes(&mut secure).await;
    }

    #[test]
    fn the_channel_can_move_between_threads() {
        fn sendable<T: Send>(_: &T) {}
        let (hub, _phone) = tokio::io::duplex(64);
        let ws = futures_util::FutureExt::now_or_never(WebSocketStream::from_raw_socket(
            hub,
            Role::Server,
            None,
        ));
        let respond = respond(ws.unwrap(), &HUB);
        sendable(&respond);
    }

    /// The next number of a `SplitMix64` sequence.
    fn splitmix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// `len` pseudo-random bytes.
    fn noise_bytes(state: &mut u64, len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            bytes.extend_from_slice(&splitmix(state).to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    /// Spec §9: whatever comes as a first message (random bytes of any length, or a good one
    /// replayed with a bit flipped, bytes overwritten, cut short or run on), the hub never panics
    /// and never makes a channel of it. Seeded, and a few hundred handshakes, so it stays quick.
    #[tokio::test]
    async fn random_and_mutated_first_messages_are_refused_and_never_panic() {
        let mut state = 0x6875_622d_6669_7273;
        let (good, _) = first(PROLOGUE);
        let mut refused = 0;
        for round in 0..400 {
            let roll = splitmix(&mut state);
            let at = usize::try_from(roll % 96).unwrap();
            let message = match round % 5 {
                // Random bytes, of any length up to twice a first message's.
                0 => noise_bytes(&mut state, usize::try_from((roll >> 8) % 193).unwrap()),
                // Random bytes of exactly a first message's length.
                1 => noise_bytes(&mut state, FIRST),
                // The good message replayed with one bit flipped.
                2 => {
                    let mut flipped = good.clone();
                    flipped[at] ^= 1 << ((roll >> 8) % 8);
                    flipped
                }
                // The good message with a run of bytes overwritten.
                3 => {
                    let mut overwritten = good.clone();
                    let end = (at + 1 + usize::try_from((roll >> 8) % 16).unwrap()).min(96);
                    let random = noise_bytes(&mut state, end - at);
                    for (byte, new) in overwritten[at..end].iter_mut().zip(random) {
                        *byte ^= new | 1;
                    }
                    overwritten
                }
                // The good message cut short, or run on with random bytes.
                _ => {
                    let mut changed = good[..at].to_vec();
                    if roll & (1 << 40) != 0 {
                        changed.clone_from(&good);
                        let more = 1 + usize::try_from((roll >> 8) % 64).unwrap();
                        changed.extend(noise_bytes(&mut state, more));
                    }
                    changed
                }
            };
            assert_ne!(message, good, "round {round} made the good message");
            let answer = answer_to(Message::binary(message)).await;
            assert_eq!(answer, None, "round {round} made a channel");
            refused += 1;
        }
        assert_eq!(refused, 400);
        // The harness itself is sound: the good message, unchanged, is answered.
        assert_eq!(
            answer_to(Message::binary(good)).await,
            Some(Peer {
                key: public(&PHONE)
            })
        );
    }

    #[tokio::test]
    async fn random_messages_never_panic() {
        const KINDS: [u8; 6] = [MORE, LAST, PING, PONG, 4, 0xff];
        const LENGTHS: [usize; 12] = [0, 1, 2, 7, 8, 9, 16, 100, 300, CHUNK - 1, CHUNK, CHUNK + 1];
        let mut state = 0x6f63_7465_742d_6c6b;
        let (mut secure, mut phone) = connect().await;
        let mut seen = [0_u32; 4];
        for _ in 0..2_000 {
            let roll = splitmix(&mut state);
            let len = LENGTHS[usize::try_from(roll % 12).unwrap()];
            if roll.is_multiple_of(7) {
                let len = usize::try_from(roll % 200).unwrap();
                phone.raw(noise_bytes(&mut state, len)).await;
            } else {
                let kind = KINDS[usize::try_from((roll >> 8) % 6).unwrap()];
                let mut message = noise_bytes(&mut state, len);
                if roll.is_multiple_of(3) {
                    message.fill(b'a');
                }
                phone.put(&plain(kind, &message)).await;
            }
            match secure.next().await {
                Incoming::Closed => {
                    seen[0] += 1;
                    closes(&mut secure).await;
                    (secure, phone) = connect().await;
                }
                Incoming::Skip => seen[1] += 1,
                Incoming::Text(_) => seen[2] += 1,
                Incoming::Pong => seen[3] += 1,
            }
        }
        assert!(
            seen[0] > 0 && seen[1] > 0 && seen[2] > 0,
            "the messages reached every outcome: {seen:?}"
        );
    }
}
