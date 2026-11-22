//! `octet-relay probe <url>`: checks a running relay end to end, as a hub and a phone would use
//! it. `/healthz` only shows that the process is up; the probe shows that registration, the
//! `connect`, the data connection and the splice all work. It is also the reference client for
//! the relay's wire protocol.
use crate::{SIGNED_PREFIX, hex, unhex};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

/// How long the whole probe may take.
const PROBE_WITHIN: Duration = Duration::from_secs(20);
/// What the probe's phone sends its hub.
const FROM_PHONE: &[u8] = b"octet-relay probe: phone to hub";
/// What the probe's hub sends its phone.
const FROM_HUB: &[u8] = b"octet-relay probe: hub to phone";

/// A socket the probe opened.
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Probes the relay at `url` (`wss://host` or `ws://host:port`, a trailing `/` allowed): registers
/// a channel under a fresh key, connects a phone socket to it, opens the hub's data connection
/// for that phone, and passes one binary message each way.
///
/// # Errors
///
/// Which step failed, and why.
pub async fn probe(url: &str) -> Result<(), String> {
    let base = url.trim_end_matches('/');
    timeout(PROBE_WITHIN, run(base)).await.map_err(|_| {
        format!(
            "the probe did not finish within {} seconds",
            PROBE_WITHIN.as_secs()
        )
    })?
}

async fn run(base: &str) -> Result<(), String> {
    let mut secret = [0; 32];
    getrandom::fill(&mut secret).map_err(|error| format!("no random bytes: {error}"))?;
    let key = SigningKey::from_bytes(&secret);
    let channel = hex(&Sha256::digest(key.verifying_key().as_bytes()));

    let mut control = dial(
        &format!("{base}/v1/hub/{channel}"),
        "the hub's control connection",
    )
    .await?;
    let challenge = next_json(&mut control, "the challenge").await?;
    let challenge = challenge
        .get("challenge")
        .and_then(Value::as_str)
        .and_then(unhex::<32>)
        .ok_or_else(|| format!("expected a challenge, got {challenge}"))?;
    let signature = key.sign(&[SIGNED_PREFIX, challenge.as_slice()].concat());
    let answer = json!({
        "key": hex(key.verifying_key().as_bytes()),
        "signature": hex(&signature.to_bytes()),
    });
    send(
        &mut control,
        Message::Text(answer.to_string().into()),
        "the registration",
    )
    .await?;
    let registered = next_json(&mut control, "the registration's answer").await?;
    if registered != json!({"registered": true}) {
        return Err(format!(
            "expected {{\"registered\": true}}, got {registered}"
        ));
    }

    let mut phone = dial(
        &format!("{base}/v1/phone/{channel}"),
        "the phone connection",
    )
    .await?;
    let connect = next_json(&mut control, "the connect for the phone").await?;
    let id = connect
        .get("connect")
        .and_then(Value::as_str)
        .filter(|id| unhex::<16>(id).is_some())
        .ok_or_else(|| format!("expected a connect, got {connect}"))?;
    let mut data = dial(
        &format!("{base}/v1/hub/{channel}/{id}"),
        "the hub's data connection",
    )
    .await?;

    send(
        &mut phone,
        Message::Binary(FROM_PHONE.into()),
        "the phone's message",
    )
    .await?;
    if next_binary(&mut data, "the phone's message").await? != FROM_PHONE {
        return Err("the phone's message arrived changed".to_owned());
    }
    send(
        &mut data,
        Message::Binary(FROM_HUB.into()),
        "the hub's message",
    )
    .await?;
    if next_binary(&mut phone, "the hub's message").await? != FROM_HUB {
        return Err("the hub's message arrived changed".to_owned());
    }
    for mut socket in [phone, data, control] {
        let _ = socket.close(None).await;
    }
    Ok(())
}

/// Opens the WebSocket at `url`, called `what` in the error.
async fn dial(url: &str, what: &str) -> Result<Socket, String> {
    connect_async(url)
        .await
        .map(|(socket, _)| socket)
        .map_err(|error| format!("could not open {what}: {error}"))
}

async fn send(socket: &mut Socket, message: Message, what: &str) -> Result<(), String> {
    socket
        .send(message)
        .await
        .map_err(|error| format!("could not send {what}: {error}"))
}

/// The next message that is not a ping or pong; `what` names it in the error.
async fn next(socket: &mut Socket, what: &str) -> Result<Message, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(Some(frame)))) => {
                return Err(format!(
                    "the relay closed the socket with {} ({}) while the probe waited for {what}",
                    u16::from(frame.code),
                    frame.reason,
                ));
            }
            Some(Ok(message)) => return Ok(message),
            Some(Err(error)) => {
                return Err(format!(
                    "the socket failed while the probe waited for {what}: {error}"
                ));
            }
            None => {
                return Err(format!(
                    "the socket closed while the probe waited for {what}"
                ));
            }
        }
    }
}

async fn next_json(socket: &mut Socket, what: &str) -> Result<Value, String> {
    match next(socket, what).await? {
        Message::Text(text) => {
            serde_json::from_str(&text).map_err(|error| format!("{what} was not JSON: {error}"))
        }
        other => Err(format!("expected {what} as text, got {other:?}")),
    }
}

async fn next_binary(socket: &mut Socket, what: &str) -> Result<Vec<u8>, String> {
    match next(socket, what).await? {
        Message::Binary(bytes) => Ok(bytes.to_vec()),
        other => Err(format!("expected {what} as binary, got {other:?}")),
    }
}
