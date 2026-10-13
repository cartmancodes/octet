// A Node WebSocket for the contract test, in the shape HubClient's socket factory wants. It adds
// the `Tailscale-User-Login` header that `tailscale serve` adds in real life; the app never sends it.
import WebSocket from "ws";

import type { Socket, SocketFactory } from "../src/client/HubClient";

/** A factory whose sockets send `login` as the tailnet user, and are pushed on `opened` so the test
 * can cut the network under the client. */
export function nodeSocketFactory(login: string, opened: WebSocket[] = []): SocketFactory {
  return (url) => {
    const ws = new WebSocket(url, { headers: { "Tailscale-User-Login": login } });
    opened.push(ws);
    const socket: Socket = {
      send: (text) => ws.send(text),
      close: () => ws.close(),
    };
    ws.on("open", () => socket.onopen?.());
    ws.on("message", (data, isBinary) => {
      if (!isBinary) socket.onmessage?.(data.toString());
    });
    ws.on("close", (code) => socket.onclose?.(code));
    ws.on("error", () => socket.onclose?.(1006));
    return socket;
  };
}
