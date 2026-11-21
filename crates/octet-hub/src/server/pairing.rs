//! Pairing with the owner's yes at the Mac. A `pair` with a good secret spends the secret and is
//! held: the hub draws a six-digit code, sends it to that phone alone in a `pair_code` frame, and
//! puts the question to the Mac (`pair_wait` on the control socket). `pair_answer` with the
//! pairing's id saves the device, or refuses the phone; two minutes without an answer refuse it
//! too, as does a save that fails. A held pairing is bound to its connection and the key its
//! handshake proved: the answer can pair that key and no other, and the connection closing ends
//! it. One pairing is held at a time, and a `pair` meanwhile is refused `busy` with no code.
//!
//! Each `pair_secret` is an attempt, with an id of its own: its link comes back with the id, and
//! its `pair_wait` names it. A wait for any attempt but the newest hears `expired`, so two openers
//! (`octet pair` and the terminal UI, say) never take each other's phone. Attempts and held
//! pairings have random ids ([`fresh_id`]), never a count: a terminal still showing a question
//! from before the hub restarted cannot answer for a phone held since.
use super::{AfterSave, Hub};
use crate::{
    link::{pair_link, split_relay},
    store::{PAIR_NOT_SAVED, Pairing, SECRET_REFUSED, hex, pair_change},
};
use octet_core::remote::protocol::{Code, pair_code, paired};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};
use tokio::{sync::oneshot, time::Instant};

/// How long the owner has to answer a held pairing.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// How many codes there are: six digits.
const CODES: u32 = 1_000_000;
/// A draw at or past this, the last whole million a `u32` holds, is drawn again: every code is
/// then equally likely.
const DRAW_BELOW: u32 = u32::MAX / CODES * CODES;
/// The refusal of a `pair` while another is held.
const BUSY: &str = "Another phone is pairing";
/// The refusal of a held phone the owner said no to.
const REFUSED_AT_THE_MAC: &str = "Pairing was refused on the Mac";
/// The refusal of a held phone nobody answered for, and the answer to a late `pair_answer`.
const TIMED_OUT: &str = "Pairing timed out";
/// The answer to a `pair_answer` for a phone whose connection closed while it was held.
const PHONE_LEFT: &str = "The phone left";
/// The answer to a `pair_answer` naming no pairing the hub holds or has just let go.
const NOT_WAITING: &str = "No phone is waiting for that answer";
/// The answer to a `pair_wait` while another waits.
const ANOTHER_WAIT: &str = "Another pairing is open";
/// Why `pair_secret` gives no link while a phone is held.
pub(super) const PHONE_WAITS: &str = "A phone is waiting for an answer";

/// The pairing the hub holds for the owner's answer.
struct Held {
    /// Its id, which the answer names.
    pending: String,
    /// The phone's connection; the answer goes to it and to no other.
    conn: u64,
    /// The phone's `pair` req, which `pair_code`, `paired` and the refusals answer.
    req: u64,
    /// The device to save: the key the connection's handshake proved, with the phone's claims.
    pairing: Pairing,
    /// The attempt whose secret the phone used; `None` for one from before any opening.
    attempt: Option<String>,
    /// The six digits the phone and the Mac show.
    code: String,
    /// Whether a device of its name was paired when it was held.
    replaces: bool,
    /// When it is refused unanswered.
    deadline: Instant,
    /// Once the owner said yes: hears how the save went.
    saving: Option<oneshot::Sender<Value>>,
}

// The code stays out, as the secret does.
impl std::fmt::Debug for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Held")
            .field("pending", &self.pending)
            .field("conn", &self.conn)
            .field("pairing", &self.pairing)
            .field("saving", &self.saving.is_some())
            .finish_non_exhaustive()
    }
}

/// Where pairing stands: the phone held, the Mac waiting to hear of one, and how the last held
/// pairing ended, for an answer that comes too late.
#[derive(Debug, Default)]
pub(super) struct Pairings {
    /// The one pairing held.
    held: Option<Held>,
    /// The `pair_wait` waiting for a phone to be held.
    waiter: Option<oneshot::Sender<Value>>,
    /// The id of the last held pairing to end unanswered, and what an answer to it is told.
    ended: Option<(String, &'static str)>,
    /// The newest attempt: the id of the last `pair_secret` that made a secret.
    attempt: Option<String>,
}

/// What `pair_secret` came to.
pub(super) enum Opened {
    /// A new secret: its link, shown once the relay has the hub, the relay's host, which the
    /// reason names when it never does, and the attempt it opens.
    Link {
        link: String,
        host: String,
        attempt: String,
    },
    /// The relay's address cannot go in a pairing link.
    Unlinkable,
    /// A phone is held: no new secret.
    Waiting,
}

// The link carries the secret: the host shows, the link does not.
impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link { host, attempt, .. } => f
                .debug_struct("Link")
                .field("host", host)
                .field("attempt", attempt)
                .finish_non_exhaustive(),
            Self::Unlinkable => f.write_str("Unlinkable"),
            Self::Waiting => f.write_str("Waiting"),
        }
    }
}

impl Hub {
    /// A `pair` from the unauthenticated `conn`, whose handshake proved `key`: refused `busy`
    /// while another is held; refused, with the secret unspent, for a secret, a field or a key
    /// the store will not take; otherwise held, its secret spent, and the phone sent its code.
    pub(super) async fn pair(
        &mut self,
        (conn, req, key): (u64, u64, &str),
        secret: &str,
        name: &str,
        platform: &str,
    ) {
        if self.pairings.held.is_some() {
            self.refuse(conn, req, Code::Busy, BUSY);
            self.close(conn);
            return;
        }
        // A device paired or revoked by the CLI counts when the key and the name are judged.
        self.store.reload_if_changed().await;
        let now = SystemTime::now();
        let pairing = match self
            .store
            .begin_pair(secret, name, platform, key, &self.relay, now)
        {
            Ok(pairing) => pairing,
            Err(reason) => {
                let code = if reason == SECRET_REFUSED {
                    Code::Unpaired
                } else {
                    Code::Invalid
                };
                self.refuse(conn, req, code, reason);
                self.close(conn);
                return;
            }
        };
        let replaces = self
            .store
            .devices()
            .iter()
            .any(|device| device.name == pairing.device.name);
        let held = Held {
            pending: fresh_id(),
            conn,
            req,
            pairing,
            attempt: self.pairings.attempt.clone(),
            code: draw_code(),
            replaces,
            deadline: Instant::now() + ANSWER_WITHIN,
            saving: None,
        };
        let frame = pair_code(req, &held.code);
        if let Some(waiter) = self.pairings.waiter.take() {
            let _ = waiter.send(question(&held));
        }
        self.pairings.held = Some(held);
        // The relay link stays up for it, and its 10 seconds no longer run.
        self.await_answer(conn, true);
        self.send(conn, &frame);
    }

    /// `pair_secret`: a new secret in place of any no phone has used (a `pair_wait` on that one
    /// hears it expired), under a new attempt, which brings the relay link up; none while a
    /// phone is held.
    pub(super) fn open_pairing(&mut self) -> Opened {
        if self.pairings.held.is_some() {
            return Opened::Waiting;
        }
        let attempt = fresh_id();
        self.pairings.attempt = Some(attempt.clone());
        let secret = self.store.replace_pairing_secret(SystemTime::now());
        if let Some(waiter) = self.pairings.waiter.take() {
            let _ = waiter.send(expired());
        }
        self.want_relay();
        let identity = self.keys.identity_public();
        let link = pair_link(&self.relay, &identity, self.keys.channel(), &secret);
        match (link, split_relay(&self.relay)) {
            (Some(link), Some((_, host))) => Opened::Link {
                link,
                host: host.to_owned(),
                attempt,
            },
            _ => Opened::Unlinkable,
        }
    }

    /// Whether `attempt` is the newest: an opening no later `pair_secret` has replaced.
    pub(super) fn current_attempt(&self, attempt: &str) -> bool {
        self.pairings.attempt.as_deref() == Some(attempt)
    }

    /// `pair_wait` for `attempt`: `expired` for any attempt but the newest, or a phone held
    /// through another; else the held phone's question at once; `expired` with no pairing open;
    /// another waiter's refusal; or else, `reply` kept, the question once a phone is held.
    pub(super) fn wait_for_phone(&mut self, attempt: &str, reply: oneshot::Sender<Value>) {
        let held_elsewhere = self
            .pairings
            .held
            .as_ref()
            .is_some_and(|held| held.attempt.as_deref() != Some(attempt));
        if !self.current_attempt(attempt) || held_elsewhere {
            let _ = reply.send(expired());
        } else if let Some(held) = self.pairings.held.as_ref().filter(|h| h.saving.is_none()) {
            let _ = reply.send(question(held));
        } else if !self.store.pairing_open(SystemTime::now()) {
            let _ = reply.send(expired());
        } else if self
            .pairings
            .waiter
            .as_ref()
            .is_some_and(|w| !w.is_closed())
        {
            let _ = reply.send(json!({ "error": ANOTHER_WAIT }));
        } else {
            self.pairings.waiter = Some(reply);
        }
    }

    /// `pair_answer` for the held pairing `pending`: no refuses the phone; yes saves its device
    /// off the loop, and `reply` hears once it is saved ([`paired_saved`](Self::paired_saved)).
    pub(super) fn answer(&mut self, pending: String, accept: bool, reply: oneshot::Sender<Value>) {
        let Some(mut held) = self
            .pairings
            .held
            .take_if(|held| held.pending == pending && held.saving.is_none())
        else {
            let error = match self.pairings.ended {
                Some((ref ended, reason)) if *ended == pending => reason,
                _ => NOT_WAITING,
            };
            let _ = reply.send(json!({ "error": error }));
            return;
        };
        if !accept {
            self.let_go(&held, REFUSED_AT_THE_MAC);
            let _ = reply.send(json!({ "paired": false }));
            return;
        }
        // In place of a device of its name, whichever that is by now.
        let change = pair_change(held.pairing.device.clone());
        held.saving = Some(reply);
        self.pairings.held = Some(held);
        self.save(change, move |result| AfterSave::Paired { pending, result });
    }

    /// The device of the held pairing `pending` was saved, or not: the phone is told `paired`
    /// (and its connection closed: it says hello on one of its own), and the connections of a
    /// key it replaced are closed; or the phone is refused. The Mac hears either way.
    pub(super) async fn paired_saved(
        &mut self,
        pending: String,
        result: std::io::Result<Result<Option<String>, &'static str>>,
    ) {
        let Some(mut held) = self.pairings.held.take_if(|held| held.pending == pending) else {
            return;
        };
        let reply = held.saving.take();
        let answer = match result {
            Ok(Ok(replaced)) => {
                let name = held.pairing.device.name.clone();
                // Who paired, and when; never the key, the code or the secret.
                self.store.audit(&name, "", "paired").await;
                self.send(held.conn, &paired(&name, &self.name));
                self.close(held.conn);
                if let Some(old) = replaced {
                    self.close_key(&old);
                }
                json!({ "paired": true, "device": name })
            }
            failed => {
                // A kind or the store's reason; never the key.
                match failed {
                    Err(error) => eprintln!("Couldn't save a paired device: {}", error.kind()),
                    Ok(Err(reason)) => eprintln!("Couldn't save a paired device: {reason}"),
                    Ok(Ok(_)) => {}
                }
                self.let_go(&held, PAIR_NOT_SAVED);
                json!({ "error": PAIR_NOT_SAVED })
            }
        };
        self.want_relay();
        if let Some(reply) = reply {
            let _ = reply.send(answer);
        }
    }

    /// Every second: refuses the held phone whose two minutes are up, and answers `expired` to a
    /// waiter whose secret has run out (one whose client left is let go).
    pub(super) fn pairing_tick(&mut self, now: Instant) {
        if let Some(held) = self
            .pairings
            .held
            .take_if(|held| held.saving.is_none() && now >= held.deadline)
        {
            self.pairings.ended = Some((held.pending.clone(), TIMED_OUT));
            self.let_go(&held, TIMED_OUT);
        }
        if self
            .pairings
            .waiter
            .as_ref()
            .is_some_and(oneshot::Sender::is_closed)
        {
            self.pairings.waiter = None;
        }
        if self.pairings.held.is_none()
            && !self.store.pairing_open(SystemTime::now())
            && let Some(waiter) = self.pairings.waiter.take()
        {
            let _ = waiter.send(expired());
        }
    }

    /// `conn` is going: a pairing held on it, not yet answered yes, ends with it.
    pub(super) fn pairing_conn_gone(&mut self, conn: u64) {
        if let Some(held) = self
            .pairings
            .held
            .take_if(|held| held.conn == conn && held.saving.is_none())
        {
            self.pairings.ended = Some((held.pending, PHONE_LEFT));
        }
    }

    /// Refuses the phone of a held pairing, taken from [`Pairings`] already, with `reason`, and
    /// closes it; the relay link lingers so it hears.
    fn let_go(&mut self, held: &Held, reason: &str) {
        self.refuse(held.conn, held.req, Code::Unpaired, reason);
        self.close(held.conn);
        self.linger();
        self.want_relay();
    }

    /// Closes every connection of the device whose key was `key`, which a pairing replaced.
    fn close_key(&mut self, key: &str) {
        let gone: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, state)| state.device.as_ref().is_some_and(|d| d.key == key))
            .map(|(&conn, _)| conn)
            .collect();
        for conn in gone {
            self.close(conn);
        }
        self.buckets.remove(key);
    }
}

/// What `pair_wait` tells the Mac of a held pairing.
fn question(held: &Held) -> Value {
    json!({
        "pending": held.pending,
        "code": held.code,
        "name": held.pairing.device.name,
        "platform": held.pairing.device.platform,
        "replaces": held.replaces,
    })
}

/// What `pair_wait` hears when its secret is spent, replaced or out of time with no phone held.
fn expired() -> Value {
    json!({ "expired": true })
}

/// A pairing id: 16 random bytes as 32 lowercase hex digits, from the operating system's
/// randomness as the code is. Unguessable, and never one a hub made before it restarted.
///
/// # Panics
///
/// Panics if the operating system has no random bytes to give.
fn fresh_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("the operating system gave no random bytes");
    hex(&bytes)
}

/// Six digits from the operating system's randomness, every code equally likely: a draw past
/// the last whole million a `u32` holds is drawn again, never folded onto the codes below it.
///
/// # Panics
///
/// Panics if the operating system has no random bytes to give.
fn draw_code() -> String {
    loop {
        let mut bytes = [0; 4];
        getrandom::fill(&mut bytes).expect("the operating system gave no random bytes");
        let drawn = u32::from_le_bytes(bytes);
        if drawn < DRAW_BELOW {
            return format!("{:06}", drawn % CODES);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_six_digits_and_differ_between_pairings() {
        let codes: Vec<String> = (0..200).map(|_| draw_code()).collect();
        for code in &codes {
            assert!(
                code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
                "{code}"
            );
        }
        let distinct: HashSet<&String> = codes.iter().collect();
        assert!(distinct.len() >= 190, "{} distinct of 200", distinct.len());
    }

    #[test]
    fn debug_output_never_shows_the_link() {
        let opened = Opened::Link {
            link: "octet://pair/AQID-secret".into(),
            host: "relay.example".into(),
            attempt: fresh_id(),
        };
        let shown = format!("{opened:?}");
        assert!(
            !shown.contains("secret") && shown.contains("relay.example"),
            "{shown}"
        );
    }

    #[test]
    fn ids_are_32_hex_digits_and_differ() {
        let ids: HashSet<String> = (0..200).map(|_| fresh_id()).collect();
        assert_eq!(ids.len(), 200);
        for id in &ids {
            assert!(
                id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{id}"
            );
        }
    }

    #[test]
    fn a_draw_is_kept_only_below_the_last_whole_million() {
        assert_eq!(DRAW_BELOW, 4_294_000_000);
        assert_eq!(DRAW_BELOW % CODES, 0);
        assert!(u64::from(DRAW_BELOW) + u64::from(CODES) > u64::from(u32::MAX));
    }
}
