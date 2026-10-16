// The store with more than one Mac: every session keyed by its Mac, so that two Macs that report
// the same raw id (`{pid}-{started}`) never share a row, a view or an answer.
import { FakeClock, Sockets } from "./client/fakes";
import { HubClient, Refusal } from "./client/HubClient";
import { sendVia } from "./components/send";
import type { Links } from "./macs";
import * as model from "./model/session";
import type { SessionInfo } from "./protocol";
import { MAX_VIEWS, bestHub, connectStore, createOctetStore, releaseSession, unwatch, watch, type OctetStore } from "./store";

const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/work/${id}`,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

const GIST = { action: "Run tests", files: [], scope: "once" };

let store: OctetStore;
beforeEach(() => {
  store = createOctetStore();
});
afterEach(() => jest.restoreAllMocks());

function macs(...ids: string[]) {
  const clock = new FakeClock();
  const rigs = new Map(
    ids.map((mac) => {
      const sockets = new Sockets();
      const client = new HubClient({
        url: `wss://${mac}.tail1.ts.net/`,
        token: "t",
        factory: sockets.factory,
        now: clock.now,
        schedule: clock.schedule,
        random: () => 1,
      });
      const off = connectStore(store, client, { mac, now: clock.now, schedule: clock.schedule });
      client.start();
      sockets.last.open();
      sockets.last.receive({ type: "welcome", device: "iPhone" });
      return [mac, { sockets, client, off, socket: () => sockets.last }] as const;
    }),
  );
  clock.advance(50);
  const rig = (mac: string) => {
    const r = rigs.get(mac);
    if (!r) throw new Error(mac);
    return r;
  };
  const links: Links = { client: (mac) => rigs.get(mac)?.client ?? null, macs: () => [...rigs.keys()] };
  /** One Mac's frames, as its socket brings them. */
  const hear = (mac: string, ...frames: object[]) => {
    for (const f of frames) rig(mac).socket().receive(f);
  };
  /** The clients' 50 ms, then the coalescer's: a second Mac's batch waits out the first's commit. */
  const tick = () => clock.advance(100);
  return { clock, rig, links, hear, tick };
}

const user = (session: string, seq: number, data: string) => ({ type: "event", session, journal: "j1", seq, event: { type: "user", data } });

describe("same_raw_id_on_two_macs", () => {
  it("gives two rows, two views and two approvals, and an answer reaches only the owning Mac", async () => {
    const { rig, links, hear, tick } = macs("m1", "m2");
    hear("m1", { type: "sessions", sessions: [info("123-456")] });
    hear("m2", { type: "sessions", sessions: [info("123-456", { approvals: 1, approval: { id: 7, summary: GIST } })] });
    tick();
    const rows = store.getState().sessions;
    expect(rows.map((s) => s.id)).toEqual(["m1/123-456", "m2/123-456"]);
    expect(rows.map((s) => [s.mac, s.raw])).toEqual([
      ["m1", "123-456"],
      ["m2", "123-456"],
    ]);

    watch(store, links, "m1/123-456");
    watch(store, links, "m2/123-456");
    expect(rig("m1").socket().commandsOf("subscribe").map((c) => c.session)).toEqual(["123-456"]);
    expect(rig("m2").socket().commandsOf("subscribe").map((c) => c.session)).toEqual(["123-456"]);
    hear("m1", { type: "subscribed", session: "123-456", from: "start", journals: ["j1"] }, user("123-456", 1, "from m1"));
    hear(
      "m2",
      { type: "subscribed", session: "123-456", from: "start", journals: ["j1"] },
      user("123-456", 1, "from m2"),
      { type: "state", session: "123-456", state: { phase: "idle", queue: [], queued: 0, mode: "ask", approvals: [{ id: 7, summary: GIST }] } },
    );
    tick();
    const views = store.getState().bySession;
    expect([...views.keys()].sort()).toEqual(["m1/123-456", "m2/123-456"]);
    expect(views.get("m1/123-456")?.transcript.entries).toMatchObject([{ text: "from m1" }]);
    expect(views.get("m2/123-456")?.transcript.entries).toMatchObject([{ text: "from m2" }]);
    expect(views.get("m1/123-456")?.approvals).toHaveLength(0);
    expect(views.get("m2/123-456")?.approvals.map((a) => a.id)).toEqual([7]);

    // The answer names the key; it leaves on m2's socket alone, with the hub's own id.
    void sendVia(links, undefined, { type: "answer", session: "m2/123-456", approval: 7, allow: true });
    expect(rig("m2").socket().commandsOf("answer")).toEqual([expect.objectContaining({ session: "123-456", approval: 7, allow: true })]);
    expect(rig("m1").socket().commandsOf("answer")).toEqual([]);
    // And a send bound to a key goes there too.
    void sendVia(links, "m1/123-456", { type: "prompt", session: "m1/123-456", text: "hi" });
    expect(rig("m1").socket().commandsOf("prompt")).toEqual([expect.objectContaining({ session: "123-456", text: "hi" })]);
    expect(rig("m2").socket().commandsOf("prompt")).toEqual([]);
  });

  it("refuses, sending nothing, a raw id, a key on no Mac, a body for another Mac, and no Mac when there are several", async () => {
    const { rig, links } = macs("m1", "m2");
    const refused = async (p: Promise<unknown>) => {
      const error = await p.then(() => null, (e: unknown) => e);
      expect(error).toBeInstanceOf(Refusal);
      return error as Refusal;
    };
    await refused(sendVia(links, undefined, { type: "answer", session: "123-456", approval: 1, allow: true }));
    await refused(sendVia(links, undefined, { type: "answer", session: "m9/123-456", approval: 1, allow: true }));
    await refused(sendVia(links, "m1/123-456", { type: "answer", session: "m2/123-456", approval: 1, allow: true }));
    const choose = await refused(sendVia(links, undefined, { type: "recent" }));
    expect(choose.code).toBe("unsent");
    expect(choose.message).toBe("Choose a Mac");
    for (const mac of ["m1", "m2"]) {
      expect(rig(mac).socket().commandsOf("answer")).toEqual([]);
      expect(rig(mac).socket().commandsOf("recent")).toEqual([]);
    }
  });

  it("sends a body with no session to the only Mac", () => {
    const { rig, links } = macs("m1");
    void sendVia(links, undefined, { type: "recent" });
    expect(rig("m1").socket().commandsOf("recent")).toHaveLength(1);
  });
});

describe("a Mac's sessions frame", () => {
  it("replaces only that Mac's rows: the other Mac's keep their identity", () => {
    const { hear, tick } = macs("m1", "m2");
    hear("m1", { type: "sessions", sessions: [info("a"), info("b")] });
    hear("m2", { type: "sessions", sessions: [info("x"), info("y")] });
    tick();
    const before = store.getState().sessions.filter((s) => s.mac === "m2");
    hear("m1", { type: "sessions", sessions: [info("c")] });
    tick();
    const after = store.getState().sessions;
    expect(after.map((s) => s.id)).toEqual(["m1/c", "m2/x", "m2/y"]);
    const m2 = after.filter((s) => s.mac === "m2");
    expect(m2).toHaveLength(2);
    m2.forEach((row, i) => expect(row).toBe(before[i]));
  });

  it("ends only that Mac's sessions", () => {
    const { links, hear, tick } = macs("m1", "m2");
    hear("m1", { type: "sessions", sessions: [info("a")] });
    hear("m2", { type: "sessions", sessions: [info("a")] });
    tick();
    watch(store, links, "m1/a");
    watch(store, links, "m2/a");
    hear("m1", user("a", 1, "one"));
    hear("m2", user("a", 1, "two"));
    tick();
    hear("m1", { type: "sessions", sessions: [] });
    tick();
    expect(store.getState().bySession.get("m1/a")?.ended).toBe(true);
    expect(store.getState().bySession.get("m2/a")?.ended).toBe(false);
  });

  it("a stray frame costs only itself: the valid frames of both Macs apply, the foreign one is dropped", () => {
    const warn = jest.spyOn(console, "warn").mockImplementation(() => undefined);
    const ev = (session: string, seq: number, data: string) => ({ v: 1, type: "event", session, journal: "j1", seq, event: { type: "user", data } }) as const;
    const ended = store.getState().applyBatches([
      { mac: "m1", frames: [{ v: 1, type: "sessions", sessions: [info("m1/a")] }, ev("m1/a", 1, "from m1")] },
      // m2's batch holds a frame keyed for m1, and one of its own.
      { mac: "m2", frames: [ev("m1/a", 2, "stray"), { v: 1, type: "sessions", sessions: [info("m2/b"), info("m1/z")] }, ev("m2/b", 1, "from m2")] },
    ]);
    expect(ended).toEqual([[], []]);
    const s = store.getState();
    expect(s.bySession.get("m1/a")?.transcript.entries).toMatchObject([{ text: "from m1" }]);
    expect(s.bySession.get("m2/b")?.transcript.entries).toMatchObject([{ text: "from m2" }]);
    // m2's list keeps only its own row: m1's rows are m1's to list.
    expect(s.sessions.map((x) => x.id)).toEqual(["m1/a", "m2/b"]);
    expect(warn).toHaveBeenCalled();
  });

  it("a batch the store cannot apply costs only its own Mac, and says so", () => {
    const error = jest.spyOn(console, "error").mockImplementation(() => undefined);
    const real = model.reduceView;
    jest.spyOn(model, "reduceView").mockImplementation((view, ops) => {
      if (ops.some((op) => op.op === "event" && (op.ev as { data?: unknown }).data === "boom")) throw new Error("boom");
      return real(view, ops);
    });
    const ev = (session: string, data: string) => ({ v: 1, type: "event", session, journal: "j1", seq: 1, event: { type: "user", data } }) as const;
    const ended = store.getState().applyBatches([
      { mac: "m1", frames: [{ v: 1, type: "sessions", sessions: [info("m1/a")] }, ev("m1/a", "boom")] },
      { mac: "m2", frames: [{ v: 1, type: "sessions", sessions: [info("m2/b")] }, ev("m2/b", "fine")] },
    ]);
    expect(ended).toEqual([null, []]);
    const s = store.getState();
    // Nothing of m1's batch is in: not its list, not its listed flag.
    expect(s.sessions.map((x) => x.id)).toEqual(["m2/b"]);
    expect(s.listed.has("m1")).toBe(false);
    expect(s.bySession.get("m2/b")?.transcript.entries).toMatchObject([{ text: "fine" }]);
    expect(error).toHaveBeenCalled();
  });
});

describe("a new client for the same Mac", () => {
  it("keeps its first list when the old client's feed is torn down after it connected", () => {
    const clock = new FakeClock();
    const connect = () => {
      const sockets = new Sockets();
      const client = new HubClient({ url: "wss://m1.tail1.ts.net/", token: "t", factory: sockets.factory, now: clock.now, schedule: clock.schedule, random: () => 1 });
      const off = connectStore(store, client, { mac: "m1", now: clock.now, schedule: clock.schedule });
      client.start();
      sockets.last.open();
      sockets.last.receive({ type: "welcome", device: "iPhone" });
      return { socket: sockets.last, off };
    };
    const old = connect();
    old.socket.receive({ type: "sessions", sessions: [info("a")] });
    clock.advance(0); // the old feed's list commits, now
    expect(store.getState().sessions.map((x) => x.id)).toEqual(["m1/a"]);
    // The new client connects and lists within 50 ms of that commit: its batch waits.
    const fresh = connect();
    fresh.socket.receive({ type: "sessions", sessions: [info("b")] });
    clock.advance(1);
    old.off();
    clock.advance(100);
    expect(store.getState().sessions.map((x) => x.id)).toEqual(["m1/b"]);
    fresh.off();
  });
});

describe("a refusal on one Mac", () => {
  it("fails only that Mac's session of the raw id", () => {
    const { rig, links } = macs("m1", "m2");
    watch(store, links, "m1/123-456");
    watch(store, links, "m2/123-456");
    const req = rig("m1").socket().commandsOf("subscribe")[0]?.req;
    rig("m1").socket().receive({ type: "refused", req, reason: "No such session", code: "invalid" });
    expect(store.getState().bySession.get("m1/123-456")?.failed).toBe("No such session");
    expect(store.getState().bySession.get("m2/123-456")?.failed ?? null).toBeNull();
  });
});

describe("the view budget", () => {
  it("counts views across Macs: with 4 live, a 5th on the other Mac lets the oldest go", () => {
    expect(MAX_VIEWS).toBe(4);
    const { rig, links, hear, tick } = macs("m1", "m2");
    hear("m1", { type: "sessions", sessions: ["a", "b", "c", "d"].map((id) => info(id)) });
    hear("m2", { type: "sessions", sessions: [info("e")] });
    tick();
    const forget1 = jest.spyOn(rig("m1").client, "forget");
    const visit = (mac: string, id: string) => {
      watch(store, links, `${mac}/${id}`);
      hear(mac, { type: "subscribed", session: id, from: "start", journals: ["j1"] }, user(id, 1, `hi ${id}`));
      tick();
      unwatch(links, `${mac}/${id}`);
      tick();
    };
    for (const id of ["a", "b", "c", "d"]) visit("m1", id);
    const holding = () =>
      [...store.getState().bySession].filter(([, v]) => v.transcript.entries.length > 0).map(([k]) => k).sort();
    expect(holding()).toEqual(["m1/a", "m1/b", "m1/c", "m1/d"]);
    visit("m2", "e");
    expect(holding()).toEqual(["m1/b", "m1/c", "m1/d", "m2/e"]);
    expect(forget1).toHaveBeenCalledWith("a");
  });

  it("an unwatch and a release reach the owning Mac with the raw id", () => {
    const { rig, links, hear, tick } = macs("m1", "m2");
    hear("m2", { type: "sessions", sessions: [info("a")] });
    tick();
    watch(store, links, "m2/a");
    hear("m2", user("a", 1, "x"));
    tick();
    unwatch(links, "m2/a");
    expect(rig("m2").socket().commandsOf("unsubscribe").map((c) => c.session)).toEqual(["a"]);
    expect(rig("m1").socket().commandsOf("unsubscribe")).toEqual([]);
    hear("m2", { type: "sessions", sessions: [] });
    tick();
    const forget = jest.spyOn(rig("m2").client, "forget");
    releaseSession(store, links, "m2/a");
    expect(forget).toHaveBeenCalledWith("a");
    expect(store.getState().bySession.has("m2/a")).toBe(false);
  });
});

describe("reset of one Mac", () => {
  it("keeps the other Mac's sessions, views, link state and drafts", () => {
    const { links, hear, tick } = macs("m1", "m2");
    hear("m1", { type: "sessions", sessions: [info("a")] });
    hear("m2", { type: "sessions", sessions: [info("a")] });
    tick();
    watch(store, links, "m1/a");
    watch(store, links, "m2/a");
    hear("m1", user("a", 1, "one"));
    hear("m2", user("a", 1, "two"));
    tick();
    store.getState().setDraft("m1/a", "draft one");
    store.getState().setDraft("m2/a", "draft two");
    store.getState().remember("m1/a", "sent one");
    store.getState().remember("m2/a", "sent two");
    store.getState().setReplyTo("m1/a");
    const kept = store.getState().bySession.get("m2/a");

    store.getState().reset("m1");
    const s = store.getState();
    expect(s.sessions.map((x) => x.id)).toEqual(["m2/a"]);
    expect([...s.bySession.keys()]).toEqual(["m2/a"]);
    expect(s.bySession.get("m2/a")).toBe(kept);
    expect([...s.hubs.keys()]).toEqual(["m2"]);
    expect(s.listed.has("m1")).toBe(false);
    expect(s.listed.has("m2")).toBe(true);
    expect([...s.drafts.keys()]).toEqual(["m2/a"]);
    expect([...s.sent.keys()]).toEqual(["m2/a"]);
    expect(s.replyTo).toBeNull();
    expect(bestHub(s)).toBe("ready");

    // No argument: everything goes.
    store.getState().reset();
    expect(store.getState().sessions).toEqual([]);
    expect(store.getState().bySession.size).toBe(0);
    expect(store.getState().hubs.size).toBe(0);
    expect(bestHub(store.getState())).toBe("offline");
  });
});

describe("link state per Mac", () => {
  it("is kept for each Mac, and the best of them is the app's", () => {
    store.getState().setHub("m1", "connecting");
    store.getState().setHub("m2", "offline");
    expect(bestHub(store.getState())).toBe("connecting");
    store.getState().setHub("m2", "ready");
    expect(bestHub(store.getState())).toBe("ready");
    expect(store.getState().hubs.get("m1")).toBe("connecting");
  });

  it("a ready link forgets only its own Mac's answered questions", () => {
    store.getState().applyBatch("m1", [{ v: 1, type: "sessions", sessions: [info("m1/a", { question: true })] }]);
    store.getState().applyBatch("m2", [{ v: 1, type: "sessions", sessions: [info("m2/a", { question: true })] }]);
    store.getState().markAnswered("m1/a");
    store.getState().markAnswered("m2/a");
    store.getState().setHub("m1", "connecting");
    store.getState().setHub("m1", "ready");
    expect([...store.getState().answered]).toEqual(["m2/a"]);
  });
});
