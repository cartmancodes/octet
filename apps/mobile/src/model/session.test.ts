import type { ApprovalSummary, CatalogInfo, KnownJournalEvent, StateInfo } from "../protocol";
import { EMPTY_VIEW, reduceView, type Op, type SessionView } from "./session";

const ev = (event: KnownJournalEvent): Op => ({ op: "event", ev: event });
const summary = (action: string, extra: Partial<ApprovalSummary> = {}): ApprovalSummary => ({
  action,
  files: [],
  scope: "once",
  ...extra,
});
const approval = (id: number, action = "Run a shell command"): Op =>
  ev({ type: "approval", data: { id, detail: `{"id":${id}}`, summary: summary(action) } });
const closed = (id: number): Op => ev({ type: "approval_closed", data: id });
const state = (over: Partial<StateInfo> = {}): StateInfo => ({
  phase: "idle",
  queue: [],
  queued: 0,
  mode: "ask",
  approvals: [],
  ...over,
});
const view = (ops: Op[], from: SessionView = EMPTY_VIEW): SessionView => reduceView(from, ops);
const ids = (v: SessionView): number[] => v.approvals.map((a) => a.id);

describe("approvals", () => {
  it("come from approval and approval_closed records", () => {
    let v = view([approval(1), approval(2)]);
    expect(ids(v)).toEqual([1, 2]);
    v = view([closed(1)], v);
    expect(ids(v)).toEqual([2]);
    expect(v.approvals[0]?.summary.action).toBe("Run a shell command");
    expect(v.approvals[0]?.detail).toBe('{"id":2}');
  });

  it("are not duplicated by a replayed record", () => {
    expect(ids(view([approval(1), approval(1)]))).toEqual([1]);
  });

  it("are replaced by the state frame's list", () => {
    const v = view([approval(1), { op: "state", state: state({ approvals: [{ id: 5, summary: summary("Edit 2 files") }] }) }]);
    expect(ids(v)).toEqual([5]);
    expect(v.state?.mode).toBe("ask");
  });

  it("keep their detail when the state frame lists them again", () => {
    const v = view([approval(1), { op: "state", state: state({ approvals: [{ id: 1, summary: summary("Run a shell command") }] }) }]);
    expect(v.approvals[0]?.detail).toBe('{"id":1}');
  });

  it("all go when the session stops", () => {
    expect(view([approval(1), approval(2), ev({ type: "stopped", data: null })]).approvals).toEqual([]);
  });

  it("are cleaned and bounded", () => {
    const v = view([
      ev({
        type: "approval",
        data: {
          id: 1,
          detail: "d".repeat(1_000_000),
          summary: summary("Run\u001b[1m it", { command: "c".repeat(1_000_000), files: Array.from({ length: 5_000 }, (_, i) => `f${i}`), cwd: "/tmp\u0007" }),
        },
      }),
    ]);
    const a = v.approvals[0];
    expect(a?.summary.action).toBe("Run it");
    expect(a?.summary.cwd).toBe("/tmp");
    expect((a?.summary.command ?? "").length).toBeLessThan(20_000);
    expect(a?.summary.files.length).toBeLessThanOrEqual(200);
    expect((a?.detail ?? "").length).toBeLessThan(20_000);
  });

  it("ignores a malformed approval", () => {
    const bad = [{ type: "approval", data: { id: "x" } }, { type: "approval", data: null }, { type: "approval_closed", data: "1" }] as unknown as KnownJournalEvent[];
    expect(view(bad.map(ev))).toBe(EMPTY_VIEW);
  });

  it("stay at 32 pending", () => {
    expect(view(Array.from({ length: 50 }, (_, i) => approval(i))).approvals).toHaveLength(32);
  });
});

describe("the other records", () => {
  it("keep the mode, model and usage", () => {
    const v = view([ev({ type: "mode", data: "auto" }), ev({ type: "model_selected", data: "opus" }), ev({ type: "usage", data: "1k tokens" })]);
    expect(v).toMatchObject({ mode: "auto", model: "opus", usage: "1k tokens" });
  });

  it("put transcript records in the transcript", () => {
    const v = view([ev({ type: "user", data: "hi" }), ev({ type: "text", data: "hello" })]);
    expect(v.transcript.entries).toMatchObject([{ kind: "user" }, { kind: "reply", text: "hello" }]);
  });
});

describe("a_reset_replaces_the_transcript", () => {
  it("drops the transcript and the approvals the replay will bring back", () => {
    const old = view([ev({ type: "user", data: "old" }), ev({ type: "text", data: "old reply" }), approval(1)]);
    const after = view([{ op: "reset" }, ev({ type: "user", data: "new" })], old);
    expect(after.transcript.entries).toMatchObject([{ kind: "user", text: "new" }]);
    expect(after.transcript.bytes).toBe(3);
    expect(after.approvals).toEqual([]);
  });

  it("keeps the state and catalog, which are not journal records", () => {
    const s = state({ now: "Reading" });
    const old = view([{ op: "state", state: s }]);
    const { approvals: _approvals, ...rest } = s;
    expect(view([{ op: "reset" }], old).state).toEqual(rest);
  });

  it("clears a failure", () => {
    expect(view([{ op: "reset" }], view([{ op: "failed", reason: "gone" }])).failed).toBeNull();
  });
});

describe("state and catalog", () => {
  const catalog = (n: number): CatalogInfo => ({
    modes: ["ask", "auto"],
    current_mode: "ask",
    models: Array.from({ length: n }, (_, i) => ({ engine: "claude", selection: `s${i}`, name: `Model ${i}`, id: `m${i}` })),
    current: null,
  });

  it("keep the same object when a frame says nothing new", () => {
    const v1 = view([{ op: "state", state: state({ now: "x" }) }, { op: "catalog", catalog: catalog(3) }]);
    const v2 = view([{ op: "state", state: state({ now: "x" }) }, { op: "catalog", catalog: catalog(3) }], v1);
    expect(v2).toBe(v1);
    const v3 = view([{ op: "state", state: state({ now: "y" }) }], v1);
    expect(v3).not.toBe(v1);
    expect(v3.catalog).toBe(v1.catalog);
  });

  it("clip a huge catalog and a huge state", () => {
    const v = view([
      { op: "catalog", catalog: catalog(100_000) },
      { op: "state", state: state({ queue: Array.from({ length: 1_000 }, () => "q".repeat(100_000)), now: "n".repeat(1_000_000), goal: { objective: "o".repeat(1_000_000), status: "active", turns: 1, max: 5 } }) },
    ]);
    expect(v.catalog?.models.length).toBeLessThanOrEqual(500);
    expect(v.state?.queue.length).toBeLessThanOrEqual(8);
    expect((v.state?.queue[0] ?? "").length).toBeLessThanOrEqual(8_000);
    expect((v.state?.now ?? "").length).toBeLessThanOrEqual(8_000);
    expect((v.state?.goal?.objective ?? "").length).toBeLessThanOrEqual(8_000);
  });

  it("clean control characters from state strings", () => {
    const v = view([{ op: "state", state: state({ now: "Running \u001b[31mls\u0007", queue: ["a‮b"] }) }]);
    expect(v.state?.now).toBe("Running ls");
    expect(v.state?.queue).toEqual(["ab"]);
  });

  it("survive a state that is not an object of the right shape", () => {
    const junk = { op: "state", state: { approvals: "no", queue: 5, mode: 3 } } as unknown as Op;
    expect(() => view([junk])).not.toThrow();
  });
});

describe("failed", () => {
  it("marks the view failed and keeps the transcript", () => {
    const v = view([{ op: "failed", reason: "Not allowed" }], view([ev({ type: "user", data: "x" })]));
    expect(v.failed).toBe("Not allowed");
    expect(v.transcript.entries).toHaveLength(1);
  });
});
