import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { randomBytes } from "node:crypto";
import { readFileSync } from "node:fs";
import path from "node:path";

import { MAX_FRAME_BYTES } from "./frames";
import { channelOver, initiate, newKeyPair, showCode, type Channel } from "./noise";

/** The hub's handshake and transport messages, made by snow (crates/octet-hub/src/secure.rs). */
interface Vectors {
  pattern: string;
  prologue: string;
  hub_static_private: string;
  hub_static_public: string;
  phone_static_private: string;
  phone_static_public: string;
  phone_ephemeral_private: string;
  message1: string;
  message2: string;
  transport: { from: "phone" | "hub"; plain: string; cipher: string }[];
}

const VECTORS = path.resolve(__dirname, "..", "..", "..", "..", "crates", "octet-hub", "tests", "data", "noise-vectors.json");
const vectors = JSON.parse(readFileSync(VECTORS, "utf8")) as Vectors;

const bytes = (hex: string): Uint8Array => Uint8Array.from(Buffer.from(hex, "hex"));
const hex = (data: Uint8Array): string => Buffer.from(data).toString("hex");
const utf8 = (data: Uint8Array): string => Buffer.from(data).toString("utf8");
const random = (n: number): Uint8Array => Uint8Array.from(randomBytes(n));
/** A `random` that always gives (a copy of) `data`. */
const fixed = (data: Uint8Array) => (): Uint8Array => data.slice();

/** Changes one bit of `data[at]`. */
const flip = (data: Uint8Array, at: number): void => {
  data[at] = (data[at] ?? 0) ^ 1;
};

const concat = (parts: Uint8Array[]): Uint8Array => Uint8Array.from(Buffer.concat(parts));
/** A transport plaintext: `kind`, then `body`. */
const plain = (kind: number, body: Uint8Array | string = new Uint8Array(0)): Uint8Array =>
  concat([Uint8Array.of(kind), typeof body === "string" ? Buffer.from(body, "utf8") : body]);

/** The hub's end of a channel in these tests: raw Noise transport messages, so a test can send
 * what the real hub never would. Its own nonces, as the ChaChaPoly rule sets them. */
function hubEnd(send: Uint8Array, receive: Uint8Array) {
  let sent = 0;
  let read = 0;
  const nonce = (n: number): Uint8Array => {
    const out = new Uint8Array(12);
    new DataView(out.buffer).setUint32(4, n, true);
    return out;
  };
  return {
    seal: (message: Uint8Array): Uint8Array => chacha20poly1305(send, nonce(sent++)).encrypt(message),
    open: (message: Uint8Array): Uint8Array => chacha20poly1305(receive, nonce(read++)).decrypt(message),
    /** `frame` as the hub sends it: chunks of at most 65,000 bytes, the last one marked. */
    frame(frame: Uint8Array): Uint8Array[] {
      const out: Uint8Array[] = [];
      for (let at = 0; at < frame.length; at += 65_000) {
        const last = at + 65_000 >= frame.length;
        out.push(this.seal(plain(last ? 1 : 0, frame.subarray(at, at + 65_000))));
      }
      return out;
    },
  };
}

/** A phone's channel and the hub's end of it, over fresh keys. */
function connected(): { phone: Channel; hub: ReturnType<typeof hubEnd> } {
  const [toHub, toPhone] = [random(32), random(32)];
  return { phone: channelOver(toHub, toPhone), hub: hubEnd(toPhone, toHub) };
}

/** The phone's handshake from the vectors, with their fixed ephemeral key. */
const vectorHandshake = () =>
  initiate(
    newKeyPair(fixed(bytes(vectors.phone_static_private))),
    bytes(vectors.hub_static_public),
    fixed(bytes(vectors.phone_ephemeral_private)),
  );

describe("noise", () => {
  it("matches_the_shared_vectors", () => {
    expect(vectors.pattern).toBe("Noise_IK_25519_ChaChaPoly_SHA256");
    expect(utf8(bytes(vectors.prologue))).toBe("octet-link-v1");
    expect(hex(newKeyPair(fixed(bytes(vectors.phone_static_private))).publicKey)).toBe(vectors.phone_static_public);
    expect(hex(newKeyPair(fixed(bytes(vectors.hub_static_private))).publicKey)).toBe(vectors.hub_static_public);

    const handshake = vectorHandshake();
    expect(hex(handshake.message1)).toBe(vectors.message1);
    const channel = handshake.finish(bytes(vectors.message2));
    if (channel === null) throw new Error("the vectors' second message makes no channel");

    // Walks the transport list in order: the hub's messages are opened, and the phone's are made
    // (a frame sealed whole, a ping sent, a pong given by opening the hub's ping) and compared.
    const transport = vectors.transport;
    const made: Uint8Array[] = [];
    let joining: Uint8Array[] = [];
    const kinds: string[] = [];
    for (let i = 0; i < transport.length; i++) {
      const entry = transport[i]!;
      const sent = bytes(entry.plain);
      const [kind, body] = [sent[0], sent.subarray(1)];
      kinds.push(`${entry.from}:${kind}`);
      if (entry.from === "hub") {
        const opened = channel.open(bytes(entry.cipher));
        if (kind === 0) {
          joining.push(body);
          expect(opened).toBe("more");
        } else if (kind === 1) {
          joining.push(body);
          expect(opened).toEqual({ frame: utf8(concat(joining)) });
          joining = [];
        } else if (kind === 2) {
          if (opened === null || typeof opened !== "object" || !("reply" in opened)) {
            throw new Error("a ping gave no reply");
          }
          made.push(opened.reply);
        } else {
          expect(opened).toBe("pong");
        }
        continue;
      }
      if (made.length === 0 && kind === 2) {
        const ping = channel.ping(fixed(body));
        if (ping === null) throw new Error("the phone could not ping");
        made.push(ping);
      } else if (made.length === 0) {
        // A frame: this chunk and the phone's next ones up to its last, sealed as one.
        const chunks = [body];
        for (let j = i; transport[j]!.plain.startsWith("00"); j++) {
          chunks.push(bytes(transport[j + 1]!.plain).subarray(1));
        }
        made.push(...channel.seal(utf8(concat(chunks))));
      }
      expect(hex(made.shift()!)).toBe(entry.cipher);
    }
    expect(made).toEqual([]);
    expect(kinds.join(" ")).toBe("phone:1 hub:1 phone:0 phone:1 hub:0 hub:1 hub:2 phone:3 phone:2 hub:3");
  });

  it("refuses_a_second_message_of_the_wrong_size", () => {
    const message2 = bytes(vectors.message2);
    expect(message2.length).toBe(48);
    expect(vectorHandshake().finish(message2.subarray(0, 47))).toBeNull();
    expect(vectorHandshake().finish(concat([message2, Uint8Array.of(0)]))).toBeNull();
    expect(vectorHandshake().finish(new Uint8Array(0))).toBeNull();
    expect(vectorHandshake().finish(message2)).not.toBeNull();
  });

  it("refuses a second message that does not decrypt, and a second try after any finish", () => {
    const tampered = bytes(vectors.message2);
    flip(tampered, 40);
    expect(vectorHandshake().finish(tampered)).toBeNull();
    const handshake = vectorHandshake();
    expect(handshake.finish(new Uint8Array(47))).toBeNull();
    expect(handshake.finish(bytes(vectors.message2))).toBeNull();
  });

  it("a_96_KiB_frame_is_two_messages_and_opens_whole", () => {
    const { phone, hub } = connected();
    // 32,768 three-byte characters: the first chunk ends inside one.
    const frame = "€".repeat(32_768);
    const messages = phone.seal(frame);
    expect(messages).toHaveLength(2);
    const plains = messages.map((m) => hub.open(m));
    expect(plains.map((p) => [p[0], p.length - 1])).toEqual([
      [0, 65_000],
      [1, 96 * 1024 - 65_000],
    ]);
    expect(utf8(concat(plains.map((p) => p.subarray(1))))).toBe(frame);
  });

  it("a_200_KiB_frame_from_the_hub_opens_whole", () => {
    const { phone, hub } = connected();
    const frame = `{"t":"${"x".repeat(200 * 1024 - 8)}"}`;
    const messages = hub.frame(Buffer.from(frame));
    expect(messages).toHaveLength(4);
    expect(messages.map((m) => phone.open(m))).toEqual(["more", "more", "more", { frame }]);
  });

  it("opens every kind of character as it was sent, wherever a chunk ends", () => {
    for (const before of [64_996, 64_997, 64_998, 64_999]) {
      const { phone, hub } = connected();
      const frame = `${"a".repeat(before)}😀é€\u{10FFFF}\u{FFFF}z`;
      expect(hub.frame(Buffer.from(frame)).map((m) => phone.open(m))).toEqual(["more", { frame }]);
      const sealed = phone.seal(frame).map((m) => hub.open(m).subarray(1));
      expect(utf8(concat(sealed))).toBe(frame);
    }
  });

  it("a_tampered_message_opens_as_null", () => {
    const { phone, hub } = connected();
    const message = hub.seal(plain(1, "{}"));
    flip(message, 3);
    expect(phone.open(message)).toBeNull();
    // Once failed, it stays failed: nothing more opens or is sealed.
    expect(phone.open(hub.seal(plain(1, "{}")))).toBeNull();
    expect(phone.seal("{}")).toEqual([]);
    expect(phone.ping(random)).toBeNull();
  });

  it("a_message_opened_twice_is_null", () => {
    const { phone, hub } = connected();
    const message = hub.seal(plain(1, "{}"));
    expect(phone.open(message)).toEqual({ frame: "{}" });
    expect(phone.open(message)).toBeNull();
  });

  it("messages out of order are null", () => {
    const { phone, hub } = connected();
    const first = hub.seal(plain(1, "{}"));
    const second = hub.seal(plain(1, "[]"));
    expect(phone.open(second)).toBeNull();
    expect(phone.open(first)).toBeNull();
  });

  it("a message too short to hold a tag, or past the largest Noise message, is null", () => {
    expect(connected().phone.open(new Uint8Array(15))).toBeNull();
    expect(connected().phone.open(new Uint8Array(0))).toBeNull();
    expect(connected().phone.open(new Uint8Array(65_536))).toBeNull();
  });

  it("an_unknown_type_byte_is_null", () => {
    for (const kind of [4, 0x80, 0xff]) {
      const { phone, hub } = connected();
      expect(phone.open(hub.seal(plain(kind, "{}")))).toBeNull();
    }
    const { phone, hub } = connected();
    expect(phone.open(hub.seal(new Uint8Array(0)))).toBeNull();
  });

  it("an_empty_chunk_is_null", () => {
    for (const kind of [0, 1]) {
      const { phone, hub } = connected();
      expect(phone.open(hub.seal(plain(kind)))).toBeNull();
    }
  });

  it("a chunk over 65,000 bytes is null", () => {
    const { phone, hub } = connected();
    expect(phone.open(hub.seal(plain(0, new Uint8Array(65_001).fill(0x61))))).toBeNull();
  });

  it("a frame that is not UTF-8 is null", () => {
    // A lone continuation byte, an overlong "/", a UTF-16 surrogate, past U+10FFFF, cut short.
    for (const bad of [[0x80], [0xc0, 0xaf], [0xed, 0xa0, 0x80], [0xf4, 0x90, 0x80, 0x80], [0xe2, 0x82], [0xff]]) {
      const { phone, hub } = connected();
      expect(phone.open(hub.seal(plain(1, Uint8Array.from(bad))))).toBeNull();
    }
  });

  it("a_pong_nobody_asked_for_is_null", () => {
    const { phone, hub } = connected();
    expect(phone.open(hub.seal(plain(3, random(8))))).toBeNull();
  });

  it("the pong to the phone's ping opens as pong, once, and a pong with other bytes is null", () => {
    const { phone, hub } = connected();
    const sent = hub.open(phone.ping(random)!);
    expect(sent).toHaveLength(9);
    expect(sent[0]).toBe(2);
    // A second ping before the first is answered: the hub is gone.
    expect(phone.ping(random)).toBeNull();
    expect(phone.open(hub.seal(plain(3, sent.subarray(1))))).toBe("pong");
    expect(phone.open(hub.seal(plain(3, sent.subarray(1))))).toBeNull();

    const other = connected();
    const bytesSent = other.hub.open(other.phone.ping(random)!).subarray(1).slice();
    flip(bytesSent, 0);
    expect(other.phone.open(other.hub.seal(plain(3, bytesSent)))).toBeNull();
  });

  it("two pings carry different bytes", () => {
    const { phone, hub } = connected();
    const first = hub.open(phone.ping(random)!).subarray(1);
    expect(phone.open(hub.seal(plain(3, first)))).toBe("pong");
    const second = hub.open(phone.ping(random)!).subarray(1);
    expect(hex(second)).not.toBe(hex(first));
  });

  it("a_ping_opens_as_a_reply_that_carries_its_bytes", () => {
    const { phone, hub } = connected();
    const ping = random(8);
    const opened = phone.open(hub.seal(plain(2, ping)));
    if (opened === null || typeof opened !== "object" || !("reply" in opened)) throw new Error("no reply");
    expect(hex(hub.open(opened.reply))).toBe(hex(plain(3, ping)));
    // The channel goes on.
    expect(phone.open(hub.seal(plain(1, "{}")))).toEqual({ frame: "{}" });
  });

  it("a ping or pong of another length is null", () => {
    for (const [kind, length] of [
      [2, 7],
      [2, 9],
      [3, 7],
      [3, 0],
    ] as const) {
      const { phone, hub } = connected();
      const asked = kind === 3 ? hub.open(phone.ping(random)!).subarray(1) : random(8);
      const body = new Uint8Array(length);
      body.set(asked.subarray(0, Math.min(length, 8)));
      expect(phone.open(hub.seal(plain(kind, body)))).toBeNull();
    }
  });

  it("a ping between the chunks of a frame leaves the frame whole", () => {
    const { phone, hub } = connected();
    expect(phone.open(hub.seal(plain(0, "y".repeat(65_000))))).toBe("more");
    expect(phone.open(hub.seal(plain(2, random(8))))).toHaveProperty("reply");
    expect(phone.open(hub.seal(plain(1, "y".repeat(5_000))))).toEqual({ frame: "y".repeat(70_000) });
  });

  it("a_frame_past_MAX_FRAME_BYTES_is_dropped_and_the_next_frame_opens", () => {
    const { phone, hub } = connected();
    const big = hub.frame(new Uint8Array(MAX_FRAME_BYTES + 1).fill(0x7a));
    expect(big).toHaveLength(5);
    expect(big.map((m) => phone.open(m))).toEqual(["more", "more", "more", "more", "more"]);
    expect(phone.open(hub.seal(plain(1, '{"next":1}')))).toEqual({ frame: '{"next":1}' });

    // Exactly MAX_FRAME_BYTES is taken.
    const most = "w".repeat(MAX_FRAME_BYTES);
    const opened = hub.frame(Buffer.from(most)).map((m) => phone.open(m));
    expect(opened.at(-1)).toEqual({ frame: most });
  });

  it("an empty frame seals as nothing", () => {
    expect(connected().phone.seal("")).toEqual([]);
  });

  it("an_all_zero_hub_key_makes_no_channel", () => {
    const local = newKeyPair(random);
    // All zero, and a point of small order (u = 1), both give an all-zero shared secret.
    for (const hubKey of [new Uint8Array(32), Uint8Array.of(1, ...new Uint8Array(31))]) {
      const handshake = initiate(local, hubKey, random);
      expect(handshake.message1).toHaveLength(0);
      expect(handshake.finish(bytes(vectors.message2))).toBeNull();
    }
  });

  it("a second message with an all-zero ephemeral key makes no channel", () => {
    const message2 = bytes(vectors.message2);
    message2.fill(0, 0, 32);
    expect(vectorHandshake().finish(message2)).toBeNull();
  });

  it("two_handshakes_use_different_ephemeral_keys", () => {
    const local = newKeyPair(random);
    const hubKey = newKeyPair(random).publicKey;
    const first = initiate(local, hubKey, random).message1;
    const second = initiate(local, hubKey, random).message1;
    expect(first).toHaveLength(96);
    expect(second).toHaveLength(96);
    expect(hex(first.subarray(0, 32))).not.toBe(hex(second.subarray(0, 32)));
  });

  it("does not keep the ephemeral private key after finish", () => {
    for (const message2 of [bytes(vectors.message2), new Uint8Array(3)]) {
      const ephemeral = bytes(vectors.phone_ephemeral_private);
      const handshake = initiate(
        newKeyPair(fixed(bytes(vectors.phone_static_private))),
        bytes(vectors.hub_static_public),
        () => ephemeral,
      );
      handshake.finish(message2);
      expect(ephemeral.every((b) => b === 0)).toBe(true);
    }
  });

  it("shows a pair code as two groups of three", () => {
    expect(showCode("482913")).toBe("482 913");
    expect(showCode("000007")).toBe("000 007");
  });
});
