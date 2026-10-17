import { FakeClock } from "./client/fakes";
import type { BatchItem } from "./client/HubClient";
import { COALESCE_MS, createCoalescer, type Batch } from "./coalesce";
import type { ToPhone } from "./protocol";
import { createOctetStore } from "./store";

const text = (session: string, seq: number): ToPhone => ({
  v: 1,
  type: "event",
  session,
  journal: "j1",
  seq,
  event: { type: "text", data: `${seq} ` },
});

describe("the coalescer", () => {
  it("commits a lone batch at once", () => {
    const clock = new FakeClock();
    const commits: Batch[][] = [];
    const c = createCoalescer((list) => (commits.push(list), list.map(() => [])), { now: clock.now, schedule: clock.schedule });
    c.push("m1", [text("m1/a", 1)]);
    expect(commits).toHaveLength(1);
    expect(clock.pending).toBe(0);
  });

  it("holds a batch that comes within 50 ms of a commit until the 50 ms mark, every pending Mac in one commit, in arrival order", () => {
    expect(COALESCE_MS).toBe(50);
    const clock = new FakeClock();
    const commits: Batch[][] = [];
    const c = createCoalescer((list) => (commits.push(list), list.map(() => [])), { now: clock.now, schedule: clock.schedule });
    c.push("m1", [text("m1/a", 1)]);
    clock.advance(10);
    c.push("m2", [text("m2/a", 1)]);
    clock.advance(10);
    c.push("m1", [text("m1/a", 2)]);
    c.push("m3", [text("m3/a", 1)]);
    expect(commits).toHaveLength(1);
    clock.advance(29);
    expect(commits).toHaveLength(1);
    clock.advance(1);
    expect(commits).toHaveLength(2);
    const second = commits[1] ?? [];
    expect(second.map((b) => b.mac)).toEqual(["m2", "m1", "m3"]);
    // One Mac's frames stay in their order.
    expect(second[1]?.frames.map((f) => (f as { seq: number }).seq)).toEqual([2]);
  });

  it("keeps the store to at most 5 commits while two Macs stream 20 batches each within 200 ms", () => {
    const clock = new FakeClock();
    const store = createOctetStore();
    const c = createCoalescer((list) => store.getState().applyBatches(list), { now: clock.now, schedule: clock.schedule });
    let commits = 0;
    store.subscribe(() => (commits += 1));
    for (let i = 0; i < 20; i++) {
      c.push("m1", [text("m1/a", i + 1)]);
      clock.advance(5);
      c.push("m2", [text("m2/a", i + 1)]);
      clock.advance(5);
    }
    clock.advance(50);
    expect(commits).toBeLessThanOrEqual(5);
    // Nothing was lost: every batch of both Macs is in.
    const all = Array.from({ length: 20 }, (_, i) => `${i + 1} `).join("");
    expect(store.getState().bySession.get("m1/a")?.transcript.entries).toMatchObject([{ text: all }]);
    expect(store.getState().bySession.get("m2/a")?.transcript.entries).toMatchObject([{ text: all }]);
  });

  it("calls each Mac's sink around its commit, with the sessions that ended", () => {
    const clock = new FakeClock();
    const store = createOctetStore();
    const c = createCoalescer((list) => store.getState().applyBatches(list), { now: clock.now, schedule: clock.schedule });
    const list = (mac: string, ...ids: string[]): BatchItem => ({
      v: 1,
      type: "sessions",
      sessions: ids.map((id) => ({ id: `${mac}/${id}`, engine: "claude", workspace: "/w", mode: "ask", phase: "idle", approvals: 0, from_phone: false })),
    });
    c.push("m1", [list("m1", "a", "b")]);
    const seen: string[] = [];
    c.push("m1", [list("m1", "b")], { before: () => seen.push("before"), after: (ended) => seen.push(`after ${ended.join(",")}`) });
    clock.advance(50);
    expect(seen).toEqual(["before", "after m1/a"]);
  });

  it("a throwing sink stops no other: every Mac's after runs, and the error is reported", () => {
    const clock = new FakeClock();
    const error = jest.spyOn(console, "error").mockImplementation(() => undefined);
    const commits: Batch[][] = [];
    const c = createCoalescer((list) => (commits.push(list), list.map(() => [])), { now: clock.now, schedule: clock.schedule });
    c.push("m0", [text("m0/a", 1)]); // a commit, so the next two wait for one together
    const seen: string[] = [];
    const bad = { before: () => void seen.push("before m1"), after: () => { seen.push("after m1"); throw new Error("m1's sink"); } };
    const throwsFirst = { before: () => { throw new Error("m2's before"); }, after: () => void seen.push("after m2") };
    c.push("m1", [text("m1/a", 1)], bad);
    c.push("m2", [text("m2/a", 1)], throwsFirst);
    c.push("m3", [text("m3/a", 1)], { after: () => void seen.push("after m3") });
    clock.advance(50);
    expect(commits[1]?.map((b) => b.mac)).toEqual(["m1", "m2", "m3"]);
    expect(seen).toEqual(["before m1", "after m1", "after m2", "after m3"]);
    expect(error).toHaveBeenCalledTimes(2);
    error.mockRestore();
  });

  it("skips the after of a batch the store did not apply, and runs the others'", () => {
    const clock = new FakeClock();
    const seen: string[] = [];
    const c = createCoalescer((list) => list.map((b) => (b.mac === "m1" ? null : [`${b.mac}/gone`])), { now: clock.now, schedule: clock.schedule });
    c.push("m0", [text("m0/a", 1)]);
    c.push("m1", [text("m1/a", 1)], { after: () => void seen.push("m1") });
    c.push("m2", [text("m2/a", 1)], { after: (ended) => void seen.push(`m2 ${ended.join(",")}`) });
    clock.advance(50);
    expect(seen).toEqual(["m2 m2/gone"]);
  });

  it("drops only the pending batch of the feed let go, not a newer feed's for the same Mac", () => {
    const clock = new FakeClock();
    const commits: Batch[][] = [];
    const c = createCoalescer((list) => (commits.push(list), list.map(() => [])), { now: clock.now, schedule: clock.schedule });
    const oldFeed = {};
    const newFeed = {};
    c.push("m1", [text("m1/a", 1)], oldFeed);
    c.push("m1", [text("m1/a", 2)], oldFeed);
    c.push("m1", [text("m1/b", 1)], newFeed);
    c.drop("m1", oldFeed);
    clock.advance(50);
    expect(commits[1]?.map((b) => b.frames.map((f) => (f as { session: string }).session))).toEqual([["m1/b"]]);
  });

  it("drops what a Mac had pending when it is let go, and leaves no timer when nothing is", () => {
    const clock = new FakeClock();
    const commits: Batch[][] = [];
    const c = createCoalescer((list) => (commits.push(list), list.map(() => [])), { now: clock.now, schedule: clock.schedule });
    c.push("m1", [text("m1/a", 1)]);
    c.push("m1", [text("m1/a", 2)]);
    c.push("m2", [text("m2/a", 1)]);
    c.drop("m1");
    clock.advance(50);
    expect(commits.map((list) => list.map((b) => b.mac))).toEqual([["m1"], ["m2"]]);
    c.push("m1", [text("m1/a", 3)]);
    c.drop("m1");
    expect(clock.pending).toBe(0);
  });
});
