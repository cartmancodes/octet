import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { randomBytes } from "node:crypto";

import type { Socket } from "./HubClient";
import { MAX_FRAME_BYTES } from "./frames";
import { newKeyPair, type KeyPair } from "./noise";
import { secureSocketFactory, type RawSocket } from "./secureSocket";

const random = (n: number): Uint8Array => Uint8Array.from(randomBytes(n));
const concat = (parts: Uint8Array[]): Uint8Array => Uint8Array.from(Buffer.concat(parts));
const utf8 = (text: string): Uint8Array => Uint8Array.from(Buffer.from(text, "utf8"));
const EMPTY = new Uint8Array(0);

/** The 12-byte ChaChaPoly nonce of counter `n`. */
function nonce(n: number): Uint8Array {
  const out = new Uint8Array(12);
  new DataView(out.buffer).setUint32(4, n, true);
  return out;
}

/** The hub's end of a channel after its handshake: raw transport messages, so a test can send what
 * the real hub never would. */
function hubEnd(send: Uint8Array, receive: Uint8Array) {
  let sent = 0;
  let read = 0;
  return {
    seal: (plain: Uint8Array): Uint8Array => chacha20poly1305(send, nonce(sent++)).encrypt(plain),
    open: (message: Uint8Array): Uint8Array => chacha20poly1305(receive, nonce(read++)).decrypt(message),
    /** `frame` in chunks of at most 65,000 bytes, the last one marked, as the hub sends it. */
    frame(frame: Uint8Array): Uint8Array[] {
      const out: Uint8Array[] = [];
      for (let at = 0; at < frame.length; at += 65_000) {
        const last = at + 65_000 >= frame.length;
        out.push(this.seal(concat([Uint8Array.of(last ? 1 : 0), frame.subarray(at, at + 65_000)])));
      }
      return out;
    },
  };
}

/** The hub's side of `Noise_IK_25519_ChaChaPoly_SHA256` with the prologue `octet-link-v1`, written
 * here from the Noise specification over noble's primitives: the responder the phone's handshake
 * must work with (the contract test runs it against the hub's own, snow). */
function respond(hub: KeyPair, message1: Uint8Array) {
  let h = utf8("Noise_IK_25519_ChaChaPoly_SHA256");
  let ck = h.slice();
  let key: Uint8Array | null = null;
  let n = 0;
  const mixHash = (data: Uint8Array) => {
    h = sha256(concat([h, data]));
  };
  const mixKey = (input: Uint8Array) => {
    const out = hkdf(sha256, input, ck, EMPTY, 64);
    ck = out.slice(0, 32);
    key = out.slice(32);
    n = 0;
  };
  const encryptAndHash = (plain: Uint8Array): Uint8Array => {
    const sealed = chacha20poly1305(key ?? EMPTY, nonce(n++), h).encrypt(plain);
    mixHash(sealed);
    return sealed;
  };
  const decryptAndHash = (sealed: Uint8Array): Uint8Array => {
    const plain = chacha20poly1305(key ?? EMPTY, nonce(n++), h).decrypt(sealed);
    mixHash(sealed);
    return plain;
  };
  mixHash(utf8("octet-link-v1"));
  mixHash(hub.publicKey);
  expect(message1).toHaveLength(96);
  const phoneEphemeral = message1.subarray(0, 32);
  mixHash(phoneEphemeral);
  mixKey(x25519.getSharedSecret(hub.privateKey, phoneEphemeral));
  const phoneStatic = decryptAndHash(message1.subarray(32, 80));
  mixKey(x25519.getSharedSecret(hub.privateKey, phoneStatic));
  expect(decryptAndHash(message1.subarray(80))).toHaveLength(0);
  const ephemeral = newKeyPair(random);
  mixHash(ephemeral.publicKey);
  mixKey(x25519.getSharedSecret(ephemeral.privateKey, phoneEphemeral));
  mixKey(x25519.getSharedSecret(ephemeral.privateKey, phoneStatic));
  const message2 = concat([ephemeral.publicKey, encryptAndHash(EMPTY)]);
  const split = hkdf(sha256, EMPTY, ck, EMPTY, 64);
  return { message2, phoneStatic, end: hubEnd(split.slice(32), split.slice(0, 32)) };
}

/** A relay socket the test drives: what the phone sent, and the relay's side of the socket. */
class FakeRaw implements RawSocket {
  sent: Uint8Array[] = [];
  closed = false;
  onopen?: () => void;
  onmessage?: (bytes: Uint8Array) => void;
  onclose?: (code: number) => void;
  constructor(readonly url: string) {}
  send(bytes: Uint8Array): void {
    this.sent.push(bytes.slice());
  }
  close(): void {
    this.closed = true;
  }
}

const URL_ = "wss://relay.example/v1/phone/" + "ab".repeat(32);

/** A secure socket over a fake relay socket, and what it handed on. */
function rig(hub = newKeyPair(random)) {
  const device = newKeyPair(random);
  const raws: FakeRaw[] = [];
  const factory = secureSocketFactory({ hubKey: hub.publicKey, device }, (url) => {
    const raw = new FakeRaw(url);
    raws.push(raw);
    return raw;
  });
  const socket: Socket = factory(URL_);
  const seen: unknown[] = [];
  socket.onopen = () => seen.push("open");
  socket.onmessage = (text) => seen.push({ frame: text });
  socket.onclose = (code) => seen.push({ close: code });
  socket.onpong = () => seen.push("pong");
  const raw = (): FakeRaw => {
    const last = raws[raws.length - 1];
    if (!last) throw new Error("no relay socket was opened");
    return last;
  };
  /** The relay socket opens and the hub answers the phone's first message. */
  const handshake = () => {
    raw().onopen?.();
    const answered = respond(hub, raw().sent[0] ?? EMPTY);
    raw().onmessage?.(answered.message2);
    return answered;
  };
  return { hub, device, raws, raw, socket, seen, handshake };
}

describe("secureSocketFactory", () => {
  it("the_socket_opens_only_after_the_handshake", () => {
    const r = rig();
    expect(r.raw().url).toBe(URL_);
    expect(r.seen).toEqual([]);
    r.raw().onopen?.();
    // The relay is reached: the phone sends its first message, 96 bytes, and is not open yet.
    expect(r.raw().sent.map((m) => m.length)).toEqual([96]);
    expect(r.seen).toEqual([]);
    expect(() => r.socket.send("{}")).toThrow();
    const { phoneStatic, message2 } = respond(r.hub, r.raw().sent[0] ?? EMPTY);
    expect(phoneStatic).toEqual(r.device.publicKey);
    r.raw().onmessage?.(message2);
    expect(r.seen).toEqual(["open"]);
  });

  it("a_bad_second_message_closes", () => {
    for (const bad of [
      (m: Uint8Array) => m.subarray(0, 47),
      (m: Uint8Array) => concat([m, Uint8Array.of(0)]),
      (m: Uint8Array) => {
        const out = m.slice();
        out[40] = (out[40] ?? 0) ^ 1;
        return out;
      },
      () => new Uint8Array(48),
    ]) {
      const r = rig();
      r.raw().onopen?.();
      const { message2 } = respond(r.hub, r.raw().sent[0] ?? EMPTY);
      r.raw().onmessage?.(bad(message2));
      expect(r.seen).toEqual([{ close: 1002 }]);
      expect(r.raw().closed).toBe(true);
      // The relay's own close that follows is not said again.
      r.raw().onclose?.(1000);
      expect(r.seen).toEqual([{ close: 1002 }]);
    }
  });

  it("a_refused_hub_key_never_dials_the_relay", async () => {
    // A point of small order makes an all-zero shared secret: the handshake refuses it, and its
    // first message is empty, so there is nothing to send and no reason to connect.
    const smallOrder: KeyPair = { publicKey: new Uint8Array(32), privateKey: new Uint8Array(32) };
    const r = rig(smallOrder);
    expect(r.raws).toEqual([]);
    expect(r.seen).toEqual([]);
    await Promise.resolve();
    expect(r.seen).toEqual([{ close: 1002 }]);
    expect(r.raws).toEqual([]);
    expect(() => r.socket.send("{}")).toThrow();
    expect(r.socket.ping()).toBe(false);
    r.socket.close();
  });

  it("frames_cross_whole_in_both_directions", () => {
    const r = rig();
    const { end } = r.handshake();
    // Phone to hub: a frame past one chunk goes as chunks, each opened in order to the whole.
    const big = JSON.stringify({ v: 1, type: "prompt", text: "é".repeat(70_000) });
    r.socket.send(big);
    const messages = r.raw().sent.slice(1);
    expect(messages).toHaveLength(3);
    const plains = messages.map((m) => end.open(m));
    expect(plains.map((p) => p[0])).toEqual([0, 0, 1]);
    expect(Buffer.concat(plains.map((p) => p.subarray(1))).toString("utf8")).toBe(big);
    // Hub to phone: chunks are joined, and only the whole frame is handed on.
    const reply = JSON.stringify({ v: 1, type: "ack", req: 1, message: "ü".repeat(40_000) });
    const chunks = end.frame(utf8(reply));
    expect(chunks).toHaveLength(2);
    r.raw().onmessage?.(chunks[0] ?? EMPTY);
    expect(r.seen).toEqual(["open"]);
    r.raw().onmessage?.(chunks[1] ?? EMPTY);
    expect(r.seen).toEqual(["open", { frame: reply }]);
  });

  it("a_ping_from_the_hub_is_answered_and_not_handed_on", () => {
    const r = rig();
    const { end } = r.handshake();
    const bytes = Uint8Array.of(1, 2, 3, 4, 5, 6, 7, 8);
    r.raw().onmessage?.(end.seal(concat([Uint8Array.of(2), bytes])));
    expect(r.seen).toEqual(["open"]);
    const [pong] = r.raw().sent.slice(1);
    expect(end.open(pong ?? EMPTY)).toEqual(concat([Uint8Array.of(3), bytes]));
  });

  it("pings the hub through the channel and hears its pong", () => {
    const r = rig();
    const { end } = r.handshake();
    expect(r.socket.ping()).toBe(true);
    const ping = end.open(r.raw().sent[1] ?? EMPTY);
    expect(ping[0]).toBe(2);
    expect(ping).toHaveLength(9);
    // A second ping before the first is answered: the hub is gone, and the caller is told so.
    expect(r.socket.ping()).toBe(false);
    r.raw().onmessage?.(end.seal(concat([Uint8Array.of(3), ping.subarray(1)])));
    expect(r.seen).toEqual(["open", "pong"]);
    expect(r.socket.ping()).toBe(true);
  });

  it("a_frame_past_256_KiB_is_dropped_and_the_next_arrives", () => {
    const r = rig();
    const { end } = r.handshake();
    const huge = utf8(`"${"x".repeat(MAX_FRAME_BYTES)}"`);
    for (const chunk of end.frame(huge)) r.raw().onmessage?.(chunk);
    expect(r.seen).toEqual(["open"]);
    const next = JSON.stringify({ v: 1, type: "ack", req: 2, message: "after" });
    for (const chunk of end.frame(utf8(next))) r.raw().onmessage?.(chunk);
    expect(r.seen).toEqual(["open", { frame: next }]);
    expect(r.raw().closed).toBe(false);
  });

  it("a message that does not open closes, and nothing after it is handed on", () => {
    const r = rig();
    const { end } = r.handshake();
    const [good] = end.frame(utf8("{}"));
    const forged = (good ?? EMPTY).slice();
    forged[3] = (forged[3] ?? 0) ^ 1;
    r.raw().onmessage?.(forged);
    expect(r.seen).toEqual(["open", { close: 1002 }]);
    expect(r.raw().closed).toBe(true);
    r.raw().onmessage?.(good ?? EMPTY);
    expect(r.seen).toEqual(["open", { close: 1002 }]);
  });

  it("passes the relay's close code on, before or after the handshake", () => {
    const early = rig();
    early.raw().onopen?.();
    early.raw().onclose?.(4404);
    expect(early.seen).toEqual([{ close: 4404 }]);
    const late = rig();
    late.handshake();
    late.raw().onclose?.(4409);
    expect(late.seen).toEqual(["open", { close: 4409 }]);
  });

  it("closes the relay socket when the client closes, and says nothing more", () => {
    const r = rig();
    r.handshake();
    r.socket.close();
    expect(r.raw().closed).toBe(true);
    r.raw().onclose?.(1000);
    expect(r.seen).toEqual(["open"]);
  });
});
