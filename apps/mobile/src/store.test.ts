import { act, renderHook } from "@testing-library/react-native";

import { FakeClock, Sockets } from "./client/fakes";
import { HubClient } from "./client/HubClient";
import type { CatalogInfo, JournalEvent, SessionInfo, StateInfo, ToPhone } from "./protocol";
import { entrySize } from "./model/transcript";
import { bindHooks, connectStore, createOctetStore, unwatch, watch, type OctetStore } from "./store";

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
const sessions = (...list: SessionInfo[]): ToPhone => ({ v: 1, type: "sessions", sessions: list });
let seqs = new Map<string, number>();
const ev = (session: string, event: JournalEvent, seq?: number): ToPhone => {
  const n = seq ?? (seqs.get(session) ?? 0) + 1;
  seqs.set(session, n);
  return { v: 1, type: "event", session, journal: "j1", seq: n, event };
};
const text = (session: string, data: string) => ev(session, { type: "text", data });
const user = (session: string, data: string) => ev(session, { type: "user", data });
const subscribed = (session: string, from: "cursor" | "start"): ToPhone => ({ v: 1, type: "subscribed", session, from, journals: ["j1"] });
const stateFrame = (session: string, state: Partial<StateInfo> = {}): ToPhone => ({
  v: 1,
  type: "state",
  session,
  state: { phase: "idle", queue: [], queued: 0, mode: "ask", approvals: [], ...state },
});
const catalogFrame = (session: string): ToPhone => {
  const catalog: CatalogInfo = { modes: ["ask"], current_mode: "ask", models: [], current: null };
  return { v: 1, type: "catalog", session, catalog };
};

let store: OctetStore;
beforeEach(() => {
  seqs = new Map();
  store = createOctetStore();
});
const view = (id: string) => {
  const v = store.getState().bySession[id];
  if (!v) throw new Error(`no view for ${id}`);
  return v;
};

describe("the sessions list", () => {
  it("is updated by a sessions frame", () => {
    store.getState().applyBatch([sessions(info("a"), info("b", { goal: "ship it" }))]);
    expect(store.getState().sessions.map((s) => s.id)).toEqual(["a", "b"]);
    expect(store.getState().sessions[1]?.goal).toBe("ship it");
  });

  it("keeps identity for sessions that did not change", () => {
    store.getState().applyBatch([sessions(info("a"), info("b"))]);
    const before = store.getState().sessions;
    store.getState().applyBatch([sessions(info("a"), info("b"))]);
    expect(store.getState().sessions).toBe(before);
    store.getState().applyBatch([sessions(info("a"), info("b", { phase: "running" }))]);
    const after = store.getState().sessions;
    expect(after).not.toBe(before);
    expect(after[0]).toBe(before[0]);
    expect(after[1]).not.toBe(before[1]);
  });

  it("cleans the strings and skips entries without an id", () => {
    const junk = [info("a", { now: "Running \u001b[1mls\u0007", workspace: "/w".repeat(100_000) }), { engine: "claude" }, null, 5] as unknown as SessionInfo[];
    store.getState().applyBatch([sessions(...junk)]);
    const [only, ...rest] = store.getState().sessions;
    expect(rest).toEqual([]);
    expect(only?.now).toBe("Running ls");
    expect((only?.workspace ?? "").length).toBeLessThan(10_000);
  });
});

describe("events", () => {
  it("an event for session A does not change session B", () => {
    store.getState().applyBatch([sessions(info("a"), info("b")), user("a", "hi"), user("b", "yo")]);
    const b = view("b");
    const bTranscript = b.transcript;
    const before = store.getState();
    store.getState().applyBatch([text("a", "hello")]);
    expect(view("b")).toBe(b);
    expect(view("b").transcript).toBe(bTranscript);
    expect(store.getState().sessions).toBe(before.sessions);
    expect(view("a").transcript.entries).toHaveLength(2);
    // And inside A, the user entry kept its identity.
    expect(view("a").transcript.entries[0]).toMatchObject({ kind: "user", text: "hi" });
  });

  it("the untouched entries of a session keep their identity", () => {
    store.getState().applyBatch([user("a", "hi"), text("a", "he")]);
    const first = view("a").transcript.entries[0];
    store.getState().applyBatch([text("a", "llo")]);
    expect(view("a").transcript.entries[0]).toBe(first);
    expect(view("a").transcript.entries[1]).toMatchObject({ text: "hello" });
  });

  it("a whole batch is one commit", () => {
    const calls: number[] = [];
    store.subscribe(() => calls.push(1));
    const frames: ToPhone[] = [sessions(info("a"), info("b"))];
    for (let i = 0; i < 100; i++) frames.push(text(i % 2 ? "a" : "b", "w "));
    store.getState().applyBatch(frames);
    expect(calls).toHaveLength(1);
    expect(view("a").transcript.entries[0]).toMatchObject({ text: "w ".repeat(50) });
  });

  it("an empty or all-ignored batch commits nothing", () => {
    store.getState().applyBatch([user("a", "hi")]);
    const calls: number[] = [];
    store.subscribe(() => calls.push(1));
    const ignored: ToPhone[] = [
      subscribed("a", "cursor"),
      ev("a", { type: "ready", data: "id" }),
      ev("a", { type: "models", data: [] }),
      { v: 1, type: "event", session: "a", journal: "j1", seq: 99, event: { type: "from_the_future", data: 1 } },
      { v: 1, type: "ack", req: 1, message: "ok" },
    ];
    store.getState().applyBatch(ignored);
    store.getState().applyBatch([]);
    expect(calls).toEqual([]);
  });

  it("keeps the mode, model and usage records", () => {
    store.getState().applyBatch([ev("a", { type: "mode", data: "auto" }), ev("a", { type: "usage", data: "1k" })]);
    expect(view("a")).toMatchObject({ mode: "auto", usage: "1k" });
  });

  it("survives garbage event data", () => {
    const junk = ev("a", { type: "tool", data: 5 } as unknown as JournalEvent);
    expect(() => store.getState().applyBatch([junk, ev("a", { type: "text", data: null } as unknown as JournalEvent)])).not.toThrow();
  });

  it("bounds the transcript at 1,000 entries even when one batch brings 3,000", () => {
    const frames: ToPhone[] = [];
    for (let i = 0; i < 3_000; i++) frames.push(user("a", `m${i}`));
    store.getState().applyBatch(frames);
    const t = view("a").transcript;
    expect(t.entries).toHaveLength(1_000);
    expect(t.bytes).toBe(t.entries.reduce((n, e) => n + entrySize(e), 0));
  });
});

describe("a_reset_replaces_the_transcript", () => {
  it("replaces it when subscribed says from start, in order within the batch", () => {
    store.getState().applyBatch([user("a", "old one"), text("a", "old two"), ev("a", { type: "approval", data: { id: 1, detail: "{}", summary: { action: "x", files: [], scope: "s" } } })]);
    expect(view("a").transcript.entries).toHaveLength(2);
    expect(view("a").approvals).toHaveLength(1);
    store.getState().applyBatch([text("a", "stale live"), subscribed("a", "start"), user("a", "new one"), text("a", "new two")]);
    expect(view("a").transcript.entries).toMatchObject([{ kind: "user", text: "new one" }, { kind: "reply", text: "new two" }]);
    expect(view("a").approvals).toEqual([]);
  });

  it("leaves other sessions alone", () => {
    store.getState().applyBatch([user("a", "x"), user("b", "y")]);
    const b = view("b");
    store.getState().applyBatch([subscribed("a", "start")]);
    expect(view("a").transcript.entries).toEqual([]);
    expect(view("b")).toBe(b);
  });

  it("a replay that arrives across batches ends in the same transcript", () => {
    store.getState().applyBatch([user("a", "old")]);
    store.getState().applyBatch([subscribed("a", "start")]);
    store.getState().applyBatch([user("a", "one"), text("a", "two")]);
    store.getState().applyBatch([text("a", " three")]);
    expect(view("a").transcript.entries).toMatchObject([{ text: "one" }, { text: "two three" }]);
  });
});

describe("state and catalog", () => {
  it("are stored per session", () => {
    store.getState().applyBatch([stateFrame("a", { now: "Reading", queue: ["next"], queued: 1 }), stateFrame("b", { now: "Idle" }), catalogFrame("a")]);
    expect(view("a").state?.now).toBe("Reading");
    expect(view("a").state?.queue).toEqual(["next"]);
    expect(view("b").state?.now).toBe("Idle");
    expect(view("a").catalog?.modes).toEqual(["ask"]);
    expect(view("b").catalog).toBeNull();
  });

  it("take the state's approvals as the pending ones", () => {
    store.getState().applyBatch([stateFrame("a", { approvals: [{ id: 3, summary: { action: "Edit", files: ["a.rs"], scope: "once" } }] })]);
    expect(view("a").approvals.map((a) => a.id)).toEqual([3]);
    store.getState().applyBatch([ev("a", { type: "approval_closed", data: 3 })]);
    expect(view("a").approvals).toEqual([]);
  });

  it("an identical state frame changes nothing", () => {
    store.getState().applyBatch([stateFrame("a", { now: "x" })]);
    const before = store.getState();
    store.getState().applyBatch([stateFrame("a", { now: "x" })]);
    expect(store.getState()).toBe(before);
  });
});

describe("sessions that end", () => {
  it("lose their view and are forgotten by the client", () => {
    store.getState().applyBatch([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    const ended = store.getState().applyBatch([sessions(info("b"))]);
    expect(ended).toEqual(["a"]);
    expect(store.getState().bySession.a).toBeUndefined();
    expect(store.getState().bySession.b).toBeDefined();
  });

  it("keep the view of a session never listed (events before the list)", () => {
    store.getState().applyBatch([user("early", "x")]);
    expect(store.getState().applyBatch([sessions(info("b"))])).toEqual([]);
    expect(view("early")).toBeDefined();
  });
});

describe("hub state and failures", () => {
  it("keeps the hub state", () => {
    expect(store.getState().hub).toBe("offline");
    store.getState().setHub("ready");
    expect(store.getState().hub).toBe("ready");
  });

  it("marks a session failed", () => {
    store.getState().applyBatch([user("a", "x")]);
    store.getState().subscribeFailed("a", "Not allowed");
    expect(view("a").failed).toBe("Not allowed");
    expect(view("a").transcript.entries).toHaveLength(1);
  });
});

describe("with a client", () => {
  function rig() {
    const sockets = new Sockets();
    const clock = new FakeClock();
    const client = new HubClient({ url: "wss://mac/", token: "t", factory: sockets.factory, now: clock.now, schedule: clock.schedule, random: () => 1 });
    const off = connectStore(store, client);
    client.start();
    sockets.last.open();
    sockets.last.receive({ type: "welcome", device: "iPhone" });
    clock.advance(50);
    return { sockets, clock, client, off };
  }

  it("follows the client's state", () => {
    const { client, off } = rig();
    expect(store.getState().hub).toBe("ready");
    expect(client.state).toBe("ready");
    off();
  });

  it("delivers a client batch as one commit, and a reset in its place", () => {
    const { sockets, clock, client, off } = rig();
    watch(store, client, "a");
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "hello" } });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 2, event: { type: "text", data: "hi " } });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 3, event: { type: "text", data: "there" } });
    const calls: number[] = [];
    store.subscribe(() => calls.push(1));
    clock.advance(50);
    expect(calls).toHaveLength(1);
    expect(view("a").transcript.entries).toMatchObject([{ text: "hello" }, { text: "hi there" }]);
    off();
  });

  it("a non-retryable subscribe refusal forgets the session and marks its view failed", () => {
    const { sockets, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    watch(store, client, "a");
    const req = sockets.last.commandsOf("subscribe")[0]?.req;
    sockets.last.receive({ type: "refused", req, reason: "That session is not allowed", code: "not_allowed" });
    expect(forget).toHaveBeenCalledWith("a");
    expect(view("a").failed).toBe("That session is not allowed");
    off();
  });

  it("forgets a session that ended", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a"), info("b")] });
    clock.advance(50);
    sockets.last.receive({ type: "sessions", sessions: [info("b")] });
    clock.advance(50);
    expect(forget).toHaveBeenCalledWith("a");
    expect(forget).not.toHaveBeenCalledWith("b");
    off();
  });

  it("watch subscribes fresh only a session the store holds nothing of", () => {
    const { sockets, client, off } = rig();
    const subscribe = jest.spyOn(client, "subscribe");
    watch(store, client, "a");
    expect(subscribe).toHaveBeenLastCalledWith("a", true);
    store.getState().applyBatch([user("a", "x")]);
    watch(store, client, "a");
    expect(subscribe).toHaveBeenLastCalledWith("a", false);
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    off();
  });

  it("watch asked twice before any event does not resubscribe", () => {
    const { sockets, client, off } = rig();
    watch(store, client, "a");
    watch(store, client, "a");
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(0);
    off();
  });

  it("unwatch unsubscribes and keeps the transcript for a resume", () => {
    const { sockets, client, off } = rig();
    watch(store, client, "a");
    store.getState().applyBatch([user("a", "x")]);
    unwatch(client, "a");
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(1);
    expect(view("a").transcript.entries).toHaveLength(1);
    const subscribe = jest.spyOn(client, "subscribe");
    watch(store, client, "a");
    expect(subscribe).toHaveBeenLastCalledWith("a", false);
    off();
  });

  it("stops listening when disconnected", () => {
    const { off, sockets, clock } = rig();
    off();
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    clock.advance(50);
    expect(store.getState().sessions).toEqual([]);
  });
});

describe("selectors", () => {
  it("re-render only the session that changed", async () => {
    const hooks = bindHooks(store);
    store.getState().applyBatch([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    let rendersA = 0;
    let rendersB = 0;
    const a = await renderHook(() => {
      rendersA++;
      return hooks.useSession("a");
    });
    const b = await renderHook(() => {
      rendersB++;
      return hooks.useSession("b");
    });
    const [baseA, baseB] = [rendersA, rendersB];
    await act(() => {
      store.getState().applyBatch([text("a", "hello")]);
    });
    expect(rendersA).toBeGreaterThan(baseA);
    expect(rendersB).toBe(baseB);
    expect(a.result.current.transcript.entries).toHaveLength(2);
    expect(b.result.current.transcript.entries).toHaveLength(1);
  });

  it("useSessions ignores a frame that changed nothing, and an event stream", async () => {
    const hooks = bindHooks(store);
    store.getState().applyBatch([sessions(info("a"))]);
    let renders = 0;
    await renderHook(() => {
      renders++;
      return hooks.useSessions();
    });
    const base = renders;
    await act(() => {
      store.getState().applyBatch([sessions(info("a")), text("a", "streaming")]);
    });
    expect(renders).toBe(base);
    await act(() => {
      store.getState().applyBatch([sessions(info("a", { phase: "running" }))]);
    });
    expect(renders).toBeGreaterThan(base);
  });

  it("useSession gives a stable empty view for a session not seen", async () => {
    const hooks = bindHooks(store);
    const h = await renderHook(() => hooks.useSession("nope"));
    const first = h.result.current;
    await act(() => {
      store.getState().applyBatch([user("other", "x")]);
    });
    expect(h.result.current).toBe(first);
    expect(first.transcript.entries).toEqual([]);
  });

  it("useApprovals and useHub select their parts", async () => {
    const hooks = bindHooks(store);
    store.getState().applyBatch([stateFrame("a", { approvals: [{ id: 1, summary: { action: "x", files: [], scope: "s" } }] })]);
    const approvals = await renderHook(() => hooks.useApprovals("a"));
    const hub = await renderHook(() => hooks.useHub());
    expect(approvals.result.current.map((x) => x.id)).toEqual([1]);
    expect(hub.result.current).toBe("offline");
    await act(() => {
      store.getState().setHub("ready");
    });
    expect(hub.result.current).toBe("ready");
  });
});
