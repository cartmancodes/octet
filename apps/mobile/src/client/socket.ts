import type { RawSocket, RawSocketFactory } from "./secureSocket";

/** A relay socket over React Native's WebSocket: binary messages only (a data connection carries
 * nothing else, so a text one is ignored), each passed on as its bytes, and an error is a close:
 * the client reconnects on either. The secure socket (secureSocket.ts) runs over it. */
export const webSocketFactory: RawSocketFactory = (url) => {
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  const socket: RawSocket = {
    // Exactly the bytes given, as their own buffer: never the larger one a view may sit in.
    send: (bytes) => ws.send(bytes.slice().buffer as ArrayBuffer),
    close: () => ws.close(),
  };
  ws.onopen = () => socket.onopen?.();
  ws.onmessage = (event: { data?: unknown }) => {
    if (event.data instanceof ArrayBuffer) socket.onmessage?.(new Uint8Array(event.data));
  };
  ws.onclose = (event: { code?: number }) => socket.onclose?.(event.code ?? 1006);
  ws.onerror = () => socket.onclose?.(1006);
  return socket;
};
