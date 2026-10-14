import { act, renderHook } from "@testing-library/react-native";

import { FakeClock, Sockets } from "./client/fakes";
import { HubClient } from "./client/HubClient";
import type { CatalogInfo, JournalEvent, SessionInfo, StateInfo, ToPhone } from "./protocol";
import { entrySize } from "./model/transcript";
import { MAX_VIEWS, bindHooks, connectStore, createOctetStore, releaseSession, unwatch, watch, type OctetStore } from "./store";

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
  const v = store.getState().bySession.get(id);
  if (!v) throw new Error(`no view for ${id}`);
  return v;
};

describe("reset", () => {
  it("forgets the sessions, their transcripts and the connection state, and keeps working", () => {
    store.getState().applyBatch([sessions(info("a")), subscribed("a", "start"), text("a", "hello")]);
    store.getState().setHub("ready");
    store.getState().reset();
    expect(store.getState().sessions).toEqual([]);
    expect(store.getState().bySession.size).toBe(0);
    expect(store.getState().hub).toBe("offline");
    store.getState().applyBatch([sessions(info("b"))]);
    expect(store.getState().sessions.map((s) => s.id)).toEqual(["b"]);
  });
});

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

describe("a fresh replay keeps the previous transcript on screen", () => {
  it("until the replay's first page commits, across batches", async () => {
    const hooks = bindHooks(store);
    store.getState().applyBatch([user("a", "old"), text("a", "reply")]);
    const before = view("a").transcript.entries;
    const shown = await renderHook(() => hooks.useEntries("a"));
    expect(shown.result.current).toBe(before);
    await act(() => {
      store.getState().applyBatch([subscribed("a", "start")]);
    });
    expect(view("a").replaying).toBe(true);
    expect(view("a").transcript.entries).toEqual([]);
    expect(shown.result.current).toBe(before); // no empty flash
    await act(() => {
      store.getState().applyBatch([user("a", "new")]);
    });
    expect(view("a").replaying).toBe(false);
    expect(view("a").held).toBeNull();
    expect(shown.result.current).toMatchObject([{ kind: "user", text: "new" }]);
  });

  it("holds what the batch had before the reset, stale live records included", () => {
    store.getState().applyBatch([user("a", "old")]);
    store.getState().applyBatch([text("a", "late"), subscribed("a", "start")]);
    expect(view("a").held).toMatchObject([{ text: "old" }, { text: "late" }]);
  });

  it("holds nothing when the reset and the first page come in one batch", () => {
    store.getState().applyBatch([user("a", "old")]);
    store.getState().applyBatch([subscribed("a", "start"), user("a", "new")]);
    expect(view("a").replaying).toBe(false);
    expect(view("a").held).toBeNull();
    expect(view("a").transcript.entries).toMatchObject([{ text: "new" }]);
  });

  it("an empty replay ends the hold when the client says the replay is over", async () => {
    const hooks = bindHooks(store);
    store.getState().applyBatch([user("a", "old")]);
    const shown = await renderHook(() => hooks.useEntries("a"));
    await act(() => {
      store.getState().applyBatch([subscribed("a", "start")]);
    });
    expect(shown.result.current).toMatchObject([{ text: "old" }]);
    await act(() => {
      store.getState().applyBatch([{ type: "replayed", session: "a" }]);
    });
    expect(view("a").replaying).toBe(false);
    expect(view("a").held).toBeNull();
    expect(shown.result.current).toEqual([]);
  });

  it("a replayed mark for a session the store holds nothing of makes no view", () => {
    store.getState().applyBatch([{ type: "replayed", session: "nobody" }]);
    expect(store.getState().bySession.has("nobody")).toBe(false);
  });

  it("each reset moves the view's epoch on", () => {
    store.getState().applyBatch([user("a", "old")]);
    const before = view("a").epoch;
    store.getState().applyBatch([subscribed("a", "start"), user("a", "new")]);
    expect(view("a").epoch).toBe(before + 1);
    store.getState().applyBatch([user("a", "more")]);
    expect(view("a").epoch).toBe(before + 1);
  });

  it("a second reset during a replay keeps the transcript first held", () => {
    store.getState().applyBatch([user("a", "old")]);
    store.getState().applyBatch([subscribed("a", "start")]);
    store.getState().applyBatch([subscribed("a", "start")]);
    expect(view("a").held).toMatchObject([{ text: "old" }]);
  });
});

describe("approval ids reused by a new vendor process", () => {
  const withJournals = (journals: string[]): ToPhone => ({ v: 1, type: "subscribed", session: "a", from: "cursor", journals });
  const reused = stateFrame("a", { approvals: [{ id: 1, summary: { action: "again", files: [], scope: "once" } }] });
  const idsOf = () => store.getState().bySession.get("a")?.approvals.map((x) => x.id);

  beforeEach(() => {
    store.getState().applyBatch([
      sessions(info("a")),
      subscribed("a", "start"),
      ev("a", { type: "approval", data: { id: 1, detail: "{}", summary: { action: "first", files: [], scope: "once" } } }),
      ev("a", { type: "approval_closed", data: 1 }),
    ]);
  });

  it("stays closed across a resume that lists the same journals", () => {
    store.getState().applyBatch([withJournals(["j1"]), reused]);
    expect(idsOf()).toEqual([]);
  });

  it("shows again once the session's journal list changed", () => {
    store.getState().applyBatch([withJournals(["j1", "j2"]), reused]);
    expect(idsOf()).toEqual([1]);
  });
});

describe("the Task strip's open state", () => {
  it("is closed at first, kept for the run, and closed again by reset", () => {
    expect(store.getState().taskOpen).toBe(false);
    store.getState().setTaskOpen(true);
    expect(store.getState().taskOpen).toBe(true);
    store.getState().reset();
    expect(store.getState().taskOpen).toBe(false);
  });
});

describe("listed", () => {
  it("is false until a sessions frame is applied, even an empty one, and false again after reset", () => {
    expect(store.getState().listed).toBe(false);
    store.getState().applyBatch([user("a", "x")]);
    expect(store.getState().listed).toBe(false);
    store.getState().applyBatch([sessions()]);
    expect(store.getState().listed).toBe(true);
    store.getState().reset();
    expect(store.getState().listed).toBe(false);
  });

  it("has its own hook", async () => {
    const hooks = bindHooks(store);
    const listed = await renderHook(() => hooks.useListed());
    expect(listed.result.current).toBe(false);
    await act(() => {
      store.getState().applyBatch([sessions(info("a"))]);
    });
    expect(listed.result.current).toBe(true);
  });
});

describe("a session's front approval", () => {
  const summary = { action: "Run Bash", command: "ls", files: [], scope: "this call" };

  it("comes with the sessions row, through useFrontApproval", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval("a"));
    expect(front.result.current).toBeNull();
    await act(async () => store.getState().applyBatch([sessions(info("a", { approvals: 2, approval: { id: 4, summary } }))]));
    expect(front.result.current).toEqual({ id: 4, summary });
    await act(async () => store.getState().applyBatch([sessions(info("a"))]));
    expect(front.result.current).toBeNull();
    expect(bindHooks(store).useFrontApproval).toBeDefined();
  });

  it("is the same object while the row's approval does not change", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval("a"));
    const row = () => info("a", { approvals: 1, approval: { id: 4, summary: { ...summary, files: [...summary.files] } } });
    await act(async () => store.getState().applyBatch([sessions(row())]));
    const first = front.result.current;
    expect(first).not.toBeNull();
    const before = store.getState().sessions;
    await act(async () => store.getState().applyBatch([sessions(row())]));
    expect(front.result.current).toBe(first);
    expect(store.getState().sessions).toBe(before);
    await act(async () => store.getState().applyBatch([sessions({ ...row(), approval: { id: 5, summary } })]));
    expect(front.result.current).not.toBe(first);
    expect(front.result.current?.id).toBe(5);
  });

  it("is cleaned as a state approval is: control characters out, long text cut", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval("a"));
    const hostile = { action: "Run\u202E Bash\u0007", command: "ls\u202E", files: ["a\u0000b"], scope: "x".repeat(5000) };
    await act(async () => store.getState().applyBatch([sessions(info("a", { approvals: 1, approval: { id: 1, summary: hostile } }))]));
    store.getState().applyBatch([stateFrame("b", { approvals: [{ id: 1, summary: hostile }] })]);
    const fromState = view("b").approvals[0]?.summary;
    expect(front.result.current?.summary).toEqual(fromState);
    expect(front.result.current?.summary.action).not.toMatch(/[\u202E\u0007]/);
    expect(front.result.current?.summary.scope.length).toBeLessThan(5000);
  });

  it("is dropped when the row's approval is not one", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval("a"));
    const bad = [null, "x", { id: "1", summary }, { id: 1.5, summary }, { summary }];
    for (const approval of bad) {
      await act(async () => store.getState().applyBatch([sessions(info("a", { approval } as unknown as Partial<SessionInfo>))]));
      expect(front.result.current).toBeNull();
    }
    // A summary that is not an object still gives an approval, with an empty summary.
    await act(async () => store.getState().applyBatch([sessions(info("a", { approval: { id: 2, summary: "x" } } as unknown as Partial<SessionInfo>))]));
    expect(front.result.current).toEqual({ id: 2, summary: { action: "", files: [], scope: "" } });
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

  it("useCatalogs gives the live sessions' catalogs, the same array until one changes", async () => {
    const hooks = bindHooks(store);
    const h = await renderHook(() => hooks.useCatalogs());
    expect(h.result.current).toEqual([]);
    await act(async () => store.getState().applyBatch([sessions(info("a"), info("b")), catalogFrame("a")]));
    const first = h.result.current;
    expect(first.map((c) => c.modes)).toEqual([["ask"]]);
    await act(async () => store.getState().applyBatch([stateFrame("b", { now: "Reading" })])); // not a catalog
    expect(h.result.current).toBe(first);
    // A session that left the list takes its catalog with it.
    await act(async () => store.getState().applyBatch([sessions(info("b"))]));
    expect(h.result.current).toEqual([]);
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
  it("keep their view, marked ended, and are returned once", () => {
    store.getState().applyBatch([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    const b = view("b");
    const ended = store.getState().applyBatch([sessions(info("b"))]);
    expect(ended).toEqual(["a"]);
    expect(view("a").ended).toBe(true);
    expect(view("a").transcript.entries).toHaveLength(1);
    expect(view("b")).toBe(b);
    expect(store.getState().applyBatch([sessions(info("b"))])).toEqual([]);
  });

  it("can still bring its last events", () => {
    store.getState().applyBatch([sessions(info("a")), user("a", "x")]);
    store.getState().applyBatch([sessions()]);
    store.getState().applyBatch([text("a", "final words")]);
    expect(view("a").ended).toBe(true);
    expect(view("a").transcript.entries[1]).toMatchObject({ text: "final words" });
  });

  it("are live again when listed again", () => {
    store.getState().applyBatch([sessions(info("a")), user("a", "x")]);
    store.getState().applyBatch([sessions()]);
    store.getState().applyBatch([sessions(info("a"))]);
    expect(view("a").ended).toBe(false);
  });

  it("release() also takes a failed view of a session not listed, but not of a listed one", () => {
    store.getState().applyBatch([sessions(info("a"))]);
    store.getState().subscribeFailed("zzz", "No such session");
    store.getState().subscribeFailed("a", "Busy");
    expect(store.getState().release("zzz")).toBe(true);
    expect(store.getState().bySession.has("zzz")).toBe(false);
    expect(store.getState().release("a")).toBe(false);
    expect(store.getState().bySession.has("a")).toBe(true);
  });

  it("are released by release(), which only takes an ended view", () => {
    store.getState().applyBatch([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    store.getState().applyBatch([sessions(info("b"))]);
    expect(store.getState().release("b")).toBe(false);
    expect(store.getState().release("a")).toBe(true);
    expect(store.getState().bySession.has("a")).toBe(false);
    expect(store.getState().bySession.has("b")).toBe(true);
  });

  it("keep the view of a session never listed (events before the list)", () => {
    store.getState().applyBatch([user("early", "x")]);
    expect(store.getState().applyBatch([sessions(info("b"))])).toEqual([]);
    expect(view("early").ended).toBe(false);
  });
});

describe("ids the hub chooses", () => {
  it("cannot reach Object.prototype", () => {
    const ids = ["constructor", "__proto__", "toString", "hasOwnProperty"];
    for (const id of ids) store.getState().applyBatch([sessions(info(id)), user(id, `hi ${id}`)]);
    for (const id of ids) {
      expect(view(id).transcript.entries).toMatchObject([{ text: `hi ${id}` }]);
    }
    expect(store.getState().bySession.has("valueOf")).toBe(false);
    expect(({} as Record<string, unknown>).polluted).toBeUndefined();
    expect(Object.prototype.hasOwnProperty.call(Object.prototype, "entries")).toBe(false);
  });

  it("a hook for a session never seen is the empty view, even for constructor", async () => {
    const hooks = bindHooks(store);
    const h = await renderHook(() => hooks.useSession("constructor"));
    expect(h.result.current.transcript.entries).toEqual([]);
  });
});

describe("enums and numbers in the sessions list", () => {
  it("replaces a bad mode or phase, drops a bad engine, clamps approvals", () => {
    const bad = [
      info("a", { mode: "yolo" as never, phase: "flying" as never, approvals: -3 }),
      info("b", { engine: "gpt9" as never }),
      info("c", { approvals: 1e12 }),
      info("d", { approvals: 2.7 }),
    ];
    store.getState().applyBatch([sessions(...bad)]);
    const list = store.getState().sessions;
    expect(list.map((x) => x.id)).toEqual(["a", "c", "d"]);
    expect(list[0]).toMatchObject({ mode: "ask", phase: "idle", approvals: 0 });
    expect(list[1]?.approvals).toBeLessThanOrEqual(1_000_000);
    expect(list[2]?.approvals).toBe(2);
  });
});

describe("hub state and failures", () => {
  it("keeps the hub state", () => {
    expect(store.getState().hub).toBe("offline");
    store.getState().setHub("ready");
    expect(store.getState().hub).toBe("ready");
  });

  it("clears a failure when the session is listed again, and keeps one still unlisted or listed all along", () => {
    store.getState().applyBatch([sessions(info("b"))]);
    store.getState().subscribeFailed("a", "No such session");
    store.getState().subscribeFailed("b", "Not allowed");
    store.getState().applyBatch([sessions(info("b"), info("c"))]);
    expect(view("a").failed).toBe("No such session");
    expect(view("b").failed).toBe("Not allowed"); // listed all along: a list frame is not a new start
    store.getState().applyBatch([sessions(info("a"), info("b"))]);
    expect(view("a").failed).toBeNull();
    expect(view("b").failed).toBe("Not allowed");
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
    const off = connectStore(store, client, { schedule: clock.schedule });
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

  it("keeps the transcripts of the 4 sessions seen last; one seen before them is let go and replays from the start", () => {
    expect(MAX_VIEWS).toBe(4);
    const { sockets, clock, client, off } = rig();
    const ids = ["a", "b", "c", "d", "e"];
    sockets.last.receive({ type: "sessions", sessions: ids.map((id) => info(id)) });
    const visit = (id: string) => {
      watch(store, client, id);
      sockets.last.receive({ type: "subscribed", session: id, from: "start", journals: ["j1"] });
      sockets.last.receive({ type: "event", session: id, journal: "j1", seq: 1, event: { type: "user", data: `hi ${id}` } });
      sockets.last.receive(catalogFrame(id));
      clock.advance(50);
      unwatch(client, id);
      const req = sockets.last.commandsOf("unsubscribe").at(-1)?.req;
      sockets.last.receive({ type: "ack", req, message: "" });
      clock.advance(50);
    };
    ids.forEach(visit);
    const holding = () => ids.filter((id) => (store.getState().bySession.get(id)?.transcript.entries.length ?? 0) > 0);
    expect(holding()).toEqual(["b", "c", "d", "e"]);
    // What the other screens read of it stays: its catalog (for New), its last state.
    expect(view("a").catalog).not.toBeNull();
    const asked = (id: string) => sockets.last.commandsOf("subscribe").filter((c) => c.session === id).at(-1);
    // One still held resumes after its cursor; the one let go starts over.
    watch(store, client, "e");
    expect(asked("e")).toHaveProperty("after");
    watch(store, client, "a");
    expect(asked("a")).not.toHaveProperty("after");
    // Seeing it made "b" the oldest of five: let go.
    expect(holding()).toEqual(["c", "d", "e"]);
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

  it("a session refused for good and then listed again is streamed again while a screen holds it", () => {
    const { sockets, clock, client, off } = rig();
    // A deep link to a session the Mac has not started yet: refused, so the screen says why.
    watch(store, client, "a");
    const first = sockets.last.commandsOf("subscribe")[0]?.req;
    sockets.last.receive({ type: "refused", req: first, reason: "No such session", code: "invalid" });
    expect(view("a").failed).toBe("No such session");
    sockets.last.receive({ type: "sessions", sessions: [info("b")] });
    clock.advance(50);
    expect(view("a").failed).toBe("No such session");
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    // It starts: listed now, so the failure is over and the screen streams it.
    sockets.last.receive({ type: "sessions", sessions: [info("a"), info("b")] });
    clock.advance(50);
    expect(view("a").failed).toBeNull();
    expect(sockets.last.commandsOf("subscribe").map((c) => c.session)).toEqual(["a", "a"]);
    // A list that changes again does not ask a second time.
    sockets.last.receive({ type: "sessions", sessions: [info("a", { phase: "running" }), info("b")] });
    clock.advance(50);
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(2);
    off();
  });

  it("a session refused and listed again is not streamed when no screen holds it", () => {
    const { sockets, clock, client, off } = rig();
    watch(store, client, "a");
    const req = sockets.last.commandsOf("subscribe")[0]?.req;
    sockets.last.receive({ type: "refused", req, reason: "No such session", code: "invalid" });
    unwatch(client, "a"); // the user left its screen (an unlisted failed view is released then)
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    clock.advance(50);
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    off();
  });

  it("holds an ended session for ten minutes, then forgets it", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a"), info("b")] });
    watch(store, client, "a");
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "x" } });
    clock.advance(50);
    sockets.last.receive({ type: "sessions", sessions: [info("b")] });
    clock.advance(50);
    expect(view("a").ended).toBe(true);
    expect(forget).not.toHaveBeenCalled();
    clock.advance(10 * 60_000 - 100);
    expect(forget).not.toHaveBeenCalled();
    clock.advance(200);
    expect(forget).toHaveBeenCalledWith("a");
    expect(forget).not.toHaveBeenCalledWith("b");
    expect(store.getState().bySession.has("a")).toBe(false);
    off();
  });

  it("a session that leaves the list with no view is forgotten at once, with no timer", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a"), info("b")] });
    clock.advance(50);
    watch(store, client, "a"); // asked for, but nothing has come of it yet
    const timers = clock.pending;
    sockets.last.receive({ type: "sessions", sessions: [info("b")] });
    clock.advance(50);
    expect(store.getState().bySession.has("a")).toBe(false);
    expect(forget).toHaveBeenCalledWith("a");
    expect(forget).not.toHaveBeenCalledWith("b");
    expect(clock.pending).toBe(timers);
    off();
  });

  it("releaseSession forgets at once and cancels the timer", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    watch(store, client, "a");
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "x" } });
    clock.advance(50);
    sockets.last.receive({ type: "sessions", sessions: [] });
    clock.advance(50);
    const timers = clock.pending;
    releaseSession(store, client, "a");
    expect(forget).toHaveBeenCalledWith("a");
    expect(store.getState().bySession.has("a")).toBe(false);
    expect(clock.pending).toBe(timers - 1);
    off();
  });

  it("releaseSession leaves a live session alone", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    watch(store, client, "a");
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "x" } });
    clock.advance(50);
    releaseSession(store, client, "a");
    expect(forget).not.toHaveBeenCalled();
    expect(view("a")).toBeDefined();
    off();
  });

  it("a session listed again keeps its view and loses its timer", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    watch(store, client, "a");
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "x" } });
    clock.advance(50);
    sockets.last.receive({ type: "sessions", sessions: [] });
    clock.advance(50);
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    clock.advance(50);
    clock.advance(11 * 60_000);
    expect(forget).not.toHaveBeenCalled();
    expect(view("a").ended).toBe(false);
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

describe("drafts", () => {
  it("keeps a session's unsent first prompt until it is taken, once", () => {
    store.getState().setDraft("a", "hello");
    expect(store.getState().drafts.get("a")).toBe("hello");
    expect(store.getState().takeDraft("a")).toBe("hello");
    expect(store.getState().takeDraft("a")).toBe("");
    expect(store.getState().drafts.has("a")).toBe(false);
  });

  it("holds at most 16, dropping the oldest, and a new draft for a session replaces its old one", () => {
    for (let i = 0; i < 20; i++) store.getState().setDraft(`s${i}`, `t${i}`);
    expect(store.getState().drafts.size).toBe(16);
    expect(store.getState().drafts.has("s3")).toBe(false);
    expect(store.getState().drafts.has("s4")).toBe(true);
    store.getState().setDraft("s4", "again");
    store.getState().setDraft("new", "x");
    expect(store.getState().drafts.size).toBe(16);
    expect(store.getState().drafts.get("s4")).toBe("again");
    expect(store.getState().drafts.has("s5")).toBe(false); // s4 was refreshed, so s5 was the oldest
  });

  it("is cleared by reset", () => {
    store.getState().setDraft("a", "hello");
    store.getState().reset();
    expect(store.getState().drafts.size).toBe(0);
  });
});

describe("the texts sent from this phone, for ↑", () => {
  it("keeps each session's, newest last, at most 20, and skips a repeat of the last", () => {
    for (let i = 0; i < 25; i++) store.getState().remember("a", `t${i}`);
    const a = store.getState().sent.get("a") ?? [];
    expect(a).toHaveLength(20);
    expect(a[0]).toBe("t5");
    expect(a[19]).toBe("t24");
    store.getState().remember("a", "t24");
    expect(store.getState().sent.get("a")).toHaveLength(20);
    expect(store.getState().sent.get("b")).toBeUndefined();
  });

  it("keeps at most 16 sessions', dropping the one sent to longest ago", () => {
    for (let i = 0; i < 20; i++) store.getState().remember(`s${i}`, "x");
    expect(store.getState().sent.size).toBe(16);
    expect(store.getState().sent.has("s3")).toBe(false);
    store.getState().remember("s4", "again");
    store.getState().remember("new", "x");
    expect(store.getState().sent.has("s4")).toBe(true);
    expect(store.getState().sent.has("s5")).toBe(false);
  });

  it("is cleared by reset: an unpaired phone keeps none of them", () => {
    store.getState().remember("a", "hello");
    store.getState().reset();
    expect(store.getState().sent.size).toBe(0);
  });
});
