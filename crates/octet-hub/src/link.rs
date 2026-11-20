//! The pairing link: `octet://pair/<base64url>` for a phone to open, and the relay addresses it
//! and `hub.json` may carry.
use crate::{apns::base64url, store::unhex};

/// The scheme and host the app registers for its pairing links.
const PREFIX: &str = "octet://pair/";
/// The link's version byte for a relay reached over `wss`.
const SECURE: u8 = 1;
/// The link's version byte for a relay reached over `ws`, which only debug builds make.
const PLAIN: u8 = 0;
/// Whether this build makes `ws` links: debug builds only, for tests and a relay on the same
/// network.
const PLAIN_ALLOWED: bool = cfg!(debug_assertions);
/// The longest `host` or `host:port` a link carries, in bytes.
const AUTHORITY_LIMIT: usize = 253;
/// The longest label of a host name, in bytes.
const LABEL_LIMIT: usize = 63;

/// Splits the relay address `url`, `wss://<host>` or `wss://<host>:<port>` (or `ws://` for a
/// plain one), into whether it is secure and its `host` or `host:port`. Anything else is none: a
/// path, a query, a fragment, a user name, an IP literal in brackets, an empty or over-long host.
pub(crate) fn split_relay(url: &str) -> Option<(bool, &str)> {
    let (secure, authority) = match url.split_once("://")? {
        ("wss", authority) => (true, authority),
        ("ws", authority) => (false, authority),
        _ => return None,
    };
    is_authority(authority).then_some((secure, authority))
}

/// Whether `text` is a `host` or `host:port`: letters, digits and hyphens in dot-separated
/// labels, then a port from 1 to 65535 if there is a colon.
fn is_authority(text: &str) -> bool {
    if text.is_empty() || text.len() > AUTHORITY_LIMIT {
        return false;
    }
    let (host, port) = match text.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (text, None),
    };
    let port_ok = port.is_none_or(|port| {
        port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port != 0)
    });
    port_ok && host.split('.').all(is_label)
}

/// Whether `label` is 1 to 63 letters, digits or hyphens, none at either end.
fn is_label(label: &str) -> bool {
    (1..=LABEL_LIMIT).contains(&label.len())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// The pairing link for the hub at `relay`: `octet://pair/` and the unpadded base64url of one
/// version byte (`1` for a `wss` relay, `0` for `ws`), the hub's 32-byte `identity` public key,
/// the 32-byte `channel` id, the 32-byte `secret`, and the relay's `host` or `host:port` in
/// ASCII. `channel` and `secret` are the hex the hub keeps them as. None when `relay` is not an
/// address [`split_relay`] takes, is a `ws` one in a release build (only debug builds make such
/// a link), or either is not 64 lowercase hex digits.
pub(crate) fn pair_link(
    relay: &str,
    identity: &[u8; 32],
    channel: &str,
    secret: &str,
) -> Option<String> {
    let (secure, authority) = split_relay(relay)?;
    if !secure && !PLAIN_ALLOWED {
        return None;
    }
    let channel = unhex::<32>(channel)?;
    let secret = unhex::<32>(secret)?;
    let mut payload = Vec::with_capacity(97 + authority.len());
    payload.push(if secure { SECURE } else { PLAIN });
    payload.extend_from_slice(identity);
    payload.extend_from_slice(&channel);
    payload.extend_from_slice(&secret);
    payload.extend_from_slice(authority.as_bytes());
    Some(format!("{PREFIX}{}", base64url(&payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [u8; 32] = [7; 32];
    const CHANNEL: &str = "0a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728ff";
    const SECRET: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

    /// The bytes of `text`, base64url without padding, decoded apart from the code under test.
    fn decode(text: &str) -> Vec<u8> {
        let value = |c: char| {
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
                .find(c)
                .expect("a base64url character")
        };
        let mut bytes = Vec::new();
        let (mut bits, mut held) = (0_u32, 0_u32);
        for c in text.chars() {
            bits = bits << 6 | u32::try_from(value(c)).expect("a sextet");
            held += 6;
            if held >= 8 {
                held -= 8;
                bytes.push(u8::try_from(bits >> held & 0xff).expect("a byte"));
            }
        }
        bytes
    }

    #[test]
    fn the_link_decodes_to_its_parts() {
        let link =
            pair_link("wss://relay.example:8443", &IDENTITY, CHANNEL, SECRET).expect("a link");
        let payload = decode(link.strip_prefix("octet://pair/").expect("the scheme"));
        assert_eq!(payload.len(), 97 + "relay.example:8443".len());
        assert_eq!(payload[0], 1);
        assert_eq!(payload[1..33], IDENTITY);
        assert_eq!(payload[33..65], unhex::<32>(CHANNEL).expect("hex"));
        assert_eq!(payload[65..97], unhex::<32>(SECRET).expect("hex"));
        assert_eq!(&payload[97..], b"relay.example:8443");
        let text = link.strip_prefix("octet://pair/").expect("the scheme");
        let url_safe = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
        assert!(text.bytes().all(url_safe), "{link}");
    }

    #[test]
    fn only_a_debug_build_makes_a_ws_link() {
        assert_eq!(PLAIN_ALLOWED, cfg!(debug_assertions));
        let plain = pair_link("ws://127.0.0.1:9000", &IDENTITY, CHANNEL, SECRET);
        assert_eq!(plain.is_some(), cfg!(debug_assertions));
    }

    #[test]
    #[cfg(debug_assertions)]
    fn a_ws_relay_makes_a_version_0_link() {
        let link = pair_link("ws://127.0.0.1:9000", &IDENTITY, CHANNEL, SECRET).expect("a link");
        let payload = decode(link.strip_prefix("octet://pair/").expect("the scheme"));
        assert_eq!(payload[0], 0);
        assert_eq!(&payload[97..], b"127.0.0.1:9000");
        let bare = pair_link("wss://relay.example", &IDENTITY, CHANNEL, SECRET).expect("a link");
        let payload = decode(bare.strip_prefix("octet://pair/").expect("the scheme"));
        assert_eq!(&payload[97..], b"relay.example");
    }

    #[test]
    fn a_bad_relay_address_makes_no_link() {
        let long = format!("wss://{}.example", "a".repeat(64));
        let too_long = format!(
            "wss://{}",
            std::iter::repeat_n("a".repeat(60), 5)
                .collect::<Vec<_>>()
                .join(".")
        );
        for bad in [
            "",
            "relay.example",
            "https://relay.example",
            "wss://",
            "wss://relay.example/",
            "wss://relay.example/path",
            "wss://relay.example?x=1",
            "wss://relay.example#top",
            "wss://me@relay.example",
            "wss://me:pw@relay.example",
            "wss://relay.example:",
            "wss://relay.example:0",
            "wss://relay.example:65536",
            "wss://relay.example:80:80",
            "wss://relay.example:+80",
            "wss://[::1]:8443",
            "wss://relay..example",
            "wss://-relay.example",
            "wss://relay-.example",
            "wss://relay.example.:443/",
            "wss://re lay.example",
            &long,
            &too_long,
        ] {
            assert_eq!(pair_link(bad, &IDENTITY, CHANNEL, SECRET), None, "{bad:?}");
        }
        // The secret and the channel are 32 bytes of lowercase hex too.
        for (channel, secret) in [
            (&CHANNEL[2..], SECRET),
            (CHANNEL, &SECRET[2..]),
            (&CHANNEL.to_uppercase(), SECRET),
            (CHANNEL, "zz"),
        ] {
            assert_eq!(
                pair_link("wss://relay.example", &IDENTITY, channel, secret),
                None
            );
        }
    }

    #[test]
    fn a_relay_is_split_into_its_security_and_its_authority() {
        assert_eq!(
            split_relay("wss://relay.example"),
            Some((true, "relay.example"))
        );
        assert_eq!(
            split_relay("ws://localhost:80"),
            Some((false, "localhost:80"))
        );
        assert_eq!(
            split_relay("wss://relay.octet.invalid"),
            Some((true, "relay.octet.invalid"))
        );
        assert_eq!(split_relay("WSS://relay.example"), None);
    }
}
