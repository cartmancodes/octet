/**
 * How fast this phone runs the channel's encryption, measured in the app itself (Hermes, the
 * optimised build), so the owner can check the JavaScript Noise is quick enough on real phones:
 * a handshake within 1,000 ms and at least 1 MiB a second. Settings shows it in every build.
 */
import { hexToBytes } from "@noble/hashes/utils.js";
import { getRandomBytes } from "expo-crypto";

import { channelOver, initiate, newKeyPair } from "../client/noise";

/** Handshakes timed, for their mean. */
const HANDSHAKES = 100;
/** The bytes sealed, and then opened. */
const TOTAL_BYTES = 8 * 1024 * 1024;
/** The size of each frame: just over a chunk, so each is two messages, as large frames are. */
const FRAME_BYTES = 64 * 1024;

// The handshake from the hub's test vectors (crates/octet-hub/tests/data/noise-vectors.json):
// published test keys, so every run does the same work and nothing needs a hub to answer it.
// A change to the vectors that these miss fails the measurement, and its test.
const PHONE_STATIC_PRIVATE = "808b96a1acb7c2cdd8e3eef9040f1a25303b46515c67727d88939ea9b4bfcad5";
const PHONE_EPHEMERAL_PRIVATE = "c4d1deebf805121f2c394653606d7a8794a1aebbc8d5e2effc091623303d4a57";
const HUB_STATIC_PUBLIC = "c8feca81be196cdf2cadeabf13c4903d7632dce4955aa68b6e5d9adef54e2616";
const MESSAGE2 =
  "24edb42790a02a870807c948e8a945307e5fba419fc9f029e74cb8cdc5752a7bfe1677e3b3788621385ce257b7122299";

/**
 * Times the phone's side of 100 handshakes (making the first message and finishing with the
 * hub's answer), then sealing 8 MiB in 64 KiB frames and opening them again. `handshakeMs` is the
 * mean per handshake; `megabytesPerSecond` is MiB (2^20 bytes) per second through both sealing
 * and opening, as 8 MiB over the time for both. Synchronous: it holds the JavaScript thread for
 * as long as it takes. Throws if a handshake or a frame fails, which would be a bug.
 */
export function runCryptoBench(): { handshakeMs: number; megabytesPerSecond: number } {
  const phone = newKeyPair(() => hexToBytes(PHONE_STATIC_PRIVATE));
  const hubKey = hexToBytes(HUB_STATIC_PUBLIC);
  const ephemeral = hexToBytes(PHONE_EPHEMERAL_PRIVATE);
  const message2 = hexToBytes(MESSAGE2);

  let started = performance.now();
  for (let i = 0; i < HANDSHAKES; i++) {
    // A copy each time: finish wipes the ephemeral key it was given.
    if (initiate(phone, hubKey, () => ephemeral.slice()).finish(message2) === null) {
      throw new Error("the measured handshake failed");
    }
  }
  const handshakeMs = (performance.now() - started) / HANDSHAKES;

  // Two ends of one channel, so what one seals the other opens.
  const [toHub, toPhone] = [getRandomBytes(32), getRandomBytes(32)];
  const sender = channelOver(toHub, toPhone);
  const receiver = channelOver(toPhone, toHub);
  const frame = "x".repeat(FRAME_BYTES);
  started = performance.now();
  for (let sent = 0; sent < TOTAL_BYTES; sent += FRAME_BYTES) {
    let opened: ReturnType<typeof receiver.open> = null;
    for (const message of sender.seal(frame)) opened = receiver.open(message);
    if (opened === null || typeof opened !== "object" || !("frame" in opened) || opened.frame.length !== FRAME_BYTES) {
      throw new Error("a measured frame did not open");
    }
  }
  const seconds = (performance.now() - started) / 1000;
  return { handshakeMs, megabytesPerSecond: TOTAL_BYTES / 2 ** 20 / seconds };
}
