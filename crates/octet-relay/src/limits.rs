//! The relay's limits, and the two meters that enforce the rated ones: [`Addresses`], new
//! connections per client address, and [`ByteRate`], bytes read per connection.
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

/// What the relay allows. Over a limit the newcomer is refused; an established splice is never
/// dropped for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The largest binary message on a phone or data socket, in bytes; a larger one closes the
    /// socket (and its splice). A little over the 65,535 bytes a frame can carry.
    pub message_bytes: usize,
    /// Phone sockets one channel may hold at once, waiting and spliced alike; one more is closed
    /// with 4429.
    pub phones_per_channel: usize,
    /// Registered channels across the relay; a registration that would add one more is closed
    /// with 4429 (one that replaces a registered channel adds none).
    pub channels: usize,
    /// WebSocket connections open across the relay; an upgrade past it is answered 503.
    pub sockets: usize,
    /// Phone sockets one client address may open per minute; one more is closed with 4429.
    pub connects_per_minute: u32,
    /// Hub control sockets (registration attempts) one client address may open per minute; one
    /// more is closed with 4429.
    pub registrations_per_minute: u32,
    /// The rate at which the relay reads from one connection, in bytes a second, past the burst.
    pub bytes_per_second: u64,
    /// The bytes one connection may send at once before the rate applies: room for a journal
    /// replay.
    pub burst_bytes: u64,
    /// How long a phone socket may wait for its hub data connection (which is also how long a
    /// connection id is good for), how long a hub has to answer its challenge, and how long a
    /// client has to send its HTTP request.
    pub unspliced: Duration,
    /// How often every WebSocket is pinged; one that leaves two pings unanswered is dropped.
    pub ping_every: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            message_bytes: 66_560,
            phones_per_channel: 16,
            channels: 5_000,
            sockets: 20_000,
            connects_per_minute: 30,
            registrations_per_minute: 10,
            bytes_per_second: 262_144,
            burst_bytes: 4_194_304,
            unspliced: Duration::from_secs(10),
            ping_every: Duration::from_secs(20),
        }
    }
}

/// The span a per-minute allowance is spread over.
const MINUTE: Duration = Duration::from_secs(60);

/// New connections per client address: `per_minute` may come at once, and then one more each
/// `1/per_minute` of a minute (a generic cell rate, kept as one instant per address).
#[derive(Debug)]
pub(crate) struct Addresses {
    per_minute: u32,
    /// For each address, when its allowance is next whole again.
    full_at: HashMap<String, Instant>,
}

impl Addresses {
    /// A meter allowing `per_minute` new connections a minute to each address.
    pub(crate) fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            full_at: HashMap::new(),
        }
    }

    /// Takes one connection for `address` at `now`, or says no (and takes nothing).
    pub(crate) fn admit(&mut self, address: &str, now: Instant) -> bool {
        if self.per_minute == 0 {
            return false;
        }
        let each = MINUTE / self.per_minute;
        let full_at = self.full_at.get(address).map_or(now, |at| (*at).max(now));
        let next = full_at + each;
        if next > now + MINUTE {
            return false;
        }
        self.full_at.insert(address.to_owned(), next);
        true
    }

    /// Forgets the addresses whose allowance is whole again, so the table holds only recent ones.
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.full_at.retain(|_, at| *at > now);
    }
}

/// Bytes read from one connection: [`Limits::burst_bytes`] may come at once, and then
/// [`Limits::bytes_per_second`]. A message that overdraws the allowance is still delivered; the
/// next read waits until the allowance is back in credit.
#[derive(Debug)]
pub(crate) struct ByteRate {
    per_second: u64,
    burst: Duration,
    /// When the allowance is whole again.
    full_at: Instant,
}

impl ByteRate {
    /// A meter for `limits`, whole at `now`.
    pub(crate) fn new(limits: &Limits, now: Instant) -> Self {
        let mut rate = Self {
            per_second: limits.bytes_per_second.max(1),
            burst: Duration::ZERO,
            full_at: now,
        };
        rate.burst = rate.time_for(limits.burst_bytes);
        rate
    }

    /// How long `bytes` take at the rate.
    fn time_for(&self, bytes: u64) -> Duration {
        let nanos = u128::from(bytes) * 1_000_000_000 / u128::from(self.per_second);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// Takes `bytes` read at `now`.
    pub(crate) fn take(&mut self, bytes: usize, now: Instant) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.full_at = self.full_at.max(now) + self.time_for(bytes);
    }

    /// When the next read may be made: at once while the allowance is in credit.
    pub(crate) fn ready_at(&self) -> Instant {
        self.full_at.checked_sub(self.burst).unwrap_or(self.full_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_gets_its_minute_at_once_and_then_one_at_a_time() {
        let now = Instant::now();
        let mut addresses = Addresses::new(3);
        assert!((0..3).all(|_| addresses.admit("a", now)));
        assert!(!addresses.admit("a", now));
        assert!(addresses.admit("b", now));
        assert!(!addresses.admit("a", now + Duration::from_secs(19)));
        assert!(addresses.admit("a", now + Duration::from_secs(20)));
        addresses.sweep(now + Duration::from_secs(80));
        assert!(addresses.full_at.is_empty());
    }

    #[test]
    fn no_allowance_admits_nobody() {
        assert!(!Addresses::new(0).admit("a", Instant::now()));
    }

    #[test]
    fn the_burst_is_free_and_the_rest_waits_for_the_rate() {
        let now = Instant::now();
        let limits = Limits {
            bytes_per_second: 1_000,
            burst_bytes: 2_000,
            ..Limits::default()
        };
        let mut rate = ByteRate::new(&limits, now);
        rate.take(2_000, now);
        assert_eq!(rate.ready_at(), now);
        rate.take(500, now);
        assert_eq!(rate.ready_at(), now + Duration::from_millis(500));
        // A pause refills it, up to the burst and no further.
        let later = now + Duration::from_secs(60);
        rate.take(2_000, later);
        assert_eq!(rate.ready_at(), later);
    }
}
