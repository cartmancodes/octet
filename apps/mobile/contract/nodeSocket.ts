// A Node WebSocket for the contract test, in the shape the secure socket runs over: binary
// messages only, as the relay's phone sockets carry.
import WebSocket from "ws";

import type { RawSocket, RawSocketFactory } from "../src/client/secureSocket";

/** A factory of relay sockets, each pushed on `opened` so the test can cut the network under the
 * client, and its close code on `codes`. */
export function nodeSocketFactory(opened: WebSocket[] = [], codes: number[] = []): RawSocketFactory {
  return (url) => {
    const ws = new WebSocket(url);
    opened.push(ws);
    const socket: RawSocket = {
      send: (bytes) => ws.send(bytes),
      close: () => ws.close(),
    };
    let closed = false;
    const close = (code: number) => {
      if (closed) return;
      closed = true;
      codes.push(code);
      socket.onclose?.(code);
    };
    ws.on("open", () => socket.onopen?.());
    ws.on("message", (data, isBinary) => {
      if (isBinary && Buffer.isBuffer(data)) socket.onmessage?.(new Uint8Array(data));
    });
    ws.on("close", (code) => close(code));
    ws.on("error", () => close(1006));
    return socket;
  };
}
