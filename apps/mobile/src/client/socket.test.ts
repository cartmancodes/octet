import { FakeClock } from "./fakes";
import { HubClient, type HubState } from "./HubClient";
import { webSocketFactory } from "./socket";

class FakeWebSocket {
  static last: FakeWebSocket;
  static all: FakeWebSocket[] = [];
  sent: string[] = [];
  closed = false;
  onopen?: () => void;
  onmessage?: (event: { data?: unknown }) => void;
  onclose?: (event: { code?: number }) => void;
  onerror?: () => void;
  constructor(readonly url: string) {
    FakeWebSocket.last = this;
    FakeWebSocket.all.push(this);
  }

  /** What React Native's WebSocket does when the upgrade gets no 101 (the hub's 503 "Too many
   * connections"): an error, then a close 1006 with the status as its reason. */
  refuseUpgrade(): void {
    this.onerror?.();
    this.onclose?.({ code: 1006 });
  }
  send(text: string) {
    this.sent.push(text);
  }
  close() {
    this.closed = true;
  }
}

const realWebSocket = globalThis.WebSocket;
beforeEach(() => {
  FakeWebSocket.all = [];
  globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
});
afterEach(() => {
  globalThis.WebSocket = realWebSocket;
});

describe("webSocketFactory", () => {
  it("opens the url and passes on opens, text frames and closes", () => {
    const socket = webSocketFactory("wss://mac.ts.net/");
    const seen: unknown[] = [];
    socket.onopen = () => seen.push("open");
    socket.onmessage = (text) => seen.push(text);
    socket.onclose = (code) => seen.push(code);
    const ws = FakeWebSocket.last;
    expect(ws.url).toBe("wss://mac.ts.net/");
    ws.onopen?.();
    ws.onmessage?.({ data: "{}" });
    ws.onclose?.({ code: 1001 });
    expect(seen).toEqual(["open", "{}", 1001]);
  });

  it("ignores binary frames", () => {
    const socket = webSocketFactory("wss://mac.ts.net/");
    const seen: unknown[] = [];
    socket.onmessage = (text) => seen.push(text);
    FakeWebSocket.last.onmessage?.({ data: new ArrayBuffer(4) });
    expect(seen).toEqual([]);
  });

  it("treats an error as a close, and sends and closes through", () => {
    const socket = webSocketFactory("wss://mac.ts.net/");
    const codes: number[] = [];
    socket.onclose = (code) => codes.push(code);
    FakeWebSocket.last.onerror?.();
    expect(codes).toEqual([1006]);
    socket.send("hi");
    socket.close();
    expect(FakeWebSocket.last.sent).toEqual(["hi"]);
    expect(FakeWebSocket.last.closed).toBe(true);
  });
});

describe("a hub that turns the upgrade away (503 Too many connections)", () => {
  it("is one failed connect: HubClient retries with its normal backoff, and gets in once there is room", () => {
    const clock = new FakeClock();
    const states: HubState[] = [];
    const client = new HubClient({
      url: "wss://mac.ts.net/",
      token: "device-token",
      factory: webSocketFactory,
      now: clock.now,
      schedule: clock.schedule,
      random: () => 1,
    });
    client.onState((s) => states.push(s));
    client.start();

    // 32 young sockets are waiting for their hello: every upgrade is refused, each one only once
    // (the error and the close are one lost connection), and the waits climb as for any failure.
    const waits: number[] = [];
    for (let i = 0; i < 4; i++) {
      FakeWebSocket.last.refuseUpgrade();
      expect(clock.delays).toHaveLength(1);
      expect(states.at(-1)).toBe("offline");
      const [wait] = clock.delays;
      waits.push(wait ?? -1);
      clock.advance(wait ?? 0);
    }
    expect(waits).toEqual([500, 1000, 2000, 4000]);
    expect(FakeWebSocket.all).toHaveLength(5);
    expect(states).not.toContain("unpaired");

    // The room cleared: the next upgrade opens, says hello and is welcomed.
    const ws = FakeWebSocket.last;
    ws.onopen?.();
    expect(JSON.parse(ws.sent[0] ?? "{}")).toMatchObject({ type: "hello", token: "device-token" });
    ws.onmessage?.({ data: JSON.stringify({ v: 1, type: "welcome", device: "iPhone" }) });
    expect(client.state).toBe("ready");
    expect(clock.delays.filter((d) => d <= 4000)).toEqual([]); // no reconnect waits
    client.stop();
  });
});
