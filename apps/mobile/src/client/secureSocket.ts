// The phone's socket to a hub through the relay: a WebSocket to `<relay>v1/phone/<channel>` that
// carries the Noise handshake and then the encrypted channel (noise.ts), and that the hub client
// sees as a socket of whole JSON frames. The relay only splices bytes; it reads none of this.
import { getRandomBytes } from "expo-crypto";

import type { Socket, SocketFactory } from "./HubClient";
import { initiate, type Channel, type KeyPair } from "./noise";

/** What the secure socket needs of a WebSocket that carries binary messages; React Native's is
 * wrapped in socket.ts, the contract test wraps Node's, and tests use a fake. */
export interface RawSocket {
  send(bytes: Uint8Array): void;
  close(): void;
  onopen?: () => void;
  onmessage?: (bytes: Uint8Array) => void;
  onclose?: (code: number) => void;
}
export type RawSocketFactory = (url: string) => RawSocket;

/** The keys of one pairing: the hub's static public key (pinned from the pairing link) and this
 * phone's own key pair for it. */
export interface LinkKeys {
  hubKey: Uint8Array;
  device: KeyPair;
}

/** The close code the secure socket reports when the channel itself failed: a hub key that makes
 * no handshake, a second message that does not finish it, or a message that does not open. */
export const BROKEN = 1002;

/**
 * Sockets to the hub whose key is `keys.hubKey`, as `keys.device`, over `raw`'s sockets. Each one
 * opens (`onopen`) only once the handshake is done: the phone's first message goes out when the
 * relay socket opens, and the hub's answer must finish it. After that, `send` seals a frame as
 * its chunks, `onmessage` is handed only whole frames, a ping from the hub is answered here and
 * never handed on, and `ping` sends the channel's own ping, whose answer is `onpong`. A frame
 * past 256 KiB is dropped by the channel, and the next one arrives. Anything that breaks the
 * channel closes the relay socket and is one `onclose` (`BROKEN`); the relay's own close passes
 * its code on (4404: no hub there). A hub key that makes no handshake never dials the relay.
 * `random` makes each connection's ephemeral key and the ping bytes.
 */
export function secureSocketFactory(
  keys: LinkKeys,
  raw: RawSocketFactory,
  random: (n: number) => Uint8Array = getRandomBytes,
): SocketFactory {
  return (url) => {
    const handshake = initiate(keys.device, keys.hubKey, random);
    let channel: Channel | null = null;
    /** Set once `onclose` was said, or the client closed: nothing more is said. */
    let over = false;
    let ws: RawSocket | null = null;

    const say = (code: number): void => {
      if (over) return;
      over = true;
      if (ws) ws.onopen = ws.onmessage = ws.onclose = undefined;
      socket.onclose?.(code);
    };
    const fail = (): void => {
      try {
        ws?.close();
      } catch {
        // Already closed.
      }
      say(BROKEN);
    };
    const live = (): Channel => {
      if (over || channel === null || ws === null) throw new Error("the channel is not open");
      return channel;
    };

    const socket: Socket = {
      send: (text) => {
        const open = live();
        for (const message of open.seal(text)) ws?.send(message);
      },
      close: () => {
        over = true;
        if (!ws) return;
        ws.onopen = ws.onmessage = ws.onclose = undefined;
        ws.close();
      },
      ping: () => {
        if (over || channel === null || ws === null) return false;
        const message = channel.ping(random);
        if (message === null) return false;
        try {
          ws.send(message);
          return true;
        } catch {
          return false;
        }
      },
    };

    if (handshake.message1.length === 0) {
      // A hub key of small order (a forged link): there is nothing to send, and no reason to dial.
      void Promise.resolve().then(() => say(BROKEN));
      return socket;
    }

    const opened = raw(url);
    ws = opened;
    opened.onopen = () => opened.send(handshake.message1);
    opened.onmessage = (bytes) => {
      if (channel === null) {
        channel = handshake.finish(bytes);
        if (channel === null) fail();
        else socket.onopen?.();
        return;
      }
      const got = channel.open(bytes);
      if (got === null) fail();
      else if (got === "pong") socket.onpong?.();
      else if (got !== "more" && "reply" in got) opened.send(got.reply);
      else if (got !== "more") socket.onmessage?.(got.frame);
    };
    opened.onclose = (code) => say(code);
    return socket;
  };
}
