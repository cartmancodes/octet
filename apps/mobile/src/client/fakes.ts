// Test doubles for the hub client: a socket the test drives by hand, a clock that only moves
// when told to, and the identities and pairing links of the Macs tests pair with. Not imported by
// the app.
import { sha256 } from "@noble/hashes/sha2.js";

import type { Pairing } from "../macs";
import type { HubClientOptions, Socket, SocketFactory } from "./HubClient";
import { toHex } from "./link";
import { newKeyPair, type KeyPair } from "./noise";

export class FakeSocket implements Socket {
  sent: string[] = [];
  closed = false;
  /** The encrypted pings the client sent, as the channel counts them. */
  pings = 0;
  /** A ping is waiting for its pong: the channel refuses another until it comes. */
  private awaiting = false;
  onmessage?: (text: string) => void;
  onopen?: () => void;
  onclose?: (code: number) => void;
  onpong?: () => void;

  constructor(readonly url: string) {}

  send(text: string): void {
    this.sent.push(text);
  }

  ping(): boolean {
    if (this.awaiting) return false;
    this.awaiting = true;
    this.pings += 1;
    return true;
  }

  /** The hub answers the last ping. */
  pong(): void {
    this.awaiting = false;
    this.onpong?.();
  }

  close(): void {
    this.closed = true;
  }

  /** The commands the client sent, parsed. */
  commands(): Record<string, unknown>[] {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }

  /** The commands of one type. */
  commandsOf(type: string): Record<string, unknown>[] {
    return this.commands().filter((c) => c.type === type);
  }

  open(): void {
    this.onopen?.();
  }

  /** The hub sends a frame (an object is stamped `v: 1`) or raw text. */
  receive(frame: object | string): void {
    const text = typeof frame === "string" ? frame : JSON.stringify({ v: 1, ...frame });
    this.onmessage?.(text);
  }

  /** The network or the hub ends the connection. */
  drop(code = 1006): void {
    this.onclose?.(code);
  }
}

export class Sockets {
  all: FakeSocket[] = [];

  readonly factory: SocketFactory = (url) => {
    const socket = new FakeSocket(url);
    this.all.push(socket);
    return socket;
  };

  get last(): FakeSocket {
    const socket = this.all[this.all.length - 1];
    if (!socket) throw new Error("no socket was opened");
    return socket;
  }
}

interface Timer {
  id: number;
  at: number;
  fn: () => void;
}

export class FakeClock {
  time = 1_000_000;
  private timers: Timer[] = [];
  private next = 1;

  readonly now = (): number => this.time;

  readonly schedule = (fn: () => void, ms: number): (() => void) => {
    const id = this.next++;
    this.timers.push({ id, at: this.time + ms, fn });
    return () => {
      this.timers = this.timers.filter((t) => t.id !== id);
    };
  };

  get pending(): number {
    return this.timers.length;
  }

  /** The delays of the timers waiting, soonest first. */
  get delays(): number[] {
    return this.timers.map((t) => t.at - this.time).sort((a, b) => a - b);
  }

  /** Moves time forward, running each timer that falls due in order. */
  advance(ms: number): void {
    const end = this.time + ms;
    for (;;) {
      const due = this.timers.filter((t) => t.at <= end).sort((a, b) => a.at - b.at || a.id - b.id)[0];
      if (!due) break;
      this.timers = this.timers.filter((t) => t.id !== due.id);
      this.time = Math.max(this.time, due.at);
      due.fn();
    }
    this.time = end;
  }
}

/** The relay of the tests that need only one. */
export const TEST_RELAY = "wss://relay.test/";

/** The channel id of the Mac on `host`: its own 64 hex digits, the same at every call. */
export const channelOf = (host: string): string => toHex(sha256(new TextEncoder().encode(host)));

const hubKeys = new Map<string, string>();
/** The hub key (an X25519 public key) of the Mac on `host`, made once. */
export function hubKeyOf(host: string): string {
  let key = hubKeys.get(host);
  if (key === undefined) {
    key = toHex(newKeyPair(() => sha256(new TextEncoder().encode(`hub ${host}`))).publicKey);
    hubKeys.set(host, key);
  }
  return key;
}

/** A device key pair made from `seed`: the same at every call. */
export const testKey = (seed = 7): KeyPair => newKeyPair(() => new Uint8Array(32).fill(seed));

/** What a `HubClient` needs to reach the Mac on `host` through `TEST_RELAY`, as device `key`. */
export const identityOf = (host: string, key: KeyPair | null = testKey()): Pick<HubClientOptions, "url" | "channel" | "hubKey" | "key"> => ({
  url: TEST_RELAY,
  channel: channelOf(host),
  hubKey: Uint8Array.from(Buffer.from(hubKeyOf(host), "hex")),
  key,
});

/**
 * The pairing link the Mac on `host` shows, as the hub makes it (crates/octet-hub/src/link.rs):
 * through the relay at `relay` (a `host` or `host:port`; `host` itself unless given), with
 * `secret`. Version 1 is `wss`; 0 is `ws`, which only a development build takes. `hubKey` (hex)
 * stands in for the Mac's own key, as a forged link would.
 */
export function pairLinkTo(
  host: string,
  secret: string,
  { relay = host, version = 1, hubKey = hubKeyOf(host) }: { relay?: string; version?: number; hubKey?: string } = {},
): string {
  const payload = Buffer.concat([
    Buffer.of(version),
    Buffer.from(hubKey, "hex"),
    Buffer.from(channelOf(host), "hex"),
    Buffer.from(secret, "hex"),
    Buffer.from(relay, "latin1"),
  ]);
  return `octet://pair/${payload.toString("base64url")}`;
}

/** The address a phone socket to the Mac on `host` opens, when its link names relay `relay`. */
export const phoneUrlOf = (host: string, relay = host): string => `wss://${relay}/v1/phone/${channelOf(host)}`;

/** The pairing of the Mac on `host` under `id`, as the app saves it: reached through the relay at
 * `relay` (`host` itself unless given), named `host`, paired as `iPhone`. */
export const pairingOf = (id: string, host: string, relay = host, extra: Partial<Pairing> = {}): Pairing => ({
  id,
  relay: `wss://${relay}/`,
  channel: channelOf(host),
  hubKey: hubKeyOf(host),
  deviceName: "iPhone",
  macName: host,
  ...extra,
});
