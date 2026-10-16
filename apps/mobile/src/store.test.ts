import { act, renderHook } from "@testing-library/react-native";

import { FakeClock, Sockets } from "./client/fakes";
import { HubClient } from "./client/HubClient";
import type { CatalogInfo, JournalEvent, SessionInfo, StateInfo, ToPhone } from "./protocol";
import { entrySize } from "./model/transcript";
import type { BatchItem } from "./client/HubClient";
import { linksOf, type Links } from "./macs";
import { MAX_VIEWS, bestHub, bindHooks, connectStore, createOctetStore, releaseSession, unwatch, watch, type OctetStore } from "./store";
import { M, k, keyed } from "./testKeys";

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
/** Mac M's frames, applied as its feed applies them. */
const applyM = (frames: readonly BatchItem[]): string[] => store.getState().applyBatch(M, keyed(frames));
/** The links of the client a `rig` made. */
let links: Links = linksOf(new Map());
beforeEach(() => {
  seqs = new Map();
  store = createOctetStore();
});
const view = (id: string) => {
  const v = store.getState().bySession.get(k(id));
  if (!v) throw new Error(`no view for ${id}`);
  return v;
};

describe("reset", () => {
  it("forgets the sessions, their transcripts and the connection state, and keeps working", () => {
    applyM([sessions(info("a")), subscribed("a", "start"), text("a", "hello")]);
    store.getState().setHub(M, "ready");
    store.getState().reset();
    expect(store.getState().sessions).toEqual([]);
    expect(store.getState().bySession.size).toBe(0);
    expect(bestHub(store.getState())).toBe("offline");
    applyM([sessions(info("b"))]);
    expect(store.getState().sessions.map((s) => s.id)).toEqual([k("b")]);
  });
});

describe("a question answered from the phone", () => {
  const question = (id: string, extra: Partial<SessionInfo> = {}) => info(id, { question: true, ...extra });

  it("is cleared from the row at once, and the row is a new object", () => {
    applyM([sessions(question("a"), question("b"))]);
    const before = store.getState().sessions;
    store.getState().markAnswered(k("a"));
    const after = store.getState().sessions;
    expect(after[0]?.question).toBeUndefined();
    expect(after[1]).toBe(before[1]);
  });

  it("stays cleared through rows that still say question (the Mac has not caught up), but not through a new question", () => {
    applyM([sessions(question("a"))]);
    store.getState().markAnswered(k("a"));
    applyM([sessions(question("a", { last_activity: 5 }))]);
    expect(store.getState().sessions[0]?.question).toBeUndefined();
    expect(store.getState().sessions[0]?.last_activity).toBe(5);
    // The Mac caught up (its row has no question); the next question is a new one.
    applyM([sessions(info("a", { last_activity: 6 }))]);
    applyM([sessions(question("a", { last_activity: 9 }))]);
    expect(store.getState().sessions[0]?.question).toBe(true);
  });

  it("keeps the row's identity while a stale question row repeats", () => {
    applyM([sessions(question("a"))]);
    store.getState().markAnswered(k("a"));
    const row = store.getState().sessions;
    applyM([sessions(question("a"))]);
    expect(store.getState().sessions).toBe(row);
  });

  it("marks nothing for a session that is not asking: a later question must still show", () => {
    applyM([sessions(info("a"))]);
    store.getState().markAnswered(k("a"));
    store.getState().markAnswered(k("missing"));
    applyM([sessions(question("a"))]);
    expect(store.getState().sessions[0]?.question).toBe(true);
  });

  it("forgets the mark when the link to the Mac is ready again: the fresh list is the truth", () => {
    store.getState().setHub(M, "ready");
    applyM([sessions(question("a"))]);
    store.getState().markAnswered(k("a"));
    store.getState().setHub(M, "connecting");
    // Still marked while the link is down.
    expect(store.getState().answered.has(k("a"))).toBe(true);
    store.getState().setHub(M, "ready");
    expect(store.getState().answered.size).toBe(0);
    applyM([sessions(question("a"))]);
    expect(store.getState().sessions[0]?.question).toBe(true);
    // A ready that was ready already forgets nothing.
    store.getState().markAnswered(k("a"));
    store.getState().setHub(M, "ready");
    expect(store.getState().answered.has(k("a"))).toBe(true);
  });

  it("forgets the mark when the session leaves the list, and on reset", () => {
    applyM([sessions(question("a"))]);
    store.getState().markAnswered(k("a"));
    applyM([sessions(info("b"))]);
    applyM([sessions(question("a"))]);
    expect(store.getState().sessions.find((s) => s.id === k("a"))?.question).toBe(true);
    store.getState().markAnswered(k("a"));
    store.getState().reset();
    applyM([sessions(question("a"))]);
    expect(store.getState().sessions[0]?.question).toBe(true);
  });
});

describe("asking to reply", () => {
  it("is one session at a time, cleared by null and by reset", () => {
    expect(store.getState().replyTo).toBeNull();
    store.getState().setReplyTo("a");
    expect(store.getState().replyTo).toBe("a");
    store.getState().setReplyTo("b");
    expect(store.getState().replyTo).toBe("b");
    store.getState().setReplyTo(null);
    expect(store.getState().replyTo).toBeNull();
    store.getState().setReplyTo("a");
    store.getState().reset();
    expect(store.getState().replyTo).toBeNull();
  });
});

describe("the sessions list", () => {
  it("is updated by a sessions frame", () => {
    applyM([sessions(info("a"), info("b", { goal: "ship it" }))]);
    expect(store.getState().sessions.map((s) => s.id)).toEqual([k("a"), k("b")]);
    expect(store.getState().sessions[1]?.goal).toBe("ship it");
  });

  it("keeps identity for sessions that did not change", () => {
    applyM([sessions(info("a"), info("b"))]);
    const before = store.getState().sessions;
    applyM([sessions(info("a"), info("b"))]);
    expect(store.getState().sessions).toBe(before);
    applyM([sessions(info("a"), info("b", { phase: "running" }))]);
    const after = store.getState().sessions;
    expect(after).not.toBe(before);
    expect(after[0]).toBe(before[0]);
    expect(after[1]).not.toBe(before[1]);
  });

  it("keeps a row's usage as clean text, and its question only when it is true", () => {
    const raw = [
      info("a", { usage: "5h \u001b[1m12%\u0007 · week 3%", question: true }),
      info("b", { usage: "é".repeat(5_000) }),
      { ...info("c"), usage: 7, question: "yes" },
      info("d", { question: false }),
    ] as unknown as SessionInfo[];
    applyM([sessions(...raw)]);
    const [a, b, c, d] = store.getState().sessions;
    expect(a).toMatchObject({ usage: "5h 12% · week 3%", question: true });
    expect(b?.usage).toBe("é".repeat(200));
    expect(c?.usage).toBeNull();
    expect(c && "question" in c).toBe(false);
    expect(d && "question" in d).toBe(false);
    // The same usage and question keep the row; a change makes a new one.
    const before = store.getState().sessions;
    applyM([sessions(...raw)]);
    expect(store.getState().sessions).toBe(before);
    applyM([sessions(info("a", { usage: "5h 13% · week 3%", question: true }), ...raw.slice(1))]);
    expect(store.getState().sessions[0]).not.toBe(before[0]);
    expect(store.getState().sessions[1]).toBe(before[1]);
    applyM([sessions(info("a", { usage: "5h 13% · week 3%" }), ...raw.slice(1))]);
    expect(store.getState().sessions[0]?.question).toBeUndefined();
  });

  it("cleans the strings and skips entries without an id", () => {
    const junk = [info("a", { now: "Running \u001b[1mls\u0007", workspace: "/w".repeat(100_000) }), { engine: "claude" }, null, 5] as unknown as SessionInfo[];
    applyM([sessions(...junk)]);
    const [only, ...rest] = store.getState().sessions;
    expect(rest).toEqual([]);
    expect(only?.now).toBe("Running ls");
    expect((only?.workspace ?? "").length).toBeLessThan(10_000);
  });
});

describe("events", () => {
  it("an event for session A does not change session B", () => {
    applyM([sessions(info("a"), info("b")), user("a", "hi"), user("b", "yo")]);
    const b = view("b");
    const bTranscript = b.transcript;
    const before = store.getState();
    applyM([text("a", "hello")]);
    expect(view("b")).toBe(b);
    expect(view("b").transcript).toBe(bTranscript);
    expect(store.getState().sessions).toBe(before.sessions);
    expect(view("a").transcript.entries).toHaveLength(2);
    // And inside A, the user entry kept its identity.
    expect(view("a").transcript.entries[0]).toMatchObject({ kind: "user", text: "hi" });
  });

  it("the untouched entries of a session keep their identity", () => {
    applyM([user("a", "hi"), text("a", "he")]);
    const first = view("a").transcript.entries[0];
    applyM([text("a", "llo")]);
    expect(view("a").transcript.entries[0]).toBe(first);
    expect(view("a").transcript.entries[1]).toMatchObject({ text: "hello" });
  });

  it("a whole batch is one commit", () => {
    const calls: number[] = [];
    store.subscribe(() => calls.push(1));
    const frames: ToPhone[] = [sessions(info("a"), info("b"))];
    for (let i = 0; i < 100; i++) frames.push(text(i % 2 ? "a" : "b", "w "));
    applyM(frames);
    expect(calls).toHaveLength(1);
    expect(view("a").transcript.entries[0]).toMatchObject({ text: "w ".repeat(50) });
  });

  it("an empty or all-ignored batch commits nothing", () => {
    applyM([user("a", "hi")]);
    const calls: number[] = [];
    store.subscribe(() => calls.push(1));
    const ignored: ToPhone[] = [
      subscribed("a", "cursor"),
      ev("a", { type: "ready", data: "id" }),
      ev("a", { type: "models", data: [] }),
      { v: 1, type: "event", session: "a", journal: "j1", seq: 99, event: { type: "from_the_future", data: 1 } },
      { v: 1, type: "ack", req: 1, message: "ok" },
    ];
    applyM(ignored);
    applyM([]);
    expect(calls).toEqual([]);
  });

  it("keeps the mode, model and usage records", () => {
    applyM([ev("a", { type: "mode", data: "auto" }), ev("a", { type: "usage", data: "1k" })]);
    expect(view("a")).toMatchObject({ mode: "auto", usage: "1k" });
  });

  it("survives garbage event data", () => {
    const junk = ev("a", { type: "tool", data: 5 } as unknown as JournalEvent);
    expect(() => applyM([junk, ev("a", { type: "text", data: null } as unknown as JournalEvent)])).not.toThrow();
  });

  it("bounds the transcript at 1,000 entries even when one batch brings 3,000", () => {
    const frames: ToPhone[] = [];
    for (let i = 0; i < 3_000; i++) frames.push(user("a", `m${i}`));
    applyM(frames);
    const t = view("a").transcript;
    expect(t.entries).toHaveLength(1_000);
    expect(t.bytes).toBe(t.entries.reduce((n, e) => n + entrySize(e), 0));
  });
});

describe("a_reset_replaces_the_transcript", () => {
  it("replaces it when subscribed says from start, in order within the batch", () => {
    applyM([user("a", "old one"), text("a", "old two"), ev("a", { type: "approval", data: { id: 1, detail: "{}", summary: { action: "x", files: [], scope: "s" } } })]);
    expect(view("a").transcript.entries).toHaveLength(2);
    expect(view("a").approvals).toHaveLength(1);
    applyM([text("a", "stale live"), subscribed("a", "start"), user("a", "new one"), text("a", "new two")]);
    expect(view("a").transcript.entries).toMatchObject([{ kind: "user", text: "new one" }, { kind: "reply", text: "new two" }]);
    expect(view("a").approvals).toEqual([]);
  });

  it("leaves other sessions alone", () => {
    applyM([user("a", "x"), user("b", "y")]);
    const b = view("b");
    applyM([subscribed("a", "start")]);
    expect(view("a").transcript.entries).toEqual([]);
    expect(view("b")).toBe(b);
  });

  it("a replay that arrives across batches ends in the same transcript", () => {
    applyM([user("a", "old")]);
    applyM([subscribed("a", "start")]);
    applyM([user("a", "one"), text("a", "two")]);
    applyM([text("a", " three")]);
    expect(view("a").transcript.entries).toMatchObject([{ text: "one" }, { text: "two three" }]);
  });
});

describe("a fresh replay keeps the previous transcript on screen", () => {
  it("until the replay's first page commits, across batches", async () => {
    const hooks = bindHooks(store);
    applyM([user("a", "old"), text("a", "reply")]);
    const before = view("a").transcript.entries;
    const shown = await renderHook(() => hooks.useEntries(k("a")));
    expect(shown.result.current).toBe(before);
    await act(() => {
      applyM([subscribed("a", "start")]);
    });
    expect(view("a").replaying).toBe(true);
    expect(view("a").transcript.entries).toEqual([]);
    expect(shown.result.current).toBe(before); // no empty flash
    await act(() => {
      applyM([user("a", "new")]);
    });
    expect(view("a").replaying).toBe(false);
    expect(view("a").held).toBeNull();
    expect(shown.result.current).toMatchObject([{ kind: "user", text: "new" }]);
  });

  it("holds what the batch had before the reset, stale live records included", () => {
    applyM([user("a", "old")]);
    applyM([text("a", "late"), subscribed("a", "start")]);
    expect(view("a").held).toMatchObject([{ text: "old" }, { text: "late" }]);
  });

  it("holds nothing when the reset and the first page come in one batch", () => {
    applyM([user("a", "old")]);
    applyM([subscribed("a", "start"), user("a", "new")]);
    expect(view("a").replaying).toBe(false);
    expect(view("a").held).toBeNull();
    expect(view("a").transcript.entries).toMatchObject([{ text: "new" }]);
  });

  it("an empty replay ends the hold when the client says the replay is over", async () => {
    const hooks = bindHooks(store);
    applyM([user("a", "old")]);
    const shown = await renderHook(() => hooks.useEntries(k("a")));
    await act(() => {
      applyM([subscribed("a", "start")]);
    });
    expect(shown.result.current).toMatchObject([{ text: "old" }]);
    await act(() => {
      applyM([{ type: "replayed", session: "a" }]);
    });
    expect(view("a").replaying).toBe(false);
    expect(view("a").held).toBeNull();
    expect(shown.result.current).toEqual([]);
  });

  it("a replayed mark for a session the store holds nothing of makes no view", () => {
    applyM([{ type: "replayed", session: "nobody" }]);
    expect(store.getState().bySession.has(k("nobody"))).toBe(false);
  });

  it("each reset moves the view's epoch on", () => {
    applyM([user("a", "old")]);
    const before = view("a").epoch;
    applyM([subscribed("a", "start"), user("a", "new")]);
    expect(view("a").epoch).toBe(before + 1);
    applyM([user("a", "more")]);
    expect(view("a").epoch).toBe(before + 1);
  });

  it("a second reset during a replay keeps the transcript first held", () => {
    applyM([user("a", "old")]);
    applyM([subscribed("a", "start")]);
    applyM([subscribed("a", "start")]);
    expect(view("a").held).toMatchObject([{ text: "old" }]);
  });
});

describe("approval ids reused by a new vendor process", () => {
  const withJournals = (journals: string[]): ToPhone => ({ v: 1, type: "subscribed", session: "a", from: "cursor", journals });
  const reused = stateFrame("a", { approvals: [{ id: 1, summary: { action: "again", files: [], scope: "once" } }] });
  const idsOf = () => store.getState().bySession.get(k("a"))?.approvals.map((x) => x.id);

  beforeEach(() => {
    applyM([
      sessions(info("a")),
      subscribed("a", "start"),
      ev("a", { type: "approval", data: { id: 1, detail: "{}", summary: { action: "first", files: [], scope: "once" } } }),
      ev("a", { type: "approval_closed", data: 1 }),
    ]);
  });

  it("stays closed across a resume that lists the same journals", () => {
    applyM([withJournals(["j1"]), reused]);
    expect(idsOf()).toEqual([]);
  });

  it("shows again once the session's journal list changed", () => {
    applyM([withJournals(["j1", "j2"]), reused]);
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
    expect(store.getState().listed.has(M)).toBe(false);
    applyM([user("a", "x")]);
    expect(store.getState().listed.has(M)).toBe(false);
    applyM([sessions()]);
    expect(store.getState().listed.has(M)).toBe(true);
    store.getState().reset();
    expect(store.getState().listed.has(M)).toBe(false);
  });

  it("has its own hook", async () => {
    const hooks = bindHooks(store);
    const listed = await renderHook(() => hooks.useListed());
    expect(listed.result.current).toBe(false);
    await act(() => {
      applyM([sessions(info("a"))]);
    });
    expect(listed.result.current).toBe(true);
  });
});

describe("a session's front approval", () => {
  const summary = { action: "Run Bash", command: "ls", files: [], scope: "this call" };

  it("comes with the sessions row, through useFrontApproval", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval(k("a")));
    expect(front.result.current).toBeNull();
    await act(async () => applyM([sessions(info("a", { approvals: 2, approval: { id: 4, summary } }))]));
    expect(front.result.current).toEqual({ id: 4, summary });
    await act(async () => applyM([sessions(info("a"))]));
    expect(front.result.current).toBeNull();
    expect(bindHooks(store).useFrontApproval).toBeDefined();
  });

  it("is the same object while the row's approval does not change", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval(k("a")));
    const row = () => info("a", { approvals: 1, approval: { id: 4, summary: { ...summary, files: [...summary.files] } } });
    await act(async () => applyM([sessions(row())]));
    const first = front.result.current;
    expect(first).not.toBeNull();
    const before = store.getState().sessions;
    await act(async () => applyM([sessions(row())]));
    expect(front.result.current).toBe(first);
    expect(store.getState().sessions).toBe(before);
    await act(async () => applyM([sessions({ ...row(), approval: { id: 5, summary } })]));
    expect(front.result.current).not.toBe(first);
    expect(front.result.current?.id).toBe(5);
  });

  it("is cleaned as a state approval is: control characters out, long text cut", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval(k("a")));
    const hostile = { action: "Run\u202E Bash\u0007", command: "ls\u202E", files: ["a\u0000b"], scope: "x".repeat(5000) };
    await act(async () => applyM([sessions(info("a", { approvals: 1, approval: { id: 1, summary: hostile } }))]));
    applyM([stateFrame("b", { approvals: [{ id: 1, summary: hostile }] })]);
    const fromState = view("b").approvals[0]?.summary;
    expect(front.result.current?.summary).toEqual(fromState);
    expect(front.result.current?.summary.action).not.toMatch(/[\u202E\u0007]/);
    expect(front.result.current?.summary.scope.length).toBeLessThan(5000);
  });

  it("is dropped when the row's approval is not one", async () => {
    const hooks = bindHooks(store);
    const front = await renderHook(() => hooks.useFrontApproval(k("a")));
    const bad = [null, "x", { id: "1", summary }, { id: 1.5, summary }, { summary }];
    for (const approval of bad) {
      await act(async () => applyM([sessions(info("a", { approval } as unknown as Partial<SessionInfo>))]));
      expect(front.result.current).toBeNull();
    }
    // A summary that is not an object still gives an approval, with an empty summary.
    await act(async () => applyM([sessions(info("a", { approval: { id: 2, summary: "x" } } as unknown as Partial<SessionInfo>))]));
    expect(front.result.current).toEqual({ id: 2, summary: { action: "", files: [], scope: "" } });
  });
});

describe("state and catalog", () => {
  it("are stored per session", () => {
    applyM([stateFrame("a", { now: "Reading", queue: ["next"], queued: 1 }), stateFrame("b", { now: "Idle" }), catalogFrame("a")]);
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
    await act(async () => applyM([sessions(info("a"), info("b")), catalogFrame("a")]));
    const first = h.result.current;
    expect(first.map((c) => c.modes)).toEqual([["ask"]]);
    await act(async () => applyM([stateFrame("b", { now: "Reading" })])); // not a catalog
    expect(h.result.current).toBe(first);
    // A session that left the list takes its catalog with it.
    await act(async () => applyM([sessions(info("b"))]));
    expect(h.result.current).toEqual([]);
  });

  it("take the state's approvals as the pending ones", () => {
    applyM([stateFrame("a", { approvals: [{ id: 3, summary: { action: "Edit", files: ["a.rs"], scope: "once" } }] })]);
    expect(view("a").approvals.map((a) => a.id)).toEqual([3]);
    applyM([ev("a", { type: "approval_closed", data: 3 })]);
    expect(view("a").approvals).toEqual([]);
  });

  it("an identical state frame changes nothing", () => {
    applyM([stateFrame("a", { now: "x" })]);
    const before = store.getState();
    applyM([stateFrame("a", { now: "x" })]);
    expect(store.getState()).toBe(before);
  });
});

describe("sessions that end", () => {
  it("keep their view, marked ended, and are returned once", () => {
    applyM([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    const b = view("b");
    const ended = applyM([sessions(info("b"))]);
    expect(ended).toEqual([k("a")]);
    expect(view("a").ended).toBe(true);
    expect(view("a").transcript.entries).toHaveLength(1);
    expect(view("b")).toBe(b);
    expect(applyM([sessions(info("b"))])).toEqual([]);
  });

  it("can still bring its last events", () => {
    applyM([sessions(info("a")), user("a", "x")]);
    applyM([sessions()]);
    applyM([text("a", "final words")]);
    expect(view("a").ended).toBe(true);
    expect(view("a").transcript.entries[1]).toMatchObject({ text: "final words" });
  });

  it("are live again when listed again", () => {
    applyM([sessions(info("a")), user("a", "x")]);
    applyM([sessions()]);
    applyM([sessions(info("a"))]);
    expect(view("a").ended).toBe(false);
  });

  it("release() also takes a failed view of a session not listed, but not of a listed one", () => {
    applyM([sessions(info("a"))]);
    store.getState().subscribeFailed(k("zzz"), "No such session");
    store.getState().subscribeFailed(k("a"), "Busy");
    expect(store.getState().release(k("zzz"))).toBe(true);
    expect(store.getState().bySession.has(k("zzz"))).toBe(false);
    expect(store.getState().release(k("a"))).toBe(false);
    expect(store.getState().bySession.has(k("a"))).toBe(true);
  });

  it("are released by release(), which only takes an ended view", () => {
    applyM([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    applyM([sessions(info("b"))]);
    expect(store.getState().release(k("b"))).toBe(false);
    expect(store.getState().release(k("a"))).toBe(true);
    expect(store.getState().bySession.has(k("a"))).toBe(false);
    expect(store.getState().bySession.has(k("b"))).toBe(true);
  });

  it("keep the view of a session never listed (events before the list)", () => {
    applyM([user("early", "x")]);
    expect(applyM([sessions(info("b"))])).toEqual([]);
    expect(view("early").ended).toBe(false);
  });
});

describe("ids the hub chooses", () => {
  it("cannot reach Object.prototype", () => {
    const ids = ["constructor", "__proto__", "toString", "hasOwnProperty"];
    for (const id of ids) applyM([sessions(info(id)), user(id, `hi ${id}`)]);
    for (const id of ids) {
      expect(view(id).transcript.entries).toMatchObject([{ text: `hi ${id}` }]);
    }
    expect(store.getState().bySession.has(k("valueOf"))).toBe(false);
    expect(({} as Record<string, unknown>).polluted).toBeUndefined();
    expect(Object.prototype.hasOwnProperty.call(Object.prototype, "entries")).toBe(false);
  });

  it("a hook for a session never seen is the empty view, even for constructor", async () => {
    const hooks = bindHooks(store);
    const h = await renderHook(() => hooks.useSession(k("constructor")));
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
    applyM([sessions(...bad)]);
    const list = store.getState().sessions;
    expect(list.map((x) => x.id)).toEqual([k("a"), k("c"), k("d")]);
    expect(list[0]).toMatchObject({ mode: "ask", phase: "idle", approvals: 0 });
    expect(list[1]?.approvals).toBeLessThanOrEqual(1_000_000);
    expect(list[2]?.approvals).toBe(2);
  });
});

describe("hub state and failures", () => {
  it("keeps the hub state", () => {
    expect(bestHub(store.getState())).toBe("offline");
    store.getState().setHub(M, "ready");
    expect(bestHub(store.getState())).toBe("ready");
  });

  it("clears a failure when the session is listed again, and keeps one still unlisted or listed all along", () => {
    applyM([sessions(info("b"))]);
    store.getState().subscribeFailed(k("a"), "No such session");
    store.getState().subscribeFailed(k("b"), "Not allowed");
    applyM([sessions(info("b"), info("c"))]);
    expect(view("a").failed).toBe("No such session");
    expect(view("b").failed).toBe("Not allowed"); // listed all along: a list frame is not a new start
    applyM([sessions(info("a"), info("b"))]);
    expect(view("a").failed).toBeNull();
    expect(view("b").failed).toBe("Not allowed");
  });

  it("marks a session failed", () => {
    applyM([user("a", "x")]);
    store.getState().subscribeFailed(k("a"), "Not allowed");
    expect(view("a").failed).toBe("Not allowed");
    expect(view("a").transcript.entries).toHaveLength(1);
  });
});

describe("with a client", () => {
  function rig() {
    const sockets = new Sockets();
    const clock = new FakeClock();
    const client = new HubClient({ url: "wss://mac/", token: "t", factory: sockets.factory, now: clock.now, schedule: clock.schedule, random: () => 1 });
    links = linksOf(new Map([[M, client]]));
    const off = connectStore(store, client, { mac: M, links, now: clock.now, schedule: clock.schedule });
    client.start();
    sockets.last.open();
    sockets.last.receive({ type: "welcome", device: "iPhone" });
    clock.advance(50);
    return { sockets, clock, client, off };
  }

  it("follows the client's state", () => {
    const { client, off } = rig();
    expect(bestHub(store.getState())).toBe("ready");
    expect(client.state).toBe("ready");
    off();
  });

  it("delivers a client batch as one commit, and a reset in its place", () => {
    const { sockets, clock, off } = rig();
    watch(store, links, k("a"));
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
    const { sockets, clock, off } = rig();
    const ids = ["a", "b", "c", "d", "e"];
    sockets.last.receive({ type: "sessions", sessions: ids.map((id) => info(id)) });
    const visit = (id: string) => {
      watch(store, links, k(id));
      sockets.last.receive({ type: "subscribed", session: id, from: "start", journals: ["j1"] });
      sockets.last.receive({ type: "event", session: id, journal: "j1", seq: 1, event: { type: "user", data: `hi ${id}` } });
      sockets.last.receive(catalogFrame(id));
      clock.advance(50);
      unwatch(links, k(id));
      const req = sockets.last.commandsOf("unsubscribe").at(-1)?.req;
      sockets.last.receive({ type: "ack", req, message: "" });
      clock.advance(50);
    };
    ids.forEach(visit);
    const holding = () => ids.filter((id) => (store.getState().bySession.get(k(id))?.transcript.entries.length ?? 0) > 0);
    expect(holding()).toEqual(["b", "c", "d", "e"]);
    // What the other screens read of it stays: its catalog (for New), its last state.
    expect(view("a").catalog).not.toBeNull();
    const asked = (id: string) => sockets.last.commandsOf("subscribe").filter((c) => c.session === id).at(-1);
    // One still held resumes after its cursor; the one let go starts over.
    watch(store, links, k("e"));
    expect(asked("e")).toHaveProperty("after");
    watch(store, links, k("a"));
    expect(asked("a")).not.toHaveProperty("after");
    // Seeing it made "b" the oldest of five: let go.
    expect(holding()).toEqual(["c", "d", "e"]);
    off();
  });

  it("a non-retryable subscribe refusal forgets the session and marks its view failed", () => {
    const { sockets, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    watch(store, links, k("a"));
    const req = sockets.last.commandsOf("subscribe")[0]?.req;
    sockets.last.receive({ type: "refused", req, reason: "That session is not allowed", code: "not_allowed" });
    expect(forget).toHaveBeenCalledWith("a");
    expect(view("a").failed).toBe("That session is not allowed");
    off();
  });

  it("a session refused for good and then listed again is streamed again while a screen holds it", () => {
    const { sockets, clock, off } = rig();
    // A deep link to a session the Mac has not started yet: refused, so the screen says why.
    watch(store, links, k("a"));
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

  it("the re-stream of a re-listed session adds no screen hold: one unwatch ends it and it is no longer shown", () => {
    const { sockets, clock, off } = rig();
    watch(store, links, k("a"));
    const first = sockets.last.commandsOf("subscribe")[0]?.req;
    sockets.last.receive({ type: "refused", req: first, reason: "No such session", code: "invalid" });
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    clock.advance(50);
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(2);
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "hi a" } });
    clock.advance(50);
    unwatch(links, k("a"));
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(1);
    // Not shown any more: four sessions seen after it push its transcript out.
    const others = ["b", "c", "d", "e"];
    sockets.last.receive({ type: "sessions", sessions: ["a", ...others].map((id) => info(id)) });
    for (const id of others) {
      watch(store, links, k(id));
      sockets.last.receive({ type: "subscribed", session: id, from: "start", journals: ["j1"] });
      sockets.last.receive({ type: "event", session: id, journal: "j1", seq: 1, event: { type: "user", data: `hi ${id}` } });
      clock.advance(50);
    }
    expect(view("a").transcript.entries).toHaveLength(0);
    off();
  });

  it("a session refused and listed again is not streamed when no screen holds it", () => {
    const { sockets, clock, off } = rig();
    watch(store, links, k("a"));
    const req = sockets.last.commandsOf("subscribe")[0]?.req;
    sockets.last.receive({ type: "refused", req, reason: "No such session", code: "invalid" });
    unwatch(links, k("a")); // the user left its screen (an unlisted failed view is released then)
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    clock.advance(50);
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    off();
  });

  it("holds an ended session for ten minutes, then forgets it", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a"), info("b")] });
    watch(store, links, k("a"));
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
    expect(store.getState().bySession.has(k("a"))).toBe(false);
    off();
  });

  it("a session that leaves the list with no view is forgotten at once, with no timer", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a"), info("b")] });
    clock.advance(50);
    watch(store, links, k("a")); // asked for, but nothing has come of it yet
    const timers = clock.pending;
    sockets.last.receive({ type: "sessions", sessions: [info("b")] });
    clock.advance(50);
    expect(store.getState().bySession.has(k("a"))).toBe(false);
    expect(forget).toHaveBeenCalledWith("a");
    expect(forget).not.toHaveBeenCalledWith("b");
    expect(clock.pending).toBe(timers);
    off();
  });

  it("releaseSession forgets at once and cancels the timer", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    watch(store, links, k("a"));
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "x" } });
    clock.advance(50);
    sockets.last.receive({ type: "sessions", sessions: [] });
    clock.advance(50);
    const timers = clock.pending;
    releaseSession(store, links, k("a"));
    expect(forget).toHaveBeenCalledWith("a");
    expect(store.getState().bySession.has(k("a"))).toBe(false);
    expect(clock.pending).toBe(timers - 1);
    off();
  });

  it("releaseSession leaves a live session alone", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    watch(store, links, k("a"));
    sockets.last.receive({ type: "subscribed", session: "a", from: "start", journals: ["j1"] });
    sockets.last.receive({ type: "event", session: "a", journal: "j1", seq: 1, event: { type: "user", data: "x" } });
    clock.advance(50);
    releaseSession(store, links, k("a"));
    expect(forget).not.toHaveBeenCalled();
    expect(view("a")).toBeDefined();
    off();
  });

  it("a session listed again keeps its view and loses its timer", () => {
    const { sockets, clock, client, off } = rig();
    const forget = jest.spyOn(client, "forget");
    sockets.last.receive({ type: "sessions", sessions: [info("a")] });
    watch(store, links, k("a"));
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
    watch(store, links, k("a"));
    expect(subscribe).toHaveBeenLastCalledWith("a", true);
    applyM([user("a", "x")]);
    watch(store, links, k("a"));
    expect(subscribe).toHaveBeenLastCalledWith("a", false);
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    off();
  });

  it("watch asked twice before any event does not resubscribe", () => {
    const { sockets, off } = rig();
    watch(store, links, k("a"));
    watch(store, links, k("a"));
    expect(sockets.last.commandsOf("subscribe")).toHaveLength(1);
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(0);
    off();
  });

  it("unwatch unsubscribes and keeps the transcript for a resume", () => {
    const { sockets, client, off } = rig();
    watch(store, links, k("a"));
    applyM([user("a", "x")]);
    unwatch(links, k("a"));
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(1);
    expect(view("a").transcript.entries).toHaveLength(1);
    const subscribe = jest.spyOn(client, "subscribe");
    watch(store, links, k("a"));
    expect(subscribe).toHaveBeenLastCalledWith("a", false);
    off();
  });

  // Usage opens over the Conversation: both screens watch the one session.
  it("two screens watching one session: the first to leave does not unsubscribe it", () => {
    const { sockets, off } = rig();
    watch(store, links, k("a"));
    watch(store, links, k("a")); // Usage over the Conversation
    unwatch(links, k("a"));
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(0);
    unwatch(links, k("a"));
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(1);
    off();
  });

  it("the count never goes below none: extra unwatches do not cancel a later watch", () => {
    const { sockets, off } = rig();
    watch(store, links, k("a"));
    unwatch(links, k("a"));
    unwatch(links, k("a")); // one too many
    watch(store, links, k("a"));
    watch(store, links, k("a"));
    unwatch(links, k("a")); // one screen is still there
    expect(sockets.last.commandsOf("unsubscribe")).toHaveLength(1);
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
    applyM([sessions(info("a"), info("b")), user("a", "x"), user("b", "y")]);
    let rendersA = 0;
    let rendersB = 0;
    const a = await renderHook(() => {
      rendersA++;
      return hooks.useSession(k("a"));
    });
    const b = await renderHook(() => {
      rendersB++;
      return hooks.useSession(k("b"));
    });
    const [baseA, baseB] = [rendersA, rendersB];
    await act(() => {
      applyM([text("a", "hello")]);
    });
    expect(rendersA).toBeGreaterThan(baseA);
    expect(rendersB).toBe(baseB);
    expect(a.result.current.transcript.entries).toHaveLength(2);
    expect(b.result.current.transcript.entries).toHaveLength(1);
  });

  it("useSessions ignores a frame that changed nothing, and an event stream", async () => {
    const hooks = bindHooks(store);
    applyM([sessions(info("a"))]);
    let renders = 0;
    await renderHook(() => {
      renders++;
      return hooks.useSessions();
    });
    const base = renders;
    await act(() => {
      applyM([sessions(info("a")), text("a", "streaming")]);
    });
    expect(renders).toBe(base);
    await act(() => {
      applyM([sessions(info("a", { phase: "running" }))]);
    });
    expect(renders).toBeGreaterThan(base);
  });

  it("useSession gives a stable empty view for a session not seen", async () => {
    const hooks = bindHooks(store);
    const h = await renderHook(() => hooks.useSession(k("nope")));
    const first = h.result.current;
    await act(() => {
      applyM([user("other", "x")]);
    });
    expect(h.result.current).toBe(first);
    expect(first.transcript.entries).toEqual([]);
  });

  it("useApprovals and useHub select their parts", async () => {
    const hooks = bindHooks(store);
    applyM([stateFrame("a", { approvals: [{ id: 1, summary: { action: "x", files: [], scope: "s" } }] })]);
    const approvals = await renderHook(() => hooks.useApprovals(k("a")));
    const hub = await renderHook(() => hooks.useHub());
    expect(approvals.result.current.map((x) => x.id)).toEqual([1]);
    expect(hub.result.current).toBe("offline");
    await act(() => {
      store.getState().setHub(M, "ready");
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

describe("what the Usage screen reads", () => {
  const apply = (...frames: ToPhone[]) => act(async () => void applyM(frames));

  it("useTurns counts the finished records and starts over at a replay from the start", async () => {
    const hooks = bindHooks(store);
    const turns = await renderHook(() => hooks.useTurns(k("a")));
    expect(turns.result.current).toBe(0);
    await apply(sessions(info("a")), subscribed("a", "start"), ev("a", { type: "finished", data: "completed" }), ev("a", { type: "finished", data: "failed" }));
    expect(turns.result.current).toBe(2);
    await apply(subscribed("a", "start"), ev("a", { type: "finished", data: "completed" }));
    expect(turns.result.current).toBe(1);
  });

  it("useSessionUsage is the view's latest text, else the row's, else null", async () => {
    const hooks = bindHooks(store);
    const usage = await renderHook(() => hooks.useSessionUsage(k("a")));
    expect(usage.result.current).toBeNull();
    await apply(sessions(info("a", { usage: "row usage" })));
    expect(usage.result.current).toBe("row usage");
    await apply(subscribed("a", "start"), ev("a", { type: "usage", data: "view usage" }));
    expect(usage.result.current).toBe("view usage");
    await apply(sessions(info("a", { usage: null })), ev("a", { type: "usage", data: "" }));
    expect(usage.result.current).toBeNull();
  });

  it("useJournals lists the journals, newest last, and is the same array until they change", async () => {
    const hooks = bindHooks(store);
    const journals = await renderHook(() => hooks.useJournals(k("a")));
    expect(journals.result.current).toEqual([]);
    await apply(sessions(info("a")), { v: 1, type: "subscribed", session: "a", from: "start", journals: ["old.jsonl", "j1"] });
    expect(journals.result.current).toEqual(["old.jsonl", "j1"]);
    const same = journals.result.current;
    await apply(text("a", "hi"));
    expect(journals.result.current).toBe(same);
  });

  it("useUsageRows is the listed sessions that report usage", async () => {
    const hooks = bindHooks(store);
    const rows = await renderHook(() => hooks.useUsageRows());
    await apply(sessions(info("a", { usage: "5h 12%" }), info("b"), info("c", { usage: "" }), info("d", { usage: "week 3%" })));
    expect(rows.result.current.map((r) => r.id)).toEqual([k("a"), k("d")]);
    const before = rows.result.current;
    await apply(sessions(info("a", { usage: "5h 12%" }), info("b", { phase: "running" }), info("c", { usage: "" }), info("d", { usage: "week 3%" })));
    expect(rows.result.current).toBe(before);
  });
});

describe("a fresh start marked in the transcript", () => {
  const apply = (...frames: ToPhone[]) => act(async () => void applyM(frames));
  const NOTE = "Fresh conversation — the earlier one is above.";
  const lines = (id: string) => view(id).transcript.entries.map((e) => [e.kind, "text" in e ? e.text : ""]);

  it("addNote puts the divider after the old conversation once, and new-journal events land below it", async () => {
    await apply(sessions(info("a")), subscribed("a", "start"), user("a", "old"), text("a", "old reply"), { v: 1, type: "replayed", session: "a" } as never);
    expect(store.getState().addNote(k("a"), NOTE)).toBe(true);
    expect(store.getState().addNote(k("a"), NOTE)).toBe(true); // a repeated ack: nothing more
    await apply(
      { v: 1, type: "event", session: "a", journal: "j2", seq: 1, event: { type: "user", data: "new" } },
      { v: 1, type: "event", session: "a", journal: "j2", seq: 2, event: { type: "text", data: "new reply" } },
    );
    expect(lines("a")).toEqual([
      ["user", "old"],
      ["reply", "old reply"],
      ["note", NOTE],
      ["user", "new"],
      ["reply", "new reply"],
    ]);
  });

  it("a /new acked while a replay from the start still holds the old entries lands after them and before the new journal", async () => {
    await apply(sessions(info("a")), subscribed("a", "start"), user("a", "old question"));
    // The screen comes back: a replay from the start begins, and has brought nothing yet.
    await apply(subscribed("a", "start"));
    expect(view("a").replaying).toBe(true);
    expect(store.getState().addNote(k("a"), NOTE)).toBe(true);
    await apply(user("a", "old question"), { v: 1, type: "replayed", session: "a" } as never);
    await apply({ v: 1, type: "event", session: "a", journal: "j2", seq: 1, event: { type: "user", data: "new question" } });
    expect(lines("a")).toEqual([
      ["user", "old question"],
      ["note", NOTE],
      ["user", "new question"],
    ]);
  });

  it("is made again at the new journal's first record on a replay from the start, and after the transcript was let go", async () => {
    const j2 = (seq: number, data: string): ToPhone => ({ v: 1, type: "event", session: "a", journal: "j2", seq, event: { type: "user", data } });
    const replay = (): ToPhone[] => [
      { v: 1, type: "subscribed", session: "a", from: "start", journals: ["j1", "j2"] },
      user("a", "old"),
      j2(1, "new"),
      { v: 1, type: "replayed", session: "a" } as never,
    ];
    await apply(sessions(info("a")), subscribed("a", "start"), user("a", "old"), { v: 1, type: "replayed", session: "a" } as never);
    store.getState().addNote(k("a"), NOTE);
    await apply(j2(1, "new"));
    const placed = [
      ["user", "old"],
      ["note", NOTE],
      ["user", "new"],
    ];
    expect(lines("a")).toEqual(placed);
    await apply(...replay());
    expect(lines("a")).toEqual(placed);
    expect(store.getState().evict(k("a"))).toBe(true);
    await apply(...replay());
    expect(lines("a")).toEqual(placed);
  });

  it("is not made for a session the store holds no view of: a later watch must still start fresh", () => {
    expect(store.getState().addNote(k("zz"), NOTE)).toBe(false);
    expect(store.getState().bySession.has(k("zz"))).toBe(false);
  });

  it("changes nothing for the other sessions' views", async () => {
    await apply(sessions(info("a"), info("b")), subscribed("a", "start"), user("a", "x"), subscribed("b", "start"), user("b", "y"));
    const b = view("b");
    store.getState().addNote(k("a"), NOTE);
    expect(view("b")).toBe(b);
  });
});
