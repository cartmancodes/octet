import type { Socket, SocketFactory } from "./HubClient";

/** The hub client's socket over React Native's WebSocket. Only text frames are passed on (the
 * hub sends no binary), and an error is a close: the client reconnects on either. */
export const webSocketFactory: SocketFactory = (url) => {
  const ws = new WebSocket(url);
  const socket: Socket = {
    send: (text) => ws.send(text),
    close: () => ws.close(),
  };
  ws.onopen = () => socket.onopen?.();
  ws.onmessage = (event: { data?: unknown }) => {
    if (typeof event.data === "string") socket.onmessage?.(event.data);
  };
  ws.onclose = (event: { code?: number }) => socket.onclose?.(event.code ?? 1006);
  ws.onerror = () => socket.onclose?.(1006);
  return socket;
};
