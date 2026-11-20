//! The hub's link to the relay: an outbound control connection that registers the hub's channel,
//! and one outbound data connection per arriving phone, each answered with the Noise handshake.
//! The hub listens on no network port: these and `hub.sock` are its only sockets.
//!
//! # Registering
//!
//! The hub dials `<relay>/v1/hub/<channel>` only while it is wanted (a device is paired or a
//! pairing is open), answers the relay's `{"challenge"}` with its key and a signature, and waits
//! for `{"registered": true}`, all within [`REGISTER_WITHIN`]. A lost control connection is
//! dialled again on the app's schedule: 0.5 seconds doubling to 30, each cut by up to half at
//! random, started over once a registration has held for [`STABLE`]. A registration closed with
//! 4409 was replaced by another hub holding the same key: the hub says so and waits the longest
//! step before trying again, so two such hubs never spin.
//!
//! # Phones
//!
//! Each `{"connect": <id>}` takes a [`Seat`] in the hub's [`Room`] before anything is dialled:
//! 64 connections in all, 32 not yet past their first frame, counting those still dialling or in
//! the handshake; with no seat free the `connect` is ignored. At most [`CONNECTS_PER_MINUTE`] are
//! acted on in any minute, and an id that is malformed or was acted on in the last minute is
//! ignored, so a hostile relay can make the hub dial only so much. One deadline,
//! [`FIRST_FRAME_WITHIN`] from the `connect`, covers the dial, the handshake and the first
//! accepted frame (the hub loop enforces the last part). The relay's WebSocket messages are
//! capped at [`MAX_NOISE`], the largest Noise message, since tungstenite holds a whole message
//! before the channel sees it.
use crate::{
    keys::HubKeys,
    secure::{MAX_NOISE, Peer, Secure, respond},
    store::{hex, unhex},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, sleep, timeout, timeout_at},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

/// Connections the hub holds at once, dialling, in the handshake or authenticated.
pub(crate) const MAX_CONNECTIONS: usize = 64;
/// Connections not yet past their first accepted frame, dialling and in the handshake included.
pub(crate) const MAX_WAITING: usize = 32;
/// The one deadline from a `connect` to the phone's first accepted frame.
pub(crate) const FIRST_FRAME_WITHIN: Duration = Duration::from_secs(10);
/// `connect`s acted on in any minute; more are ignored.
pub(crate) const CONNECTS_PER_MINUTE: usize = 120;
/// How long dialling the relay, its challenge and its answer may take in all.
const REGISTER_WITHIN: Duration = Duration::from_secs(10);
/// The largest control message the relay sends, in bytes.
const CONTROL_LIMIT: usize = 1024;
/// The first wait before dialling the relay again.
const BACKOFF_START: Duration = Duration::from_millis(500);
/// The longest wait before dialling the relay again.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a registration must hold for its loss to start the waits over.
const STABLE: Duration = Duration::from_secs(10);
/// The relay's close code for a registration a newer one replaced.
const REPLACED: u16 = 4409;
/// The span [`CONNECTS_PER_MINUTE`] counts over, and how long an id is remembered.
const MINUTE: Duration = Duration::from_secs(60);
/// Logged when another hub registered the same channel.
pub(crate) const ANOTHER_HUB: &str = "Another hub took this channel";

/// Where the relay link writes its log lines: standard error, unless a test hands it a sink.
pub(crate) type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// A [`Log`] to standard error.
pub(crate) fn stderr() -> Log {
    Arc::new(|line| eprintln!("{line}"))
}

/// Where the hub stands with the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayState {
    /// Not dialling: nothing is paired and no pairing is open.
    Off,
    /// Dialling, registering, or waiting to try again.
    Connecting,
    /// The relay holds the hub's channel.
    Registered,
}

impl RelayState {
    /// The name the control socket's `status` gives it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Connecting => "connecting",
            Self::Registered => "registered",
        }
    }
}

/// tungstenite's client stream: a TCP socket, with TLS for a `wss` relay.
pub(crate) type Stream = MaybeTlsStream<TcpStream>;

/// A phone past its handshake, for the hub loop.
#[derive(Debug)]
pub(crate) struct Arrival {
    /// The encrypted channel to it.
    pub channel: Secure<Stream>,
    /// Its key, as the handshake proved it.
    pub peer: Peer,
    /// Its place in the room, taken at its `connect`.
    pub seat: Seat,
}

/// The connections the hub holds: [`MAX_CONNECTIONS`] in all, [`MAX_WAITING`] of them not yet
/// past their first accepted frame.
#[derive(Debug, Default)]
pub(crate) struct Room {
    counts: Mutex<Counts>,
}

/// What a [`Room`] holds.
#[derive(Debug, Default)]
struct Counts {
    open: usize,
    waiting: usize,
}

impl Room {
    /// A seat for one more connection, not yet past its first frame, from `since`; `None` when
    /// the room is full.
    pub(crate) fn seat(self: &Arc<Self>, since: Instant) -> Option<Seat> {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if counts.open >= MAX_CONNECTIONS || counts.waiting >= MAX_WAITING {
            return None;
        }
        counts.open += 1;
        counts.waiting += 1;
        Some(Seat {
            room: Arc::clone(self),
            waiting: true,
            since,
        })
    }

    /// Connections held, and how many of them wait for their first frame.
    #[cfg(test)]
    fn held(&self) -> (usize, usize) {
        let counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        (counts.open, counts.waiting)
    }
}

/// One connection's place in the [`Room`], given back when it drops.
#[derive(Debug)]
pub(crate) struct Seat {
    room: Arc<Room>,
    /// Whether it still counts as waiting for its first frame.
    waiting: bool,
    /// When its `connect` came: its deadline runs from here.
    since: Instant,
}

impl Seat {
    /// It sent its first accepted frame: it no longer counts as waiting.
    pub(crate) fn admitted(&mut self) {
        if std::mem::take(&mut self.waiting) {
            let mut counts = self
                .room
                .counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            counts.waiting = counts.waiting.saturating_sub(1);
        }
    }

    /// When its `connect` came.
    pub(crate) fn since(&self) -> Instant {
        self.since
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let mut counts = self
            .room
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        counts.open = counts.open.saturating_sub(1);
        if self.waiting {
            counts.waiting = counts.waiting.saturating_sub(1);
        }
    }
}

/// The waits between dials of the relay, as the app's own reconnects wait.
#[derive(Debug, Default)]
struct Backoff {
    /// Dials since the last stable registration.
    attempt: u32,
}

impl Backoff {
    /// The longest step's attempt number: the ceiling is [`BACKOFF_MAX`] from here on.
    const LONGEST: u32 = 16;

    /// The next wait: 0.5 seconds doubling to 30, cut by up to half at random.
    fn next(&mut self) -> Duration {
        let ceiling = BACKOFF_START
            .saturating_mul(1 << self.attempt.min(Self::LONGEST))
            .min(BACKOFF_MAX);
        self.attempt = (self.attempt + 1).min(Self::LONGEST);
        let mut random = [0; 2];
        // No random bytes: the whole ceiling, which is never sooner.
        let random = getrandom::fill(&mut random).map_or(u16::MAX, |()| u16::from_le_bytes(random));
        ceiling / 2 + (ceiling / 2).mul_f64(f64::from(random) / f64::from(u16::MAX))
    }

    /// A registration held: the next loss waits the shortest step.
    fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Another hub took the channel: the next wait is the longest step.
    fn replaced(&mut self) {
        self.attempt = Self::LONGEST;
    }
}

/// How a control connection ended.
#[derive(Debug)]
enum Ending {
    /// It never registered, for this reason.
    Failed(String),
    /// It registered and was lost after `held`.
    Lost { held: Duration },
    /// Another registration of the channel replaced it.
    Replaced,
}

/// What every dial needs: where the relay is, the hub's keys and room, and where phones go.
struct Dialer {
    url: String,
    keys: Arc<HubKeys>,
    room: Arc<Room>,
    arrivals: mpsc::Sender<Arrival>,
    /// The ids acted on in the last minute, oldest first, with when.
    recent: VecDeque<(Instant, [u8; 16])>,
    /// The dials and handshakes under way; aborted with the link.
    dials: JoinSet<()>,
}

/// Keeps the hub registered with the relay at `url` while `wanted` says so, saying where it
/// stands in `state`, and hands each phone that completes its handshake to `arrivals` with the
/// seat it took in `room`. Runs until `wanted`'s sender goes; writes its log lines to `log`.
pub(crate) async fn run(
    url: String,
    keys: Arc<HubKeys>,
    mut wanted: watch::Receiver<bool>,
    room: Arc<Room>,
    arrivals: mpsc::Sender<Arrival>,
    state: watch::Sender<RelayState>,
    log: Log,
) {
    let mut dialer = Dialer {
        url: url.trim_end_matches('/').to_owned(),
        keys,
        room,
        arrivals,
        recent: VecDeque::new(),
        dials: JoinSet::new(),
    };
    let mut backoff = Backoff::default();
    // Whether the last dial failed too: a run of failures is logged once.
    let mut failing = false;
    loop {
        state.send_replace(RelayState::Off);
        if wanted.wait_for(|wanted| *wanted).await.is_err() {
            return;
        }
        state.send_replace(RelayState::Connecting);
        let ending = tokio::select! {
            ending = dialer.session(&state, &log) => ending,
            unwanted = wanted.wait_for(|wanted| !*wanted) => {
                if unwanted.is_err() {
                    return;
                }
                backoff.reset();
                continue;
            }
        };
        state.send_replace(RelayState::Connecting);
        match ending {
            Ending::Failed(reason) => {
                if !failing {
                    log(&format!("Couldn't register with the relay: {reason}"));
                }
                failing = true;
            }
            Ending::Lost { held } => {
                log("Lost the relay; dialling it again");
                failing = false;
                if held >= STABLE {
                    backoff.reset();
                }
            }
            Ending::Replaced => {
                log(&format!("{ANOTHER_HUB}; trying again later"));
                failing = false;
                backoff.replaced();
            }
        }
        let wait = backoff.next();
        tokio::select! {
            () = sleep(wait) => {}
            unwanted = wanted.wait_for(|wanted| !*wanted) => {
                if unwanted.is_err() {
                    return;
                }
                backoff.reset();
            }
        }
    }
}

/// A WebSocket configuration whose messages and frames are at most `limit` bytes.
fn capped(limit: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit))
}

/// The next text message on `ws`, WebSocket pings and pongs aside; `Err` with the reason if the
/// socket ends or sends anything else.
async fn next_text(ws: &mut WebSocketStream<Stream>) -> Result<String, String> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => return Ok(text.as_str().to_owned()),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(Some(frame)))) => {
                return Err(format!("closed with {}", u16::from(frame.code)));
            }
            Some(Ok(Message::Binary(_))) => return Err("binary on the control link".into()),
            Some(Ok(Message::Close(None))) | None => return Err("closed".into()),
            Some(Err(error)) => return Err(error.to_string()),
        }
    }
}

impl Dialer {
    /// One control connection: dial, register, then take `connect`s until it ends.
    async fn session(&mut self, state: &watch::Sender<RelayState>, log: &Log) -> Ending {
        let mut ws = match timeout(REGISTER_WITHIN, self.register()).await {
            Ok(Ok(ws)) => ws,
            Ok(Err(reason)) => return Ending::Failed(reason),
            Err(_) => return Ending::Failed("the relay took too long to answer".into()),
        };
        state.send_replace(RelayState::Registered);
        log("Registered with the relay");
        let registered = Instant::now();
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => self.connect(text.as_str()),
                Some(Ok(Message::Close(Some(frame)))) if u16::from(frame.code) == REPLACED => {
                    return Ending::Replaced;
                }
                // Pings are answered as the socket is read; binary means nothing here.
                Some(Ok(
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) | Message::Binary(_),
                )) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    return Ending::Lost {
                        held: registered.elapsed(),
                    };
                }
            }
        }
    }

    /// Dials the control connection and registers the channel on it.
    async fn register(&self) -> Result<WebSocketStream<Stream>, String> {
        let channel = self.keys.channel();
        let url = format!("{}/v1/hub/{channel}", self.url);
        let (mut ws, _) = connect_async_with_config(url, Some(capped(CONTROL_LIMIT)), true)
            .await
            .map_err(|error| error.to_string())?;
        let challenge = next_text(&mut ws).await?;
        let challenge = serde_json::from_str::<Value>(&challenge)
            .ok()
            .and_then(|value| unhex::<32>(value.get("challenge")?.as_str()?))
            .ok_or("the relay sent no challenge")?;
        let answer = json!({
            "key": hex(&self.keys.relay_public()),
            "signature": hex(&self.keys.sign(&challenge)),
        });
        ws.send(Message::text(answer.to_string()))
            .await
            .map_err(|error| error.to_string())?;
        let registered = next_text(&mut ws).await?;
        if serde_json::from_str::<Value>(&registered).ok() != Some(json!({"registered": true})) {
            return Err("the relay did not register the hub".into());
        }
        Ok(ws)
    }

    /// Acts on one control message: a `connect` with a well-formed id not acted on in the last
    /// minute, while the minute's allowance and the room have space, is dialled; anything else
    /// is ignored.
    fn connect(&mut self, text: &str) {
        let Some(id) = connect_id(text) else {
            return;
        };
        let now = Instant::now();
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= MINUTE)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= CONNECTS_PER_MINUTE
            || self.recent.iter().any(|(_, seen)| *seen == id)
        {
            return;
        }
        let Some(seat) = self.room.seat(now) else {
            return;
        };
        self.recent.push_back((now, id));
        while self.dials.try_join_next().is_some() {}
        let url = format!("{}/v1/hub/{}/{}", self.url, self.keys.channel(), hex(&id));
        let (keys, arrivals) = (Arc::clone(&self.keys), self.arrivals.clone());
        self.dials.spawn(dial(url, keys, seat, arrivals));
    }
}

/// The connection id of a control message `{"connect": <32 lowercase hex digits>}`; `None` for
/// any other message.
fn connect_id(text: &str) -> Option<[u8; 16]> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    unhex::<16>(object.get("connect")?.as_str()?)
}

/// Opens the data connection at `url` for the phone that holds `seat`, answers its handshake,
/// and hands it to the hub loop, all before the seat's deadline; otherwise the seat goes back.
async fn dial(url: String, keys: Arc<HubKeys>, seat: Seat, arrivals: mpsc::Sender<Arrival>) {
    let deadline = seat.since() + FIRST_FRAME_WITHIN;
    let answered = timeout_at(deadline, async {
        let (ws, _) = connect_async_with_config(url, Some(capped(MAX_NOISE)), true)
            .await
            .ok()?;
        respond(ws, keys.identity_private()).await
    })
    .await;
    if let Ok(Some((channel, peer))) = answered {
        let arrival = Arrival {
            channel,
            peer,
            seat,
        };
        let _ = arrivals.send(arrival).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_is_given_back_and_the_room_holds_sixty_four_and_thirty_two_waiting() {
        let room = Arc::new(Room::default());
        let now = Instant::now();
        // Thirty-two may wait; the thirty-third is turned away.
        let mut seats: Vec<Seat> = (0..MAX_WAITING).filter_map(|_| room.seat(now)).collect();
        assert_eq!(seats.len(), MAX_WAITING);
        assert!(room.seat(now).is_none(), "the waiting room is full");
        // Past their first frame they make room, up to sixty-four in all.
        seats.iter_mut().for_each(Seat::admitted);
        seats.extend((0..MAX_WAITING).filter_map(|_| room.seat(now)));
        assert_eq!(seats.len(), MAX_CONNECTIONS);
        assert_eq!(room.held(), (MAX_CONNECTIONS, MAX_WAITING));
        seats.iter_mut().for_each(Seat::admitted);
        assert!(room.seat(now).is_none(), "every connection is open");
        drop(seats);
        assert_eq!(room.held(), (0, 0));
        // A seat dropped while it waits gives back both places.
        let seat = room.seat(now).expect("room");
        assert_eq!(room.held(), (1, 1));
        drop(seat);
        assert_eq!(room.held(), (0, 0));
    }

    #[test]
    fn only_a_well_formed_connect_names_an_id() {
        let id = "00112233445566778899aabbccddeeff";
        assert_eq!(
            connect_id(&format!(r#"{{"connect":"{id}"}}"#)),
            unhex::<16>(id)
        );
        for bad in [
            format!(r#"{{"connect":"{}"}}"#, id.to_uppercase()),
            format!(r#"{{"connect":"{}"}}"#, &id[..30]),
            format!(r#"{{"connect":"{id}00"}}"#),
            format!(r#"{{"connect":"{id}","x":1}}"#),
            r#"{"connect":7}"#.to_owned(),
            r#"{"registered":true}"#.to_owned(),
            "not json".to_owned(),
        ] {
            assert_eq!(connect_id(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_waits_double_to_thirty_seconds_cut_by_up_to_half() {
        let mut backoff = Backoff::default();
        for ceiling in [500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000] {
            let wait = backoff.next();
            let ceiling = Duration::from_millis(ceiling);
            assert!(
                wait <= ceiling && wait >= ceiling / 2,
                "{wait:?} of {ceiling:?}"
            );
        }
        backoff.reset();
        assert!(backoff.next() <= BACKOFF_START);
        backoff.replaced();
        assert!(
            backoff.next() >= BACKOFF_MAX / 2,
            "a replaced hub waits the longest step"
        );
    }
}
