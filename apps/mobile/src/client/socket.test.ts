import { FakeClock, identityOf } from "./fakes";
import { HubClient, type HubState } from "./HubClient";
import { newKeyPair } from "./noise";
import { secureSocketFactory } from "./secureSocket";
import { webSocketFactory } from "./socket";

class FakeWebSocket {
  static last: FakeWebSocket;
  static all: FakeWebSocket[] = [];
  sent: unknown[] = [];
  closed = false;
  binaryType = "blob";
  onopen?: () => void;
  onmessage?: (event: { data?: unknown }) => void;
  onclose?: (event: { code?: number }) => void;
  onerror?: () => void;
  constructor(readonly url: string) {
    FakeWebSocket.last = this;
    FakeWebSocket.all.push(this);
  }

  /** What React Native's WebSocket does when the upgrade gets no 101 (the relay's 503 or 429): an
   * error, then a close 1006 with the status as its reason. */
  refuseUpgrade(): void {
    this.onerror?.();
    this.onclose?.({ code: 1006 });
  }
  send(data: unknown) {
    this.sent.push(data);
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

const URL_ = "wss://relay.example/v1/phone/" + "ab".repeat(32);

describe("webSocketFactory", () => {
  it("opens the url for binary, and passes on opens, binary messages as bytes, and closes", () => {
    const socket = webSocketFactory(URL_);
    const seen: unknown[] = [];
    socket.onopen = () => seen.push("open");
    socket.onmessage = (bytes) => seen.push(Array.from(bytes));
    socket.onclose = (code) => seen.push(code);
    const ws = FakeWebSocket.last;
    expect(ws.url).toBe(URL_);
    expect(ws.binaryType).toBe("arraybuffer");
    ws.onopen?.();
    ws.onmessage?.({ data: Uint8Array.of(1, 2, 3).buffer });
    ws.onclose?.({ code: 4404 });
    expect(seen).toEqual(["open", [1, 2, 3], 4404]);
  });

  it("ignores text messages: data connections carry binary only", () => {
    const socket = webSocketFactory(URL_);
    const seen: unknown[] = [];
    socket.onmessage = (bytes) => seen.push(bytes);
    FakeWebSocket.last.onmessage?.({ data: "{}" });
    expect(seen).toEqual([]);
  });

  it("treats an error as a close, and sends exactly the bytes given, and closes through", () => {
    const socket = webSocketFactory(URL_);
    const codes: number[] = [];
    socket.onclose = (code) => codes.push(code);
    FakeWebSocket.last.onerror?.();
    expect(codes).toEqual([1006]);
    // A view into a larger buffer goes as its own bytes, not the buffer under it.
    const whole = Uint8Array.of(9, 8, 7, 6);
    socket.send(whole.subarray(1, 3));
    const [sent] = FakeWebSocket.last.sent;
    expect(sent).toBeInstanceOf(ArrayBuffer);
    expect(Array.from(new Uint8Array(sent as ArrayBuffer))).toEqual([8, 7]);
    socket.close();
    expect(FakeWebSocket.last.closed).toBe(true);
  });
});

describe("a relay that turns the upgrade away (503 or 429)", () => {
  it("is one failed connect: HubClient retries with its normal backoff, and the next upgrade starts a handshake", () => {
    const clock = new FakeClock();
    const states: HubState[] = [];
    const hub = newKeyPair(() => new Uint8Array(32).fill(3));
    const identity = { ...identityOf("mac"), hubKey: hub.publicKey };
    const client = new HubClient({
      ...identity,
      factory: secureSocketFactory({ hubKey: identity.hubKey, device: identity.key ?? newKeyPair(() => new Uint8Array(32)) }, webSocketFactory),
      now: clock.now,
      schedule: clock.schedule,
      random: () => 1,
    });
    client.onState((s) => states.push(s));
    client.start();

    // Every upgrade is refused, each one only once (the error and the close are one lost
    // connection), and the waits climb as for any failure.
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

    // The room cleared: the next upgrade opens, and the phone's first handshake message goes out.
    FakeWebSocket.last.onopen?.();
    const [first] = FakeWebSocket.last.sent;
    expect(new Uint8Array(first as ArrayBuffer)).toHaveLength(96);
    expect(client.state).toBe("connecting");
    client.stop();
  });
});
