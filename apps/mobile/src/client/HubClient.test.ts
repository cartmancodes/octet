import type { Cursor } from "../protocol";
import { FakeClock, Sockets } from "./fakes";
import { HubClient, Refusal, type BatchItem, type HubState } from "./HubClient";

const URL = "wss://mac.tail1234.ts.net/";
const TOKEN = "device-token";

interface Rig {
  client: HubClient;
  sockets: Sockets;
  clock: FakeClock;
  states: HubState[];
  batches: BatchItem[][];
  cursors: [string, Cursor | null][];
}

function rig(opts: { token?: string | null; cursors?: Record<string, Cursor>; random?: () => number } = {}): Rig {
  const sockets = new Sockets();
  const clock = new FakeClock();
  const cursors: [string, Cursor | null][] = [];
  const client = new HubClient({
    url: URL,
    token: opts.token === undefined ? TOKEN : opts.token,
    factory: sockets.factory,
    now: clock.now,
    schedule: clock.schedule,
    random: opts.random ?? (() => 1),
    cursors: opts.cursors,
    onCursor: (session, cursor) => cursors.push([session, cursor]),
  });
  const states: HubState[] = [];
  const batches: BatchItem[][] = [];
  client.onState((s) => states.push(s));
  client.onBatch((frames) => batches.push(frames));
  return { client, sockets, clock, states, batches, cursors };
}

/** Starts the client and walks it through hello to ready. */
function ready(r: Rig): void {
  r.client.start();
  r.sockets.last.open();
  r.sockets.last.receive({ type: "welcome", device: "iPhone" });
  r.sockets.last.receive({ type: "sessions", sessions: [] });
  r.clock.advance(50); // the sessions frame goes out
}

const event = (session: string, journal: string, seq: number, data = "x") => ({
  type: "event",
  session,
  journal,
  seq,
  event: { type: "text", data },
});

const flush = (r: Rig) => r.clock.advance(50);
const delivered = (r: Rig): BatchItem[] => r.batches.flat();
const eventSeqs = (frames: BatchItem[]) =>
  frames.flatMap((f) => (f.type === "event" ? [`${f.journal}:${f.seq}`] : []));

describe("connecting", () => {
  it("hello_then_ready", () => {
    const r = rig();
    r.client.start();
    expect(r.states).toEqual(["connecting"]);
    expect(r.sockets.all.map((s) => s.url)).toEqual([URL]);
    expect(r.sockets.last.sent).toEqual([]); // nothing before the socket is open

    r.sockets.last.open();
    expect(r.states).toEqual(["connecting", "authenticating"]);
    expect(r.sockets.last.commands()).toEqual([{ v: 1, type: "hello", req: expect.any(Number), token: TOKEN }]);

    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    expect(r.states).toEqual(["connecting", "authenticating", "ready"]);
    r.sockets.last.receive({ type: "sessions", sessions: [] });
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
  });

  it("start is idempotent and does nothing without a token", () => {
    const r = rig();
    r.client.start();
    r.client.start();
    expect(r.sockets.all).toHaveLength(1);

    const bare = rig({ token: null });
    bare.client.start();
    expect(bare.sockets.all).toHaveLength(0);
  });

  it("gives up on a connection that never says welcome", () => {
    const r = rig();
    r.client.start();
    r.sockets.last.open();
    r.clock.advance(14_999);
    expect(r.sockets.all).toHaveLength(1);
    r.clock.advance(1);
    expect(r.sockets.all[0]?.closed).toBe(true);
    expect(r.states.at(-1)).toBe("offline");
    r.clock.advance(500);
    expect(r.sockets.all).toHaveLength(2);
  });
});

describe("pairing", () => {
  it("pairing_returns_the_token", async () => {
    const r = rig({ token: null });
    const pending = r.client.pair("s3cret", "My phone", "ios");
    const socket = r.sockets.last;
    expect(socket.url).toBe(URL);
    socket.open();
    expect(socket.commands()).toEqual([
      { v: 1, type: "pair", req: expect.any(Number), secret: "s3cret", name: "My phone", platform: "ios" },
    ]);
    socket.receive({ type: "paired", token: "fresh-token", device: "My phone" });
    await expect(pending).resolves.toEqual({ token: "fresh-token", device: "My phone" });
    expect(socket.closed).toBe(true); // the hub keeps a paired socket unauthenticated

    // The client now holds the token and says hello on a new connection.
    r.client.start();
    expect(r.sockets.all).toHaveLength(2);
    r.sockets.last.open();
    expect(r.sockets.last.commands()[0]).toMatchObject({ type: "hello", token: "fresh-token" });
  });

  it("a refused pairing rejects with the hub's code and leaves the main state alone", async () => {
    const r = rig({ token: null });
    const pending = r.client.pair("bad", "p", "ios");
    r.sockets.last.open();
    const req = r.sockets.last.commands()[0]?.req;
    r.sockets.last.receive({ type: "refused", req, code: "unpaired", reason: "That code is not valid" });
    await expect(pending).rejects.toMatchObject({ name: "Refusal", code: "unpaired", message: "That code is not valid" });
    expect(r.states).toEqual([]);
  });

  it("a pairing that is cut off or silent rejects", async () => {
    const r = rig({ token: null });
    const cut = r.client.pair("s", "p", "ios");
    r.sockets.last.open();
    r.sockets.last.drop();
    await expect(cut).rejects.toMatchObject({ code: "lost" });

    const silent = r.client.pair("s", "p", "ios");
    r.clock.advance(30_000);
    await expect(silent).rejects.toMatchObject({ code: "lost" });
    expect(r.sockets.last.closed).toBe(true);
  });
});

describe("requests", () => {
  it("requests_resolve_on_ack_and_reject_on_refused_with_code", async () => {
    const r = rig();
    ready(r);
    const socket = r.sockets.last;

    const first = r.client.request({ type: "prompt", session: "s1", text: "hi" });
    const second = r.client.request({ type: "cancel", session: "s1" });
    const [one, two] = socket.commandsOf("prompt").concat(socket.commandsOf("cancel"));
    expect(one).toMatchObject({ v: 1, type: "prompt", session: "s1", text: "hi" });
    expect(one?.req).not.toBe(two?.req);

    // Answers may come in any order.
    socket.receive({ type: "refused", req: two?.req, code: "turn_open", reason: "A turn is running" });
    socket.receive({ type: "ack", req: one?.req, message: "queued" });
    await expect(first).resolves.toMatchObject({ type: "ack", message: "queued" });
    const refusal = await second.catch((e: unknown) => e);
    expect(refusal).toBeInstanceOf(Refusal);
    expect(refusal).toMatchObject({ code: "turn_open", message: "A turn is running" });

    // Answers are consumed, not forwarded as transcript traffic.
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
  });

  it("a recent frame answers its request", async () => {
    const r = rig();
    ready(r);
    const pending = r.client.request({ type: "recent" });
    const req = r.sockets.last.commandsOf("recent")[0]?.req;
    r.sockets.last.receive({ type: "recent", req, items: [] });
    await expect(pending).resolves.toMatchObject({ type: "recent", items: [] });
  });

  it("a refusal without a code is invalid", async () => {
    const r = rig();
    ready(r);
    const pending = r.client.request({ type: "end", session: "s" });
    r.sockets.last.receive({ type: "refused", req: r.sockets.last.commandsOf("end")[0]?.req, reason: "no" });
    await expect(pending).rejects.toMatchObject({ code: "invalid" });
  });

  it("requests_time_out_after_30s", async () => {
    const r = rig();
    ready(r);
    let outcome: unknown = "pending";
    r.client.request({ type: "prompt", session: "s", text: "x" }).catch((e: unknown) => (outcome = e));
    r.clock.advance(29_999);
    await Promise.resolve();
    expect(outcome).toBe("pending");
    r.clock.advance(1);
    await Promise.resolve();
    expect(outcome).toBeInstanceOf(Refusal);
    expect(outcome).toMatchObject({ code: "lost" }); // sent, outcome unknown
    expect(r.clock.pending).toBe(0);
    // A late answer, ack or refusal, is dropped, not shown as traffic.
    const req = r.sockets.last.commandsOf("prompt")[0]?.req;
    r.sockets.last.receive({ type: "ack", req, message: "late" });
    r.sockets.last.receive({ type: "refused", req, code: "busy", reason: "late" });
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
  });

  it("remembers only the last 64 timed-out requests", async () => {
    const r = rig();
    ready(r);
    for (let i = 0; i < 70; i++) r.client.request({ type: "cancel", session: "s" }).catch(() => undefined);
    r.clock.advance(30_000);
    await Promise.resolve();
    const reqs = r.sockets.last.commandsOf("cancel").map((c) => c.req);
    // The oldest was forgotten, so its late refusal is only dropped for not being req 0.
    for (const req of reqs) r.sockets.last.receive({ type: "refused", req, code: "busy", reason: "late" });
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
    expect(r.states.at(-1)).toBe("ready");
  });

  it("rejects at once when offline, and when the link drops with a request waiting", async () => {
    const r = rig();
    await expect(r.client.request({ type: "cancel", session: "s" })).rejects.toMatchObject({ code: "unsent" });
    ready(r);
    const waiting = r.client.request({ type: "cancel", session: "s" });
    r.sockets.last.drop();
    await expect(waiting).rejects.toMatchObject({ code: "lost" });
    expect(r.sockets.last.commandsOf("cancel")).toHaveLength(1);
  });

  it("a request is not sent before the hello is answered", async () => {
    const r = rig();
    r.client.start();
    r.sockets.last.open();
    await expect(r.client.request({ type: "cancel", session: "s" })).rejects.toMatchObject({ code: "unsent" });
    expect(r.sockets.last.commandsOf("cancel")).toHaveLength(0);
  });

  it("a socket that throws on send leaves the request unsent", async () => {
    const r = rig();
    ready(r);
    r.sockets.last.send = () => {
      throw new Error("closed");
    };
    await expect(r.client.request({ type: "cancel", session: "s" })).rejects.toMatchObject({ code: "unsent" });
    expect(r.clock.pending).toBe(0);
  });

  it("a pairing whose socket throws on send or close rejects unsent", async () => {
    const r = rig({ token: null });
    const pending = r.client.pair("s", "p", "ios");
    const socket = r.sockets.last;
    socket.send = () => {
      throw new Error("closed");
    };
    socket.close = () => {
      throw new Error("closed");
    };
    socket.open();
    await expect(pending).rejects.toMatchObject({ code: "unsent" });
    expect(r.clock.pending).toBe(0);
  });
});

describe("reconnecting", () => {
  it("reconnects_with_jittered_backoff_and_stops_in_background", () => {
    const r = rig({ random: () => 1 });
    ready(r);
    r.sockets.last.drop();
    expect(r.states.at(-1)).toBe("offline");
    expect(r.clock.delays).toEqual([500]);

    // Each failed attempt doubles the wait, up to 30 s.
    const waits: number[] = [];
    for (let i = 0; i < 9; i++) {
      const [wait] = r.clock.delays;
      waits.push(wait ?? -1);
      r.clock.advance(wait ?? 0);
      expect(r.states.at(-1)).toBe("connecting");
      r.sockets.last.drop(); // never opened
    }
    expect(waits).toEqual([500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);

    // A welcome starts the climb over.
    r.clock.advance(r.clock.delays[0] ?? 0);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    r.sockets.last.drop();
    expect(r.clock.delays).toEqual([500]);

    // Backgrounded: the socket closes and no timer is left.
    const socket = r.sockets.last;
    r.client.stop();
    expect(r.clock.pending).toBe(0);
    r.clock.advance(120_000);
    expect(r.sockets.last).toBe(socket);

    // Foregrounded: it connects again at once.
    const before = r.sockets.all.length;
    r.client.start();
    expect(r.sockets.all).toHaveLength(before + 1);
  });

  it("jitter spreads the waits", () => {
    const r = rig({ random: () => 0 });
    ready(r);
    r.sockets.last.drop();
    expect(r.clock.delays).toEqual([250]);
  });

  it("stop closes a live socket, cancels every timer and goes offline", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    r.sockets.last.receive(event("s1", "a", 1));
    r.client.request({ type: "cancel", session: "s1" }).catch(() => undefined);
    expect(r.clock.pending).toBeGreaterThan(0);
    const socket = r.sockets.last;
    r.client.stop();
    expect(socket.closed).toBe(true);
    expect(r.clock.pending).toBe(0);
    expect(r.states.at(-1)).toBe("offline");
    // Late traffic from the closed socket changes nothing.
    socket.receive(event("s1", "a", 2));
    socket.drop();
    expect(r.clock.pending).toBe(0);
  });

  it("stop hands over the frames still waiting for a batch", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    r.sockets.last.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    r.sockets.last.receive(event("s1", "a", 1));
    expect(eventSeqs(delivered(r))).toEqual([]);
    r.client.stop();
    expect(eventSeqs(delivered(r))).toEqual(["a:1"]);
    expect(r.cursors.at(-1)).toEqual(["s1", { journal: "a", seq: 1 }]);
  });

  it("a stale socket's late close does not disturb the new connection", () => {
    const r = rig();
    ready(r);
    const old = r.sockets.last;
    old.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    old.drop();
    expect(r.states.at(-1)).toBe("ready");
  });

  it("reconnects_resume_from_cursors_without_duplicates", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    r.client.subscribe("s2");
    const first = r.sockets.last;
    expect(first.commandsOf("subscribe").map((c) => [c.session, c.after])).toEqual([
      ["s1", undefined],
      ["s2", undefined],
    ]);

    first.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    for (const seq of [1, 2, 3]) first.receive(event("s1", "a", seq));
    first.receive({ type: "subscribed", session: "s2", from: "start", journals: ["x"] });
    first.receive(event("s2", "x", 7));
    first.receive(event("s1", "a", 3)); // seen twice
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual(["a:1", "a:2", "a:3", "x:7"]);

    // The network drops; the client comes back and asks for each session after its cursor.
    first.drop();
    r.clock.advance(500);
    const second = r.sockets.last;
    expect(second).not.toBe(first);
    second.open();
    expect(second.commands().map((c) => c.type)).toEqual(["hello"]);
    second.receive({ type: "welcome", device: "iPhone" });
    expect(second.commandsOf("subscribe").map((c) => [c.session, c.after])).toEqual([
      ["s1", { journal: "a", seq: 3 }],
      ["s2", { journal: "x", seq: 7 }],
    ]);

    // The hub replays after the cursor, but a record it sends twice, or an overlap, is dropped.
    second.receive({ type: "subscribed", session: "s1", from: "cursor", journals: ["a"] });
    second.receive(event("s1", "a", 3));
    second.receive(event("s1", "a", 4));
    second.receive(event("s1", "a", 4));
    second.receive({ type: "subscribed", session: "s2", from: "cursor", journals: ["x", "y"] });
    second.receive(event("s2", "y", 1));
    second.receive(event("s2", "x", 8)); // the old journal's last words arrive once
    second.receive(event("s2", "x", 8));
    r.batches.length = 0;
    flush(r);
    expect(delivered(r).map((f) => f.type)).toContain("subscribed");
    expect(eventSeqs(delivered(r))).toEqual(["a:4", "y:1", "x:8"]);

    // After the journal rolled over, the cursor is the newest journal's.
    second.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    expect(r.sockets.last.commandsOf("subscribe").map((c) => [c.session, c.after])).toEqual([
      ["s1", { journal: "a", seq: 4 }],
      ["s2", { journal: "y", seq: 1 }],
    ]);
  });

  it("an unsubscribed session is not resumed and its stray events are dropped", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    r.client.unsubscribe("s1");
    expect(r.sockets.last.commandsOf("unsubscribe").map((c) => c.session)).toEqual(["s1"]);
    r.sockets.last.receive(event("s1", "a", 1));
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual([]);
    r.sockets.last.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    expect(r.sockets.last.commandsOf("subscribe")).toEqual([]);
  });

  it("subscribing while offline sends on ready, with a persisted cursor", () => {
    const r = rig({ cursors: { s1: { journal: "a", seq: 9 } } });
    r.client.subscribe("s1");
    r.client.start();
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("subscribe")).toEqual([]);
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    expect(r.sockets.last.commandsOf("subscribe")[0]).toMatchObject({ session: "s1", after: { journal: "a", seq: 9 } });
    r.sockets.last.receive(event("s1", "a", 9));
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual([]);
  });

  it("a fresh subscribe ignores the saved cursor", () => {
    const r = rig({ cursors: { s1: { journal: "a", seq: 9 } } });
    ready(r);
    r.client.subscribe("s1", true);
    expect(r.sockets.last.commandsOf("subscribe")[0]?.after).toBeUndefined();
    r.sockets.last.receive(event("s1", "a", 1));
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual(["a:1"]);
  });

  it("a_subscribed_reset_clears_the_cursor", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    const socket = r.sockets.last;
    socket.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    socket.receive(event("s1", "a", 1));
    socket.receive(event("s1", "a", 2));
    flush(r);
    r.batches.length = 0;

    // The hub did not know the cursor (its journals were rotated): it replays from the start.
    socket.drop();
    r.clock.advance(500);
    const next = r.sockets.last;
    next.open();
    next.receive({ type: "welcome", device: "iPhone" });
    expect(next.commandsOf("subscribe")[0]?.after).toEqual({ journal: "a", seq: 2 });
    next.receive({ type: "subscribed", session: "s1", from: "start", journals: ["b"] });
    next.receive(event("s1", "b", 1));
    next.receive(event("s1", "a", 1)); // seq 1 of "a" again: new after a reset
    flush(r);

    // The consumer sees the reset, in order, before the replay it should replace the transcript with.
    expect(delivered(r).flatMap((f) => (f.type === "subscribed" ? [`${f.from}`] : f.type === "event" ? [`${f.journal}:${f.seq}`] : [f.type]))).toEqual([
      "start",
      "b:1",
      "a:1",
    ]);
    expect(r.cursors.at(-1)).toEqual(["s1", { journal: "a", seq: 1 }]);

    // The cursor was cleared, not carried over: a reset with no events leaves none.
    next.receive({ type: "subscribed", session: "s1", from: "start", journals: ["b"] });
    flush(r);
    expect(r.cursors.at(-1)).toEqual(["s1", null]);
    next.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    expect(r.sockets.last.commandsOf("subscribe")[0]?.after).toBeUndefined();
  });

  it("reports cursors to the store at most once per session per batch", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    r.sockets.last.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    for (let seq = 1; seq <= 5; seq++) r.sockets.last.receive(event("s1", "a", seq));
    flush(r);
    expect(r.cursors).toEqual([["s1", { journal: "a", seq: 5 }]]);
    flush(r);
    expect(r.cursors).toHaveLength(1);
  });
});

describe("unpairing", () => {
  it.each(["unpaired", "revoked"] as const)(
    "unpaired_and_revoked_move_to_unpaired_and_forget_the_token (%s)",
    (code) => {
      const r = rig();
      ready(r);
      r.sockets.last.receive({ type: "refused", req: 0, code, reason: "This phone was unpaired on the Mac" });
      expect(r.states.at(-1)).toBe("unpaired");
      expect(r.sockets.last.closed).toBe(true);
      expect(r.clock.pending).toBe(0);
      expect(r.client.token).toBeNull();

      // The socket closing afterwards does not start a reconnect, and start() has nothing to say.
      r.sockets.last.drop(1008);
      r.clock.advance(120_000);
      r.client.start();
      expect(r.sockets.all).toHaveLength(1);
      expect(r.states.at(-1)).toBe("unpaired");
      // The refusal is also shown to the consumer.
      flush(r);
      expect(delivered(r).some((f) => f.type === "refused" && f.code === code)).toBe(true);
    },
  );

  it("a refused hello is unpaired too", () => {
    const r = rig();
    r.client.start();
    r.sockets.last.open();
    const req = r.sockets.last.commands()[0]?.req;
    r.sockets.last.receive({ type: "refused", req, code: "unpaired", reason: "Unknown device; pair it again" });
    expect(r.states.at(-1)).toBe("unpaired");
    expect(r.client.token).toBeNull();
  });

  it("a network close never unpairs, and the token is kept", () => {
    const r = rig();
    ready(r);
    for (const code of [1000, 1001, 1006, 1008, 1011, 4401]) {
      r.sockets.last.drop(code);
      expect(r.states.at(-1)).toBe("offline");
      r.clock.advance(30_000);
      r.sockets.last.open();
      expect(r.sockets.last.commands()[0]).toMatchObject({ type: "hello", token: TOKEN });
      r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    }
    expect(r.states).not.toContain("unpaired");
    expect(r.client.token).toBe(TOKEN);
  });

  it("other refusals, matched or not, leave the connection alone", () => {
    const r = rig();
    ready(r);
    r.sockets.last.receive({ type: "refused", req: 0, code: "rate_limited", reason: "Slow down" });
    expect(r.states.at(-1)).toBe("ready");
    flush(r);
    expect(delivered(r).some((f) => f.type === "refused" && f.code === "rate_limited")).toBe(true);
  });

  it("an unmatched refusal with a req is dropped; only req 0 is shown", () => {
    const r = rig();
    ready(r);
    r.sockets.last.receive({ type: "refused", req: 4242, code: "busy", reason: "stray" });
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
  });

  it("a subscribe's ack is not traffic: the batch gets the client's replayed mark in its place", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("live");
    r.sockets.last.receive({ type: "subscribed", session: "live", from: "start", journals: ["j"] });
    r.sockets.last.receive(event("live", "j", 1));
    r.sockets.last.receive({ type: "ack", req: r.sockets.last.commandsOf("subscribe")[0]?.req, message: "ok" });
    flush(r);
    // The hub acks a subscribe after the replay's last record: the mark follows it, in order.
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions", "subscribed", "event", "replayed"]);
    expect(delivered(r)[3]).toEqual({ type: "replayed", session: "live" });
  });

  it("no replayed mark for a session no longer wanted", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("live");
    const req = r.sockets.last.commandsOf("subscribe")[0]?.req;
    r.client.unsubscribe("live");
    r.sockets.last.receive({ type: "ack", req, message: "ok" });
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
  });

  it("a throwing onCursor callback does not break stop or unpair", () => {
    const sockets = new Sockets();
    const clock = new FakeClock();
    const client = new HubClient({
      url: URL,
      token: TOKEN,
      factory: sockets.factory,
      now: clock.now,
      schedule: clock.schedule,
      onCursor: () => {
        throw new Error("disk full");
      },
    });
    client.start();
    sockets.last.open();
    sockets.last.receive({ type: "welcome", device: "p" });
    client.subscribe("s");
    sockets.last.receive({ type: "subscribed", session: "s", from: "start", journals: ["a"] });
    sockets.last.receive(event("s", "a", 1));
    expect(() => client.stop()).not.toThrow();
    expect(client.state).toBe("offline");
  });
});

describe("subscribing again on a live connection", () => {
  /** Ready, subscribed to s1 with a:1..3 taken. */
  function live(): Rig {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    r.sockets.last.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    for (const seq of [1, 2, 3]) r.sockets.last.receive(event("s1", "a", seq));
    flush(r);
    r.batches.length = 0;
    return r;
  }
  const lastReq = (r: Rig, type: string) => r.sockets.last.commandsOf(type).at(-1)?.req;

  it("a fresh subscribe unsubscribes first, drops stale events, and loses no replayed row", () => {
    const r = live();
    const socket = r.sockets.last;
    r.client.subscribe("s1", true);
    expect(socket.commands().at(-1)).toMatchObject({ type: "unsubscribe", session: "s1" });
    expect(socket.commandsOf("subscribe")).toHaveLength(1); // not yet

    // The old subscription's last live events, before its unsubscribe is answered.
    socket.receive(event("s1", "a", 500));
    socket.receive({ type: "subscribed", session: "s1", from: "cursor", journals: ["a"] });
    expect(socket.commandsOf("subscribe")).toHaveLength(1);

    socket.receive({ type: "ack", req: lastReq(r, "unsubscribe"), message: "ok" });
    expect(socket.commandsOf("subscribe")).toHaveLength(2);
    expect(socket.commandsOf("subscribe")[1]?.after).toBeUndefined();

    socket.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    for (const seq of [1, 2, 3, 4]) socket.receive(event("s1", "a", seq));
    flush(r);
    expect(delivered(r).map((f) => (f.type === "event" ? `${f.journal}:${f.seq}` : f.type))).toEqual([
      "subscribed",
      "a:1",
      "a:2",
      "a:3",
      "a:4",
    ]);
  });

  it("forget then subscribe waits for the ack and starts from the start", () => {
    const r = live();
    const socket = r.sockets.last;
    r.client.forget("s1");
    r.client.subscribe("s1");
    expect(socket.commandsOf("unsubscribe")).toHaveLength(1);
    expect(socket.commandsOf("subscribe")).toHaveLength(1);
    socket.receive(event("s1", "a", 500));
    socket.receive({ type: "ack", req: lastReq(r, "unsubscribe"), message: "ok" });
    expect(socket.commandsOf("subscribe")).toHaveLength(2);
    expect(socket.commandsOf("subscribe")[1]?.after).toBeUndefined();
    socket.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    socket.receive(event("s1", "a", 1));
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual(["a:1"]);
  });

  it("unsubscribe then subscribe resumes after the cursor once the ack is in", () => {
    const r = live();
    const socket = r.sockets.last;
    r.client.unsubscribe("s1");
    r.client.subscribe("s1");
    socket.receive(event("s1", "a", 4)); // stale: dropped, and not remembered
    expect(socket.commandsOf("subscribe")).toHaveLength(1);
    socket.receive({ type: "ack", req: lastReq(r, "unsubscribe"), message: "ok" });
    expect(socket.commandsOf("subscribe")[1]?.after).toEqual({ journal: "a", seq: 3 });
    socket.receive({ type: "subscribed", session: "s1", from: "cursor", journals: ["a"] });
    socket.receive(event("s1", "a", 4));
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual(["a:4"]);
  });

  it("an unsubscribe nobody wants subscribed again sends no subscribe", () => {
    const r = live();
    r.client.unsubscribe("s1");
    r.sockets.last.receive({ type: "ack", req: lastReq(r, "unsubscribe"), message: "ok" });
    expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(1);
  });

  it("a refused unsubscribe still lets the subscribe go", () => {
    const r = live();
    r.client.subscribe("s1", true);
    r.sockets.last.receive({ type: "refused", req: lastReq(r, "unsubscribe"), code: "invalid", reason: "x" });
    expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(2);
    flush(r);
    expect(delivered(r)).toEqual([]);
  });

  it("a reconnect during the wait subscribes once on the new connection", () => {
    const r = live();
    r.client.subscribe("s1", true);
    r.sockets.last.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "p" });
    expect(r.sockets.last.commandsOf("unsubscribe")).toHaveLength(0);
    expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(1);
    expect(r.sockets.last.commandsOf("subscribe")[0]?.after).toBeUndefined();
  });

  it("subscribing to a session already live is a no-op", () => {
    const r = live();
    r.client.subscribe("s1");
    expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(1);
    expect(r.sockets.last.commandsOf("unsubscribe")).toHaveLength(0);
  });
});

describe("a refused subscribe", () => {
  function failing(r: Rig): [string, Refusal][] {
    const failed: [string, Refusal][] = [];
    r.client.onSubscribeFailed((session, refusal) => failed.push([session, refusal]));
    return failed;
  }
  const refuse = (r: Rig, code: string, nth: number) =>
    r.sockets.last.receive({
      type: "refused",
      req: r.sockets.last.commandsOf("subscribe")[nth]?.req,
      code,
      reason: `no: ${code}`,
    });

  it("is surfaced at once with its session for most codes, and not as traffic", () => {
    const r = rig();
    const failed = failing(r);
    ready(r);
    r.client.subscribe("gone");
    refuse(r, "unavailable", 0);
    expect(failed).toHaveLength(1);
    expect(failed[0]?.[0]).toBe("gone");
    expect(failed[0]?.[1]).toBeInstanceOf(Refusal);
    expect(failed[0]?.[1]).toMatchObject({ code: "unavailable", message: "no: unavailable" });
    r.clock.advance(60_000);
    expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(1); // no retry
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions"]);
    // The consumer can ask again.
    r.client.subscribe("gone");
    expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(2);
  });

  it.each(["rate_limited", "timeout"])("%s is retried three times 5 s apart, then surfaced", (code) => {
    const r = rig();
    const failed = failing(r);
    ready(r);
    r.client.subscribe("s1");
    for (let n = 0; n < 3; n++) {
      refuse(r, code, n);
      expect(failed).toHaveLength(0);
      expect(r.clock.delays).toEqual([5000]);
      r.clock.advance(4999);
      expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(n + 1);
      r.clock.advance(1);
      expect(r.sockets.last.commandsOf("subscribe")).toHaveLength(n + 2);
    }
    refuse(r, code, 3);
    expect(failed.map(([session, refusal]) => [session, refusal.code])).toEqual([["s1", code]]);
    expect(r.clock.pending).toBe(0);
  });

  it("a retry that is accepted starts the count over", () => {
    const r = rig();
    const failed = failing(r);
    ready(r);
    r.client.subscribe("s1");
    refuse(r, "rate_limited", 0);
    r.clock.advance(5000);
    r.sockets.last.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    r.sockets.last.receive({ type: "ack", req: r.sockets.last.commandsOf("subscribe")[1]?.req, message: "ok" });
    r.client.subscribe("s1", true);
    r.sockets.last.receive({ type: "ack", req: r.sockets.last.commandsOf("unsubscribe")[0]?.req, message: "ok" });
    for (let n = 2; n < 5; n++) {
      refuse(r, "rate_limited", n);
      r.clock.advance(5000);
    }
    expect(failed).toHaveLength(0);
  });

  it("the retry timer is cancelled by stop, unsubscribe and a dropped link", () => {
    for (const cancel of [
      (r: Rig) => r.client.stop(),
      (r: Rig) => r.client.unsubscribe("s1"),
      (r: Rig) => r.client.forget("s1"),
      (r: Rig) => r.sockets.last.drop(),
    ]) {
      const r = rig();
      ready(r);
      r.client.subscribe("s1");
      refuse(r, "rate_limited", 0);
      expect(r.clock.delays).toContain(5000);
      cancel(r);
      r.clock.advance(5000);
      expect(r.sockets.all.at(-1)?.commandsOf("subscribe").length).toBeLessThanOrEqual(1);
    }
  });

  it("revoked on a subscribe unpairs and is not a subscribe failure", () => {
    const r = rig();
    const failed = failing(r);
    ready(r);
    r.client.subscribe("s1");
    refuse(r, "revoked", 0);
    expect(r.client.state).toBe("unpaired");
    expect(failed).toHaveLength(0);
  });
});

describe("hostile input", () => {
  it("garbage_frames_are_ignored", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    const socket = r.sockets.last;
    expect(r.client.garbageFrames).toBe(0);

    const garbage: string[] = [
      "not json",
      "",
      "{",
      "null",
      "42",
      '"text"',
      "[]",
      '{"v":1}',
      '{"v":1,"type":42}',
      '{"v":1,"type":"mystery","x":1}', // an unknown type
      '{"v":2,"type":"sessions","sessions":[]}', // a wrong v
      '{"type":"sessions","sessions":[]}', // no v
      '{"v":"1","type":"sessions","sessions":[]}',
      '{"v":1,"type":"event","session":"s1","journal":"a","seq":"1","event":{"type":"text","data":"x"}}',
      '{"v":1,"type":"event","session":"s1","journal":"a","seq":-1,"event":{"type":"text","data":"x"}}',
      '{"v":1,"type":"event","session":"s1","journal":"a","seq":1.5,"event":{"type":"text","data":"x"}}',
      '{"v":1,"type":"event","session":"s1","journal":"a","seq":1}',
      '{"v":1,"type":"event","session":"s1","journal":"a","seq":1,"event":{"data":"x"}}',
      '{"v":1,"type":"event","session":5,"journal":"a","seq":1,"event":{"type":"text","data":"x"}}',
      '{"v":1,"type":"ack","req":"1","message":"x"}',
      '{"v":1,"type":"refused","req":1}',
      '{"v":1,"type":"subscribed","session":"s1","from":"sideways","journals":[]}',
      '{"v":1,"type":"subscribed","session":"s1","from":"start","journals":[1]}',
      '{"v":1,"type":"sessions","sessions":{}}',
      '{"v":1,"type":"state","session":"s1"}',
      "x".repeat(256 * 1024 + 1), // too big
      JSON.stringify({ v: 1, ...event("s1", "a", 1, "y".repeat(256 * 1024)) }), // big, though valid JSON
      JSON.stringify({ v: 1, ...event("s1", "a", 1, "é".repeat(131_072)) }), // under 256K characters, over 256 KB of UTF-8
    ];
    for (const text of garbage) expect(() => socket.receive(text)).not.toThrow();
    expect(r.client.garbageFrames).toBe(garbage.length);
    expect(r.states.at(-1)).toBe("ready");

    // Still works, and nothing garbage got through.
    socket.receive(event("s1", "a", 1));
    flush(r);
    expect(delivered(r).map((f) => f.type)).toEqual(["sessions", "event"]);
    expect(r.client.garbageFrames).toBe(garbage.length);
  });

  it("a frame of exactly 256 KB is accepted", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("s1");
    const head = JSON.stringify({ v: 1, ...event("s1", "a", 1, "") });
    const text = JSON.stringify({ v: 1, ...event("s1", "a", 1, "y".repeat(256 * 1024 - head.length)) });
    expect(text.length).toBe(256 * 1024);
    r.sockets.last.receive(text);
    flush(r);
    expect(eventSeqs(delivered(r))).toEqual(["a:1"]);
    expect(r.client.garbageFrames).toBe(0);
  });

  it("hostile keys and values do not reach the client's own state", () => {
    const r = rig();
    ready(r);
    r.client.subscribe("__proto__");
    r.sockets.last.receive('{"v":1,"type":"event","session":"__proto__","journal":"__proto__","seq":1,"event":{"type":"text","data":"x"},"__proto__":{"polluted":1}}');
    flush(r);
    expect(({} as Record<string, unknown>).polluted).toBeUndefined();
    expect(eventSeqs(delivered(r))).toEqual(["__proto__:1"]);
  });

  it("a listener that throws does not stop the others or the client", () => {
    const r = rig();
    r.client.onBatch(() => {
      throw new Error("boom");
    });
    const seen: BatchItem[][] = [];
    r.client.onBatch((frames) => seen.push(frames));
    ready(r);
    expect(() => flush(r)).not.toThrow();
    expect(seen).toHaveLength(1);
  });
});

describe("batching", () => {
  it("batches_flush_at_most_every_50ms", () => {
    const r = rig();
    const calls: number[] = [];
    r.client.onBatch(() => calls.push(r.clock.now()));
    ready(r);
    r.client.subscribe("s1");
    r.sockets.last.receive({ type: "subscribed", session: "s1", from: "start", journals: ["a"] });
    flush(r);
    r.batches.length = 0;
    calls.length = 0;

    // A burst in one tick is one call, not one per frame.
    for (let seq = 1; seq <= 100; seq++) r.sockets.last.receive(event("s1", "a", seq));
    expect(r.batches).toHaveLength(0); // nothing synchronous
    r.clock.advance(0);
    expect(r.batches).toHaveLength(1);
    expect(r.batches[0]).toHaveLength(100);

    // A stream of one frame every 10 ms flushes about every 50 ms, never faster.
    let seq = 101;
    for (let t = 0; t < 500; t += 10) {
      r.sockets.last.receive(event("s1", "a", seq++));
      r.clock.advance(10);
    }
    r.clock.advance(50);
    for (let i = 1; i < calls.length; i++) {
      expect((calls[i] ?? 0) - (calls[i - 1] ?? 0)).toBeGreaterThanOrEqual(50);
    }
    expect(calls.length).toBeGreaterThan(5);
    expect(calls.length).toBeLessThanOrEqual(11);
    expect(eventSeqs(delivered(r))).toHaveLength(100 + 50);
    // In order, with nothing lost.
    expect(eventSeqs(delivered(r))).toEqual(Array.from({ length: 150 }, (_, i) => `a:${i + 1}`));
  });

  it("an idle client has no timer", () => {
    const r = rig();
    ready(r);
    flush(r);
    expect(r.clock.pending).toBe(0);
  });

  it("unsubscribing from the batch stream stops the calls", () => {
    const r = rig();
    const seen: number[] = [];
    const off = r.client.onBatch((f) => seen.push(f.length));
    off();
    ready(r);
    flush(r);
    expect(seen).toEqual([]);
  });
});
