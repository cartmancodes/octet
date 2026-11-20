//! The relay end to end: each test starts `serve` on `127.0.0.1:0` and talks to it as hubs and
//! phones would, over real WebSocket connections. The tests for the 10 second deadlines set up in real time
//! and then pause tokio's clock, which moves on by itself whenever every task is waiting, for the
//! wait alone: paused from the start, the clock can also jump while a loopback write is still on
//! its way (macOS delivers loopback traffic asynchronously), and fire a timer early. The rest use
//! real time, with short durations set through `Limits` where a timer is under test.
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use octet_relay::{Config, Limits, Log, probe, serve};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{Instant, pause, resume, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Error, Message,
        client::IntoClientRequest,
        http::{HeaderName, HeaderValue},
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The longest any one step may take before the test fails (virtual time in paused tests).
const WAIT: Duration = Duration::from_secs(30);
/// The header the address tests name.
const ADDRESS_HEADER: &str = "X-Client-Address";

/// A running relay; dropping it shuts the relay down.
struct Relay {
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

async fn start(config: Config) -> Relay {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local address");
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(listener, config, async {
        let _ = stopped.await;
    }));
    Relay { addr, _stop: stop }
}

/// Default limits, less the per-address ones, which tests that are not about them would trip.
fn roomy_limits() -> Limits {
    Limits {
        connects_per_minute: 10_000,
        registrations_per_minute: 10_000,
        ..Limits::default()
    }
}

fn roomy() -> Config {
    Config {
        limits: roomy_limits(),
        ..Config::default()
    }
}

fn with_limits(limits: Limits) -> Config {
    Config {
        limits,
        ..Config::default()
    }
}

async fn within<T>(future: impl Future<Output = T>) -> T {
    timeout(WAIT, future).await.expect("the step took too long")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
        .collect()
}

fn new_key() -> SigningKey {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).expect("random bytes");
    SigningKey::from_bytes(&bytes)
}

fn channel_of(key: &SigningKey) -> String {
    hex(&Sha256::digest(key.verifying_key().as_bytes()))
}

fn hub_path(channel: &str) -> String {
    format!("/v1/hub/{channel}")
}

fn phone_path(channel: &str) -> String {
    format!("/v1/phone/{channel}")
}

fn data_path(channel: &str, id: &str) -> String {
    format!("/v1/hub/{channel}/{id}")
}

/// Opens a WebSocket at `path` with `headers`: the socket, or the HTTP status it was refused with.
async fn try_open(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Result<Ws, u16> {
    let mut request = format!("ws://{addr}{path}")
        .into_client_request()
        .expect("request");
    for (name, value) in headers {
        request.headers_mut().append(
            HeaderName::try_from(*name).expect("header name"),
            HeaderValue::try_from(*value).expect("header value"),
        );
    }
    match within(Box::pin(connect_async(request))).await {
        Ok((ws, _)) => Ok(ws),
        Err(Error::Http(response)) => Err(response.status().as_u16()),
        Err(error) => panic!("the upgrade failed: {error}"),
    }
}

async fn open(addr: SocketAddr, path: &str) -> Ws {
    Box::pin(try_open(addr, path, &[])).await.expect("upgraded")
}

/// The HTTP status of an upgrade to `path`: 101 when it went through.
async fn status(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> u16 {
    Box::pin(try_open(addr, path, headers))
        .await
        .map_or_else(|status| status, |_| 101)
}

/// The next text message as JSON, past pings.
async fn next_json(ws: &mut Ws) -> Value {
    within(async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).expect("JSON"),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("expected a text message, got {other:?}"),
            }
        }
    })
    .await
}

/// The next binary message, past pings.
async fn next_binary(ws: &mut Ws) -> Vec<u8> {
    within(async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(bytes))) => return bytes.to_vec(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("expected a binary message, got {other:?}"),
            }
        }
    })
    .await
}

/// Reads until the connection ends, which must carry no data: the close code it ended with, or
/// `None` when it ended without a close frame.
async fn closed_with(ws: &mut Ws) -> Option<u16> {
    within(async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => return frame.map(|frame| u16::from(frame.code)),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(message)) => panic!("data arrived on a closing connection: {message:?}"),
                Some(Err(_)) | None => return None,
            }
        }
    })
    .await
}

fn binary(bytes: &[u8]) -> Message {
    Message::Binary(bytes.to_vec().into())
}

fn text(text: &str) -> Message {
    Message::Text(text.to_owned().into())
}

/// The challenge a fresh control connection is sent.
async fn challenge_of(control: &mut Ws) -> Vec<u8> {
    let message = next_json(control).await;
    let challenge = message["challenge"].as_str().expect("a challenge");
    assert_eq!(challenge.len(), 64, "{message}");
    assert!(
        challenge
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    );
    unhex(challenge)
}

/// The registration answer: `signer` signs, and `key` is the key sent.
fn answer(signer: &SigningKey, key: &SigningKey, signed: &[u8]) -> Message {
    let signature = signer.sign(signed);
    text(
        &json!({
            "key": hex(key.verifying_key().as_bytes()),
            "signature": hex(&signature.to_bytes()),
        })
        .to_string(),
    )
}

fn signed(challenge: &[u8]) -> Vec<u8> {
    [b"octet-relay-v1".as_slice(), challenge].concat()
}

/// A registered control connection for `key`'s channel.
async fn register(addr: SocketAddr, key: &SigningKey) -> Ws {
    let mut control = open(addr, &hub_path(&channel_of(key))).await;
    let challenge = challenge_of(&mut control).await;
    control
        .send(answer(key, key, &signed(&challenge)))
        .await
        .expect("send");
    assert_eq!(next_json(&mut control).await, json!({"registered": true}));
    control
}

/// The connection id of the next `connect` on `control`.
async fn next_connect(control: &mut Ws) -> String {
    let message = next_json(control).await;
    let id = message["connect"].as_str().expect("a connect").to_owned();
    assert_eq!(id.len(), 32, "{message}");
    assert!(
        id.bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    );
    id
}

/// A phone socket on `channel` and the hub data socket spliced to it.
async fn spliced(addr: SocketAddr, control: &mut Ws, channel: &str) -> (Ws, Ws) {
    let phone = open(addr, &phone_path(channel)).await;
    let id = next_connect(control).await;
    let data = open(addr, &data_path(channel, &id)).await;
    (phone, data)
}

/// A plain HTTP request to the relay: the whole response.
async fn http(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    within(stream.read_to_end(&mut response))
        .await
        .expect("read");
    String::from_utf8(response).expect("text")
}

#[tokio::test]
async fn a_hub_with_a_good_signature_registers() {
    let relay = start(roomy()).await;
    let key = new_key();
    let mut control = register(relay.addr, &key).await;
    let channel = channel_of(&key);
    let _phone = open(relay.addr, &phone_path(&channel)).await;
    next_connect(&mut control).await;
}

#[tokio::test]
async fn a_bad_signature_closes_4401() {
    let relay = start(roomy()).await;
    let key = new_key();
    let mut control = open(relay.addr, &hub_path(&channel_of(&key))).await;
    let challenge = challenge_of(&mut control).await;
    let mut wrong = signed(&challenge);
    wrong[20] ^= 1;
    control.send(answer(&key, &key, &wrong)).await.unwrap();
    assert_eq!(closed_with(&mut control).await, Some(4401));
}

#[tokio::test]
async fn a_key_that_does_not_hash_to_the_channel_closes_4401() {
    let relay = start(roomy()).await;
    let (owner, stranger) = (new_key(), new_key());
    let mut control = open(relay.addr, &hub_path(&channel_of(&owner))).await;
    let challenge = challenge_of(&mut control).await;
    control
        .send(answer(&stranger, &stranger, &signed(&challenge)))
        .await
        .unwrap();
    assert_eq!(closed_with(&mut control).await, Some(4401));
    let phone = open(relay.addr, &phone_path(&channel_of(&owner))).await;
    let mut phone = phone;
    assert_eq!(closed_with(&mut phone).await, Some(4404));
}

#[tokio::test]
async fn a_challenge_left_unanswered_closes_4408() {
    let relay = start(roomy()).await;
    let mut control = open(relay.addr, &hub_path(&channel_of(&new_key()))).await;
    let started = Instant::now();
    challenge_of(&mut control).await;
    pause();
    assert_eq!(closed_with(&mut control).await, Some(4408));
    assert!(
        started.elapsed() >= Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_phone_with_no_hub_closes_4404() {
    let relay = start(roomy()).await;
    let mut phone = open(relay.addr, &phone_path(&channel_of(&new_key()))).await;
    assert_eq!(closed_with(&mut phone).await, Some(4404));
}

#[tokio::test]
async fn a_phone_and_hub_are_spliced_both_ways() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    let large: Vec<u8> = (0..65_536_u32).map(|at| (at % 251) as u8).collect();
    for bytes in [b"x".as_slice(), b"\x00\x01\x02 handshake", &large] {
        phone.send(binary(bytes)).await.unwrap();
        assert_eq!(next_binary(&mut data).await, bytes);
        data.send(binary(bytes)).await.unwrap();
        assert_eq!(next_binary(&mut phone).await, bytes);
    }
    // A second phone on the same channel gets its own splice.
    let (mut other, mut other_data) = spliced(relay.addr, &mut control, &channel).await;
    other.send(binary(b"second")).await.unwrap();
    assert_eq!(next_binary(&mut other_data).await, b"second");
    data.send(binary(b"first")).await.unwrap();
    assert_eq!(next_binary(&mut phone).await, b"first");
}

#[tokio::test]
async fn closing_one_side_closes_the_other() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let normal = || {
        Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        })
    };

    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    phone.close(normal()).await.unwrap();
    assert_eq!(closed_with(&mut data).await, Some(1000));

    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    data.close(normal()).await.unwrap();
    assert_eq!(closed_with(&mut phone).await, Some(1000));

    // A side that goes without a close frame closes the other too.
    let (phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    drop(phone);
    assert!(closed_with(&mut data).await.is_some());
}

#[tokio::test]
async fn a_second_registration_replaces_the_first_with_4409() {
    let relay = start(roomy()).await;
    let key = new_key();
    let mut first = register(relay.addr, &key).await;
    let mut second = register(relay.addr, &key).await;
    assert_eq!(closed_with(&mut first).await, Some(4409));
    let _phone = open(relay.addr, &phone_path(&channel_of(&key))).await;
    next_connect(&mut second).await;
}

#[tokio::test]
async fn a_connection_id_is_good_once() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let _phone = open(relay.addr, &phone_path(&channel)).await;
    let id = next_connect(&mut control).await;
    let _data = open(relay.addr, &data_path(&channel, &id)).await;
    assert_eq!(
        status(relay.addr, &data_path(&channel, &id), &[]).await,
        404
    );
}

#[tokio::test]
async fn a_made_up_connection_id_is_404() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let _phone = open(relay.addr, &phone_path(&channel)).await;
    let id = next_connect(&mut control).await;
    let made_up = hex(&[7; 16]);
    assert_eq!(
        status(relay.addr, &data_path(&channel, &made_up), &[]).await,
        404
    );
    // A real id under another channel is no better.
    let other = channel_of(&new_key());
    assert_eq!(status(relay.addr, &data_path(&other, &id), &[]).await, 404);
}

#[tokio::test]
async fn an_id_from_a_replaced_registration_is_404() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut first = register(relay.addr, &key).await;
    let mut phone = open(relay.addr, &phone_path(&channel)).await;
    let id = next_connect(&mut first).await;
    let _second = register(relay.addr, &key).await;
    assert_eq!(closed_with(&mut first).await, Some(4409));
    assert!(closed_with(&mut phone).await.is_some());
    assert_eq!(
        status(relay.addr, &data_path(&channel, &id), &[]).await,
        404
    );
}

#[tokio::test]
async fn a_registrations_phones_and_data_sockets_close_when_its_control_ends() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    let mut waiting = open(relay.addr, &phone_path(&channel)).await;
    let id = next_connect(&mut control).await;
    control.close(None).await.unwrap();
    assert_eq!(closed_with(&mut phone).await, Some(1001));
    assert_eq!(closed_with(&mut data).await, Some(1001));
    assert_eq!(closed_with(&mut waiting).await, Some(1001));
    assert_eq!(
        status(relay.addr, &data_path(&channel, &id), &[]).await,
        404
    );
}

#[tokio::test]
async fn a_17th_phone_closes_4429_and_the_16_stay() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let mut phones = Vec::new();
    for _ in 0..16 {
        let phone = open(relay.addr, &phone_path(&channel)).await;
        phones.push((phone, next_connect(&mut control).await));
    }
    let mut extra = open(relay.addr, &phone_path(&channel)).await;
    assert_eq!(closed_with(&mut extra).await, Some(4429));
    for (phone, id) in &mut phones {
        let mut data = open(relay.addr, &data_path(&channel, id)).await;
        phone.send(binary(id.as_bytes())).await.unwrap();
        assert_eq!(next_binary(&mut data).await, id.as_bytes());
    }
}

#[tokio::test]
async fn the_channel_ceiling_closes_a_new_registration_4429() {
    let relay = start(with_limits(Limits {
        channels: 1,
        ..roomy_limits()
    }))
    .await;
    let (first, second) = (new_key(), new_key());
    let _held = register(relay.addr, &first).await;
    let mut control = open(relay.addr, &hub_path(&channel_of(&second))).await;
    let challenge = challenge_of(&mut control).await;
    control
        .send(answer(&second, &second, &signed(&challenge)))
        .await
        .unwrap();
    assert_eq!(closed_with(&mut control).await, Some(4429));
    // Replacing a registered channel adds none, so it is still let in.
    let mut again = register(relay.addr, &first).await;
    let _phone = open(relay.addr, &phone_path(&channel_of(&first))).await;
    next_connect(&mut again).await;
}

#[tokio::test]
async fn the_socket_ceiling_answers_503() {
    let relay = start(with_limits(Limits {
        sockets: 2,
        ..roomy_limits()
    }))
    .await;
    let (first, second) = (new_key(), new_key());
    let held = register(relay.addr, &first).await;
    let _other = register(relay.addr, &second).await;
    let no_hub = phone_path(&channel_of(&new_key()));
    assert_eq!(status(relay.addr, &no_hub, &[]).await, 503);
    drop(held);
    let started = Instant::now();
    while status(relay.addr, &no_hub, &[]).await != 101 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the seat was not given back"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_message_over_the_cap_closes() {
    let cap = Limits::default().message_bytes;
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    let at_cap = vec![1; cap];
    phone.send(binary(&at_cap)).await.unwrap();
    assert_eq!(next_binary(&mut data).await, at_cap);
    let _ = phone.send(binary(&vec![2; cap + 1])).await;
    closed_with(&mut phone).await;
    assert!(closed_with(&mut data).await.is_some());
}

#[tokio::test]
async fn a_text_message_on_data_closes() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;

    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    data.send(text("{}")).await.unwrap();
    assert!(closed_with(&mut data).await.is_some());
    assert!(closed_with(&mut phone).await.is_some());

    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    phone.send(text("{}")).await.unwrap();
    assert!(closed_with(&mut phone).await.is_some());
    assert!(closed_with(&mut data).await.is_some());
}

#[tokio::test]
#[expect(
    non_snake_case,
    reason = "KiB is the unit's name; the plan names the test so"
)]
async fn a_control_message_over_1_KiB_closes() {
    let relay = start(roomy()).await;
    let key = new_key();
    let mut control = register(relay.addr, &key).await;
    // An unknown message of exactly 1 KiB is ignored.
    let at_limit = json!({"pad": "x".repeat(1024 - 10)}).to_string();
    assert_eq!(at_limit.len(), 1024);
    control.send(text(&at_limit)).await.unwrap();
    let _phone = open(relay.addr, &phone_path(&channel_of(&key))).await;
    next_connect(&mut control).await;
    let over = json!({"pad": "x".repeat(1025 - 10)}).to_string();
    control.send(text(&over)).await.unwrap();
    assert!(closed_with(&mut control).await.is_some());
}

#[tokio::test]
async fn an_unspliced_phone_closes_4408() {
    let relay = start(roomy()).await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let started = Instant::now();
    let mut phone = open(relay.addr, &phone_path(&channel)).await;
    let id = next_connect(&mut control).await;
    pause();
    assert_eq!(closed_with(&mut phone).await, Some(4408));
    assert!(
        started.elapsed() >= Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    resume();
    assert_eq!(
        status(relay.addr, &data_path(&channel, &id), &[]).await,
        404
    );
}

#[tokio::test]
async fn a_silent_connection_is_dropped_after_two_pings() {
    let every = Duration::from_millis(100);
    let relay = start(with_limits(Limits {
        ping_every: every,
        ..roomy_limits()
    }))
    .await;
    let (silent_key, chatty_key) = (new_key(), new_key());
    let mut silent = register(relay.addr, &silent_key).await;
    let mut chatty = register(relay.addr, &chatty_key).await;
    // The chatty hub reads, so its pings are answered; the silent one reads nothing.
    let reading = async {
        while let Some(Ok(message)) = chatty.next().await {
            assert!(
                matches!(message, Message::Ping(_)),
                "unexpected {message:?}"
            );
        }
    };
    assert!(
        timeout(every * 6, reading).await.is_err(),
        "the chatty hub was dropped"
    );
    let mut pings = 0;
    let ended = within(async {
        loop {
            match silent.next().await {
                Some(Ok(Message::Ping(_))) => pings += 1,
                Some(Ok(Message::Close(frame))) => return frame.map(|frame| u16::from(frame.code)),
                Some(Ok(message)) => panic!("unexpected {message:?}"),
                Some(Err(_)) | None => return None,
            }
        }
    })
    .await;
    assert_eq!(pings, 2);
    assert_eq!(ended, Some(4408));
    let _phone = open(relay.addr, &phone_path(&channel_of(&chatty_key))).await;
    next_connect(&mut chatty).await;
}

#[tokio::test]
async fn the_byte_rate_limit_delays_but_delivers_a_burst() {
    let relay = start(with_limits(Limits {
        bytes_per_second: 200_000,
        burst_bytes: 200_000,
        ..roomy_limits()
    }))
    .await;
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let (mut phone, mut data) = spliced(relay.addr, &mut control, &channel).await;
    let messages: Vec<Vec<u8>> = (0..8_u8).map(|at| vec![at; 50_000]).collect();
    let started = Instant::now();
    let sending = async {
        for message in &messages {
            phone.send(binary(message)).await.unwrap();
        }
    };
    let receiving = async {
        let mut burst_at = None;
        for (at, message) in messages.iter().enumerate() {
            assert_eq!(&next_binary(&mut data).await, message);
            if at == 3 {
                burst_at = Some(started.elapsed());
            }
        }
        (burst_at.unwrap(), started.elapsed())
    };
    let ((), (burst, all)) = tokio::join!(sending, receiving);
    // The first 200,000 bytes are the burst and pass at once; the next 200,000 wait for the rate.
    assert!(
        burst < Duration::from_millis(500),
        "the burst took {burst:?}"
    );
    assert!(
        all >= Duration::from_millis(700),
        "everything took only {all:?}"
    );
}

#[tokio::test]
async fn connects_per_address_are_limited() {
    let limits = Limits {
        connects_per_minute: 2,
        ..roomy_limits()
    };
    let no_hub = phone_path(&channel_of(&new_key()));
    let phone_from = async |addr: SocketAddr, address: &str| {
        let mut phone = try_open(addr, &no_hub, &[(ADDRESS_HEADER, address)])
            .await
            .unwrap();
        closed_with(&mut phone).await
    };

    // Unset: the socket's address is counted and the header means nothing.
    let relay = start(with_limits(limits.clone())).await;
    assert_eq!(phone_from(relay.addr, "198.51.100.1").await, Some(4404));
    assert_eq!(phone_from(relay.addr, "198.51.100.2").await, Some(4404));
    assert_eq!(phone_from(relay.addr, "198.51.100.3").await, Some(4429));

    // Set: the header's address is counted.
    let relay = start(Config {
        limits,
        client_address_header: Some(ADDRESS_HEADER.to_owned()),
        ..Config::default()
    })
    .await;
    assert_eq!(phone_from(relay.addr, "198.51.100.1").await, Some(4404));
    assert_eq!(phone_from(relay.addr, "198.51.100.1").await, Some(4404));
    assert_eq!(phone_from(relay.addr, "198.51.100.1").await, Some(4429));
    assert_eq!(phone_from(relay.addr, "198.51.100.2").await, Some(4404));
}

#[tokio::test]
async fn registrations_per_address_are_limited() {
    let relay = start(with_limits(Limits {
        registrations_per_minute: 1,
        ..roomy_limits()
    }))
    .await;
    let _held = register(relay.addr, &new_key()).await;
    let mut refused = open(relay.addr, &hub_path(&channel_of(&new_key()))).await;
    assert_eq!(closed_with(&mut refused).await, Some(4429));
}

#[tokio::test]
async fn the_last_address_in_the_header_is_the_one_counted() {
    let relay = start(Config {
        limits: Limits {
            connects_per_minute: 1,
            ..roomy_limits()
        },
        client_address_header: Some("X-Forwarded-For".to_owned()),
        ..Config::default()
    })
    .await;
    let no_hub = phone_path(&channel_of(&new_key()));
    let phone_from = async |forwarded: &str| {
        let mut phone = try_open(relay.addr, &no_hub, &[("X-Forwarded-For", forwarded)])
            .await
            .unwrap();
        closed_with(&mut phone).await
    };
    assert_eq!(phone_from("198.51.100.7, 203.0.113.1").await, Some(4404));
    assert_eq!(phone_from("203.0.113.1, 198.51.100.8").await, Some(4404));
    assert_eq!(phone_from("198.51.100.9,203.0.113.1").await, Some(4429));
}

#[tokio::test]
async fn an_origin_of_another_host_is_403_and_the_relays_own_is_not() {
    let relay = start(roomy()).await;
    let no_hub = phone_path(&channel_of(&new_key()));
    let own = format!("http://{}", relay.addr);
    for foreign in ["https://evil.example", "null", "http://127.0.0.2"] {
        assert_eq!(
            status(relay.addr, &no_hub, &[("Origin", foreign)]).await,
            403,
            "{foreign}"
        );
    }
    assert_eq!(status(relay.addr, &no_hub, &[("Origin", &own)]).await, 101);
    assert_eq!(status(relay.addr, &no_hub, &[]).await, 101);
}

#[tokio::test]
async fn healthz_answers_and_any_other_path_is_404() {
    let relay = start(roomy()).await;
    let response = http(relay.addr, "GET /healthz HTTP/1.1\r\nHost: relay\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(response.ends_with("\r\n\r\nok"), "{response}");
    for path in ["/", "/healthzz", "/v1/hub", "/v1/phone/", "/v1/pair"] {
        let response = http(
            relay.addr,
            &format!("GET {path} HTTP/1.1\r\nHost: relay\r\n\r\n"),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 404 "), "{path}: {response}");
    }
    let channel = channel_of(&new_key());
    let id = hex(&[1; 16]);
    for path in [
        "/nowhere".to_owned(),
        format!("/v1/phone/{}", channel.to_uppercase()),
        format!("/v1/phone/{}", &channel[1..]),
        format!("/v1/hub/{channel}/{id}/more"),
        format!("/v1/hub/{channel}/{}", &id[1..]),
        format!("/v1/hub/{channel}?x=1"),
    ] {
        assert_eq!(status(relay.addr, &path, &[]).await, 404, "{path}");
    }
}

#[tokio::test]
async fn logs_carry_no_channel_id_or_address() {
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    let address = "203.0.113.77";
    let relay = start(Config {
        limits: Limits {
            phones_per_channel: 1,
            ..roomy_limits()
        },
        client_address_header: Some(ADDRESS_HEADER.to_owned()),
        log: Log::new(move |line| sink.lock().unwrap().push(line.to_owned())),
    })
    .await;
    let from = [(ADDRESS_HEADER, address)];
    let key = new_key();
    let channel = channel_of(&key);
    let mut control = register(relay.addr, &key).await;
    let mut phone = try_open(relay.addr, &phone_path(&channel), &from)
        .await
        .unwrap();
    let id = next_connect(&mut control).await;
    let mut data = open(relay.addr, &data_path(&channel, &id)).await;
    phone.send(binary(b"hi")).await.unwrap();
    assert_eq!(next_binary(&mut data).await, b"hi");
    let mut extra = try_open(relay.addr, &phone_path(&channel), &from)
        .await
        .unwrap();
    assert_eq!(closed_with(&mut extra).await, Some(4429));
    assert_eq!(
        status(relay.addr, &data_path(&channel, &id), &from).await,
        404
    );
    let mut stranger = open(relay.addr, &hub_path(&channel)).await;
    let challenge = challenge_of(&mut stranger).await;
    let other = new_key();
    stranger
        .send(answer(&other, &other, &signed(&challenge)))
        .await
        .unwrap();
    assert_eq!(closed_with(&mut stranger).await, Some(4401));
    phone.close(None).await.unwrap();
    closed_with(&mut data).await;
    control.close(None).await.unwrap();
    closed_with(&mut control).await;
    // Stopping writes the counts.
    drop(relay);
    let started = Instant::now();
    while !lines
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("stopped"))
    {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the relay did not stop"
        );
        sleep(Duration::from_millis(20)).await;
    }

    let lines = lines.lock().unwrap().clone();
    let counts = lines
        .iter()
        .find(|line| line.contains("sockets"))
        .expect("a counts line");
    assert!(counts.contains("bad registrations 1"), "{counts}");
    assert!(counts.contains(", spliced 1,"), "{counts}");
    assert!(counts.contains("refused by a limit 1,"), "{counts}");
    for line in &lines {
        for secret in [
            channel.as_str(),
            &channel[..16],
            id.as_str(),
            address,
            "127.0.0.1",
        ] {
            assert!(!line.contains(secret), "{line:?} names {secret}");
        }
    }
}

#[tokio::test]
async fn the_probe_passes_against_a_running_relay_and_fails_against_a_closed_port() {
    let relay = start(roomy()).await;
    probe(&format!("ws://{}", relay.addr)).await.unwrap();
    probe(&format!("ws://{}/", relay.addr)).await.unwrap();

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let why = probe(&format!("ws://{addr}")).await.unwrap_err();
    assert!(!why.is_empty());
}
