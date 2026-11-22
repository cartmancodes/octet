/**
 * The phone's end of the encrypted channel to a hub: the initiator of `Noise_IK_25519_ChaChaPoly_SHA256`
 * (Noise rev 34), then frames in chunks and encrypted pings. The hub's end is
 * crates/octet-hub/src/secure.rs, and both are checked against the vectors it writes.
 *
 * The primitives are noble's (X25519, ChaCha20-Poly1305, SHA-256, HMAC-based HKDF), which are
 * audited and written to run in constant time; only the Noise state machine is here, for this one
 * pattern with no options. The phone knows the hub's static key from the pairing link and starts:
 * `-> e, es, s, ss` (96 bytes), `<- e, ee, se` (48 bytes), both with empty payloads.
 *
 * A transport plaintext is one type byte and its body:
 * - 0 (more) or 1 (last): 1 to 65,000 bytes of a UTF-8 frame; 1 ends the frame.
 * - 2 (ping) or 3 (pong): exactly 8 bytes; a pong carries its ping's bytes back.
 * Anything else, or a message that does not decrypt as the next one, ends the channel.
 *
 * Secrets are held in `Uint8Array`s and overwritten with zeros once they are done with, but
 * JavaScript cannot promise they are gone: the engine may have copied them, and noble's own
 * intermediate values are left to the garbage collector.
 */
import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";

import { MAX_FRAME_BYTES } from "./frames";

/** An X25519 key pair: the phone's static key, kept in the keychain. */
export interface KeyPair {
  publicKey: Uint8Array;
  privateKey: Uint8Array;
}

/**
 * The phone's end of the channel after its handshake. Each message is one WebSocket binary
 * message, in order: a message lost, repeated or reordered fails to open. So every message this
 * makes (from `seal`, `ping`, or a `reply` from `open`) must be sent, in the order it was made.
 */
export interface Channel {
  /** `frame` as the messages to send, in order: chunks of at most 65,000 bytes, all of them before
   * any other message, so frames never interleave. None for an empty frame, or once the channel
   * has failed. */
  seal(frame: string): Uint8Array[];
  /**
   * Opens the next message from the hub. `{ frame }` is a whole frame. `{ reply }` is the sealed
   * pong to the hub's ping, for the caller to send. `"pong"` is the hub's answer to the last
   * `ping`. `"more"` is nothing to hand on yet: a chunk was kept, or a chunk of a frame past
   * `MAX_FRAME_BYTES` was dropped (that frame is dropped whole, and the next one opens). `null`
   * is a failure that must close the socket; every later call returns null too.
   */
  open(message: Uint8Array): { frame: string } | { reply: Uint8Array } | "pong" | "more" | null;
  /** A sealed ping of 8 bytes from `random`, for the caller to send; `"pong"` from `open` is its
   * answer. Null if the last ping is still unanswered (the hub is gone: close the socket), or once
   * the channel has failed. */
  ping(random: (n: number) => Uint8Array): Uint8Array | null;
}

/** The Noise protocol name, both sides. Exactly 32 bytes, so it is the first hash as it is. */
const PATTERN = "Noise_IK_25519_ChaChaPoly_SHA256";
/** Hashed into the handshake on both sides, so a later incompatible version fails it. */
const PROLOGUE = "octet-link-v1";
/** The hub's handshake message: its ephemeral key and an empty payload. */
const SECOND = 48;
/** What encryption adds to a plaintext. */
const TAG = 16;
/** The largest Noise message. */
const MAX_NOISE = 65_535;
/** The most bytes of a frame one chunk carries. */
const CHUNK = 65_000;
/** Type byte: a chunk with more to follow. */
const MORE = 0;
/** Type byte: a frame's last chunk. */
const LAST = 1;
/** Type byte: a ping of 8 bytes. */
const PING = 2;
/** Type byte: a pong, carrying its ping's 8 bytes. */
const PONG = 3;

const EMPTY = new Uint8Array(0);
const encoder = new TextEncoder();

/** A key pair whose private key is 32 bytes from `random` (X25519 clamps it when used). */
export function newKeyPair(random: (n: number) => Uint8Array): KeyPair {
  const privateKey = random(32);
  return { publicKey: x25519.getPublicKey(privateKey), privateKey };
}

/**
 * Starts a handshake with the hub whose static public key is `hubKey`, as the phone whose static
 * key is `local`, with a fresh ephemeral key from `random`. Send `message1` (96 bytes), then hand
 * the hub's answer to `finish`, once: it returns the channel, or null for an answer of any size
 * but 48 bytes, one that does not decrypt, or an all-zero shared secret. The ephemeral private key
 * and the handshake's state are wiped when `finish` returns, whatever it returns.
 *
 * A hub key that gives an all-zero shared secret (a point of small order, as a forged pairing link
 * could carry) makes no handshake: `message1` is empty, so there is nothing to send, and `finish`
 * returns null.
 */
export function initiate(
  local: KeyPair,
  hubKey: Uint8Array,
  random: (n: number) => Uint8Array,
): { message1: Uint8Array; finish(message2: Uint8Array): Channel | null } {
  const ephemeral = random(32);
  const state = new Symmetric();
  /** Set once the handshake is over, by `finish` or a refusal: its secrets are wiped. */
  let over = false;
  const end = (): void => {
    over = true;
    ephemeral.fill(0);
    state.wipe();
  };

  let message1: Uint8Array;
  try {
    state.mixHash(encoder.encode(PROLOGUE));
    state.mixHash(hubKey);
    // -> e, es, s, ss, with an empty payload.
    const ephemeralPublic = x25519.getPublicKey(ephemeral);
    state.mixHash(ephemeralPublic);
    if (!state.mixDh(ephemeral, hubKey)) throw new Error("all-zero shared secret");
    const staticSealed = state.encryptAndHash(local.publicKey);
    if (!state.mixDh(local.privateKey, hubKey)) throw new Error("all-zero shared secret");
    message1 = concat([ephemeralPublic, staticSealed, state.encryptAndHash(EMPTY)]);
  } catch {
    // Also how noble refuses a key of the wrong length.
    end();
    return { message1: EMPTY, finish: () => null };
  }

  const finish = (message2: Uint8Array): Channel | null => {
    if (over) return null;
    try {
      // <- e, ee, se, with an empty payload.
      if (message2.length !== SECOND) return null;
      const hubEphemeral = message2.subarray(0, 32);
      state.mixHash(hubEphemeral);
      if (!state.mixDh(ephemeral, hubEphemeral)) return null;
      if (!state.mixDh(local.privateKey, hubEphemeral)) return null;
      const payload = state.decryptAndHash(message2.subarray(32));
      if (payload === null || payload.length !== 0) return null;
      const [toHub, toPhone] = state.split();
      const channel = channelOver(toHub, toPhone);
      toHub.fill(0);
      toPhone.fill(0);
      return channel;
    } catch {
      return null;
    } finally {
      end();
    }
  };
  return { message1, finish };
}

/**
 * A channel over transport keys already agreed: `sendKey` seals what the phone sends, and
 * `receiveKey` opens what it receives, each with its own nonce from zero. `initiate` makes the
 * app's; the speed measurement pairs two with the keys swapped. The keys are copied.
 */
export function channelOver(sendKey: Uint8Array, receiveKey: Uint8Array): Channel {
  const send = new Cipher(sendKey.slice());
  const receive = new Cipher(receiveKey.slice());
  /** The chunks of the frame being joined, and their bytes. */
  let joining: Uint8Array[] = [];
  let joined = 0;
  /** Set while the chunks of a frame past MAX_FRAME_BYTES are dropped, until its last. */
  let dropping = false;
  /** The bytes of the phone's ping that awaits its pong. */
  let awaiting: Uint8Array | null = null;
  /** Set once anything failed: nothing more passes. */
  let dead = false;

  const fail = (): null => {
    dead = true;
    joining = [];
    send.wipe();
    receive.wipe();
    return null;
  };

  return {
    seal(frame) {
      if (dead) return [];
      const bytes = encoder.encode(frame);
      const out: Uint8Array[] = [];
      for (let at = 0; at < bytes.length; at += CHUNK) {
        const end = Math.min(at + CHUNK, bytes.length);
        const chunk = new Uint8Array(1 + end - at);
        chunk[0] = end === bytes.length ? LAST : MORE;
        chunk.set(bytes.subarray(at, end), 1);
        out.push(send.encrypt(chunk));
      }
      return out;
    },

    open(message) {
      if (dead || message.length > MAX_NOISE) return fail();
      const plain = receive.decrypt(message);
      if (plain === null) return fail();
      const kind = plain[0];
      const body = plain.subarray(1);
      if ((kind === MORE || kind === LAST) && body.length >= 1 && body.length <= CHUNK) {
        if (!dropping && joined + body.length > MAX_FRAME_BYTES) {
          // Too big for the app: dropped up to its last chunk, as an oversized text frame was.
          dropping = true;
          joining = [];
          joined = 0;
        }
        if (dropping) {
          if (kind === LAST) dropping = false;
          return "more";
        }
        joining.push(body);
        joined += body.length;
        if (kind === MORE) return "more";
        const frame = decodeUtf8(joining.length === 1 ? body : concat(joining));
        joining = [];
        joined = 0;
        return frame === null ? fail() : { frame };
      }
      if ((kind === PING || kind === PONG) && body.length === 8) {
        if (kind === PING) {
          const pong = new Uint8Array(9);
          pong[0] = PONG;
          pong.set(body, 1);
          return { reply: send.encrypt(pong) };
        }
        if (awaiting === null || !equal(awaiting, body)) return fail();
        awaiting = null;
        return "pong";
      }
      return fail();
    },

    ping(random) {
      if (dead || awaiting !== null) return null;
      const bytes = random(8);
      if (bytes.length !== 8) return null;
      awaiting = bytes.slice();
      const ping = new Uint8Array(9);
      ping[0] = PING;
      ping.set(bytes, 1);
      return send.encrypt(ping);
    },
  };
}

/** `code` (six digits) as two groups of three, the way the Mac shows it: `"482913"` → `"482 913"`. */
export const showCode = (code: string): string => `${code.slice(0, 3)} ${code.slice(3)}`;

/** Noise's CipherState for ChaChaPoly: a key and a counting nonce. */
class Cipher {
  /** The next nonce. A number counts far past any connection's messages (2^53). */
  private nonce = 0;

  constructor(private readonly key: Uint8Array) {}

  /** `plain` sealed under the next nonce, with associated data `ad`. */
  encrypt(plain: Uint8Array, ad: Uint8Array = EMPTY): Uint8Array {
    return chacha20poly1305(this.key, nonce(this.nonce++), ad).encrypt(plain);
  }

  /** `sealed` opened under the next nonce with associated data `ad`, or null if it does not
   * open. A failure spends no nonce, but every caller ends the channel or handshake on one. */
  decrypt(sealed: Uint8Array, ad: Uint8Array = EMPTY): Uint8Array | null {
    if (sealed.length < TAG) return null;
    try {
      const plain = chacha20poly1305(this.key, nonce(this.nonce), ad).decrypt(sealed);
      this.nonce += 1;
      return plain;
    } catch {
      return null;
    }
  }

  /** Overwrites the key with zeros. */
  wipe(): void {
    this.key.fill(0);
  }
}

/** Noise's SymmetricState for SHA-256: the chaining key, the hash, and the cipher once keyed. */
class Symmetric {
  /** The chaining key. */
  private ck: Uint8Array;
  /** The handshake hash. */
  private h: Uint8Array;
  /** Set by the first MixKey; until then EncryptAndHash only hashes. */
  private cipher: Cipher | null = null;

  constructor() {
    // A name of exactly 32 bytes is the first hash as it is.
    this.h = encoder.encode(PATTERN);
    this.ck = this.h.slice();
  }

  /** MixHash: h = SHA-256(h || data). */
  mixHash(data: Uint8Array): void {
    this.h = sha256(concat([this.h, data]));
  }

  /** MixKey with the X25519 result of `privateKey` and `publicKey`; false (and nothing mixed) if
   * it is all zero, as it is for a public key of small order. noble throws for those already;
   * the check here does not rely on it. */
  mixDh(privateKey: Uint8Array, publicKey: Uint8Array): boolean {
    let shared: Uint8Array;
    try {
      shared = x25519.getSharedSecret(privateKey, publicKey);
    } catch {
      return false;
    }
    // Every byte looked at, so the time taken does not depend on where a non-zero one is.
    const zero = shared.reduce((any, byte) => any | byte, 0) === 0;
    if (!zero) this.mixKey(shared);
    shared.fill(0);
    return !zero;
  }

  /** EncryptAndHash: `plain` sealed with h as associated data (or as it is before a key), then
   * hashed in. */
  encryptAndHash(plain: Uint8Array): Uint8Array {
    const sealed = this.cipher === null ? plain.slice() : this.cipher.encrypt(plain, this.h);
    this.mixHash(sealed);
    return sealed;
  }

  /** DecryptAndHash: the plaintext of `sealed` with h as associated data, or null if it does not
   * open. Only called once keyed. */
  decryptAndHash(sealed: Uint8Array): Uint8Array | null {
    const plain = this.cipher?.decrypt(sealed, this.h) ?? null;
    if (plain !== null) this.mixHash(sealed);
    return plain;
  }

  /** Split: the two transport keys, the initiator's sending key first. */
  split(): [Uint8Array, Uint8Array] {
    const keys = hkdf(sha256, EMPTY, this.ck, EMPTY, 64);
    const out: [Uint8Array, Uint8Array] = [keys.slice(0, 32), keys.slice(32)];
    keys.fill(0);
    return out;
  }

  /** Overwrites the chaining key, the hash and the cipher's key with zeros. */
  wipe(): void {
    this.ck.fill(0);
    this.h.fill(0);
    this.cipher?.wipe();
    this.cipher = null;
  }

  /** MixKey: a new chaining key and cipher key from HKDF(ck, `input`). */
  private mixKey(input: Uint8Array): void {
    const keys = hkdf(sha256, input, this.ck, EMPTY, 64);
    this.ck.fill(0);
    this.ck = keys.slice(0, 32);
    this.cipher?.wipe();
    this.cipher = new Cipher(keys.slice(32));
    keys.fill(0);
  }
}

/** The 12-byte ChaChaPoly nonce of counter `n`: four zero bytes, then `n` as 64 bits little-endian. */
function nonce(n: number): Uint8Array {
  const out = new Uint8Array(12);
  const view = new DataView(out.buffer);
  view.setUint32(4, n >>> 0, true);
  view.setUint32(8, Math.floor(n / 2 ** 32), true);
  return out;
}

/** `parts` joined. */
function concat(parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/** Whether `a` and `b` hold the same bytes. Ping bytes are not secret, so no constant time. */
function equal(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i]);
}

/**
 * `bytes` as text, or null unless they are well-formed UTF-8 (Unicode table 3-7: no overlong
 * forms, no surrogates, nothing past U+10FFFF, nothing cut short). Hermes has a TextEncoder but no
 * TextDecoder, so the app decodes for itself.
 */
function decodeUtf8(bytes: Uint8Array): string | null {
  // UTF-16 never needs more units than UTF-8 has bytes.
  const units = new Uint16Array(bytes.length);
  let count = 0;
  let i = 0;
  while (i < bytes.length) {
    const lead = bytes[i]!;
    if (lead < 0x80) {
      units[count++] = lead;
      i += 1;
      continue;
    }
    let need: number;
    let point: number;
    // The allowed range of the byte after the lead; later ones are 0x80 to 0xbf.
    let low = 0x80;
    let high = 0xbf;
    if (lead >= 0xc2 && lead <= 0xdf) {
      need = 1;
      point = lead & 0x1f;
    } else if (lead >= 0xe0 && lead <= 0xef) {
      need = 2;
      point = lead & 0x0f;
      if (lead === 0xe0) low = 0xa0;
      if (lead === 0xed) high = 0x9f;
    } else if (lead >= 0xf0 && lead <= 0xf4) {
      need = 3;
      point = lead & 0x07;
      if (lead === 0xf0) low = 0x90;
      if (lead === 0xf4) high = 0x8f;
    } else {
      return null;
    }
    if (i + need >= bytes.length) return null;
    for (let k = 1; k <= need; k++) {
      const next = bytes[i + k]!;
      if (next < low || next > high) return null;
      low = 0x80;
      high = 0xbf;
      point = (point << 6) | (next & 0x3f);
    }
    i += need + 1;
    if (point >= 0x10000) {
      point -= 0x10000;
      units[count++] = 0xd800 + (point >> 10);
      units[count++] = 0xdc00 + (point & 0x3ff);
    } else {
      units[count++] = point;
    }
  }
  let text = "";
  // In slices, so no call has too many arguments.
  for (let at = 0; at < count; at += 8192) {
    text += String.fromCharCode(...units.subarray(at, Math.min(at + 8192, count)));
  }
  return text;
}
