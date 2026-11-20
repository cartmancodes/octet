/**
 * The pairing link the Mac shows: `octet://pair/<base64url, no padding>` of one version byte, the
 * hub's 32-byte identity key, its 32-byte channel id, the 32-byte one-time secret, and then the
 * relay's address as ASCII `host` or `host:port` (crates/octet-hub/src/link.rs). Version `1` is a
 * relay reached over `wss`; `0`, over `ws`, only debug builds of the hub make and only development
 * builds of the app take. Parsed by hand: React Native's `URL` is partial, and a strict shape is
 * the point.
 */

const PREFIX = "octet://pair/";
/** The link's version byte for a relay over `wss`. */
const SECURE = 1;
/** The link's version byte for a relay over `ws`, for development builds only. */
const PLAIN = 0;
/** The version byte and the three 32-byte parts before the address. */
const FIXED = 97;
/** The longest `host` or `host:port` a link carries, in bytes. */
const AUTHORITY_LIMIT = 253;
/** One label of a host name: 1 to 63 letters, digits or hyphens, none at either end. */
const LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/i;
const BASE64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/** Said for any text that is not a pairing link this app takes. */
export const NOT_A_LINK = "That isn't an Octet pairing link";

/** A pairing link's parts: the relay's WebSocket base (`wss://<authority>/`, or `ws://` for a
 * development link), and the hub's key, the channel id and the secret as lowercase hex. */
export interface PairLink {
  relay: string;
  hubKey: string;
  channel: string;
  secret: string;
}

/** `bytes` as lowercase hex. */
export const toHex = (bytes: Uint8Array): string => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

/** The bytes of `text`, lowercase hex of whole bytes; null for anything else. */
export function fromHex(text: string): Uint8Array | null {
  if (text.length % 2 !== 0 || !/^[0-9a-f]*$/.test(text)) return null;
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(text.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/** Whether `text` is 32 bytes of lowercase hex: a key, a channel id or a secret as kept. */
export const isHex32 = (text: string): boolean => /^[0-9a-f]{64}$/.test(text);

/** The bytes of unpadded base64url `text`, or null unless it is the one text of those bytes (no
 * padding, no other alphabet, no dangling sextet, no bits set past the last byte). */
function fromBase64url(text: string): Uint8Array | null {
  if (text.length % 4 === 1) return null;
  const out = new Uint8Array(Math.floor((text.length * 6) / 8));
  let bits = 0;
  let held = 0;
  let at = 0;
  for (const char of text) {
    const value = BASE64URL.indexOf(char);
    if (value < 0) return null;
    bits = ((bits << 6) | value) & 0xfff;
    held += 6;
    if (held >= 8) {
      held -= 8;
      out[at++] = (bits >> held) & 0xff;
    }
  }
  // The bits past the last whole byte are zero in the hub's encoding.
  return (bits & ((1 << held) - 1)) === 0 ? out : null;
}

/** `text` as `host` or `host:port` in its saved form (lower case, the port without leading zeros),
 * or null unless it is one: dot-separated labels, then a port from 1 to 65535 after a colon. */
function authorityOf(text: string): string | null {
  if (text.length === 0 || text.length > AUTHORITY_LIMIT) return null;
  const colon = text.indexOf(":");
  const host = colon < 0 ? text : text.slice(0, colon);
  const port = colon < 0 ? null : text.slice(colon + 1);
  if (!host.split(".").every((label) => LABEL.test(label))) return null;
  if (port === null) return host.toLowerCase();
  if (!/^[0-9]{1,5}$/.test(port)) return null;
  const number = Number(port);
  if (number < 1 || number > 65535) return null;
  return `${host.toLowerCase()}:${number}`;
}

/** The pairing link's parts, or null for anything that is not one: another scheme or path, a
 * payload that is not canonical base64url, an unknown version (or `0` outside `development`), or
 * an address that is not a `host` or `host:port` of at most 253 bytes. */
export function parsePairLink(text: string, development: boolean): PairLink | null {
  const trimmed = text.trim();
  if (!trimmed.startsWith(PREFIX)) return null;
  const payload = fromBase64url(trimmed.slice(PREFIX.length));
  if (payload === null || payload.length <= FIXED) return null;
  const version = payload[0];
  const scheme = version === SECURE ? "wss" : version === PLAIN && development ? "ws" : null;
  if (scheme === null) return null;
  let address = "";
  for (const byte of payload.subarray(FIXED)) {
    if (byte >= 0x80) return null;
    address += String.fromCharCode(byte);
  }
  const authority = authorityOf(address);
  if (authority === null) return null;
  return {
    relay: `${scheme}://${authority}/`,
    hubKey: toHex(payload.subarray(1, 33)),
    channel: toHex(payload.subarray(33, 65)),
    secret: toHex(payload.subarray(65, 97)),
  };
}

/** Why `text` cannot be paired with, in words for the user; null for a good pairing link. */
export function pairLinkProblem(text: string, development: boolean): string | null {
  return parsePairLink(text, development) === null ? NOT_A_LINK : null;
}

/** Whether `url` is a relay address this app would have saved from a pairing link: `wss://` (or,
 * in `development`, `ws://`), the link's address rules in their saved form, and a slash. A stored
 * value that fails is not trusted. */
export function isRelayUrl(url: string, development: boolean): boolean {
  const match = /^(wss|ws):\/\/([^/]*)\/$/.exec(url);
  if (!match) return false;
  const [, scheme, authority = ""] = match;
  if (scheme === "ws" && !development) return false;
  return authorityOf(authority) === authority;
}
