// Performance budgets: the hub's busiest hour, applied through the real reducer and store. The
// budget is generous for a phone's CPU but tight for the work, so a change that makes a batch cost
// more per event (or more per session) fails here rather than on a train.
import type { BatchItem } from "./client/HubClient";
import { MAX_BYTES, MAX_CALLS, MAX_ENTRIES } from "./model/transcript";
import { createOctetStore } from "./store";

const SESSION = "a";
const OTHER = "b";
const TEXT_EVENTS = 20_000;
const TOOL_EVENTS = 2_000;
/** The client delivers at most one batch per 50 ms; a busy one holds a few dozen frames. */
const BATCH = 50;
// Times on a Mac: alone, beside the other suites, and beside them on a busy machine. Budgets
// leave room for the last (a CI runner is no faster) and still fail what the code did before.
/** 12-14 ms alone, 60-115 ms beside the other suites, 319 ms on a busy machine. */
const BUDGET_MS = 1_000;
// The tool-call budgets are totals. The worst batch is logged, not held: there one garbage
// collection is most of the number. Before calls had a fixed cost, a cap per entry and a lookup
// by id, a turn's batches grew dearer with every call, and both totals failed.
/** A hostile hub's turn: calls with no name, target or detail, which once cost nothing. Now
 * 14-24 ms alone, 73-113 ms beside the other suites, 152 ms busy (it was 1,885-2,009 ms alone). */
const HOSTILE_CALLS = 20_000;
const HOSTILE_BUDGET_MS = 600;
/** A real long turn: an agent reading file after file with no text between. Now 4-11 ms alone,
 * 12-33 ms beside the other suites (it was 178-180 ms alone). */
const READ_CALLS = 5_000;
const READ_BUDGET_MS = 100;

let seq = 0;
const event = (session: string, ev: object): BatchItem =>
  ({ v: 1, type: "event", session, journal: "j1", seq: ++seq, event: ev }) as BatchItem;

/** Text chunks, with a prompt and a finished turn every 500, and 1,000 tool calls (started, then
 * done) spread through them. */
function stream(): BatchItem[] {
  const frames: BatchItem[] = [{ v: 1, type: "subscribed", session: SESSION, from: "start", journals: ["j1"] }];
  let tools = 0;
  for (let i = 0; i < TEXT_EVENTS; i++) {
    if (i % 500 === 0) frames.push(event(SESSION, { type: "user", data: `prompt ${i}` }));
    frames.push(event(SESSION, { type: "text", data: `word${i} ` }));
    if (i % 20 === 10 && tools < TOOL_EVENTS / 2) {
      const call = { id: `t${tools++}`, name: "Bash", target: `cargo test ${i}`, phase: "running", detail: "" };
      frames.push(event(SESSION, { type: "tool", data: call }));
      frames.push(event(SESSION, { type: "tool", data: { ...call, phase: "done", detail: "ok" } }));
    }
    if (i % 500 === 499) frames.push(event(SESSION, { type: "finished", data: "" }));
  }
  return frames;
}

describe("performance budgets", () => {
  it(`applies ${TEXT_EVENTS} text and ${TOOL_EVENTS} tool events in under ${BUDGET_MS} ms`, () => {
    const store = createOctetStore();
    // Another session, with a view of its own, that the stream never touches.
    store.getState().applyBatch([event(OTHER, { type: "text", data: "quiet" })]);
    const other = store.getState().bySession.get(OTHER);
    expect(other).toBeDefined();

    const frames = stream();
    const batches: BatchItem[][] = [];
    for (let i = 0; i < frames.length; i += BATCH) batches.push(frames.slice(i, i + BATCH));

    const started = performance.now();
    for (const batch of batches) store.getState().applyBatch(batch);
    const took = performance.now() - started;
    console.log(`perf: ${frames.length} frames in ${batches.length} batches took ${took.toFixed(0)} ms`);

    expect(took).toBeLessThan(BUDGET_MS);
    const view = store.getState().bySession.get(SESSION);
    expect(view?.transcript.entries.length).toBeGreaterThan(40); // the prompts and turns are there
    const calls = view?.transcript.entries.flatMap((e) => (e.kind === "tools" ? e.calls : [])) ?? [];
    expect(calls.length).toBeGreaterThan(0);
    const replies = view?.transcript.entries.filter((e) => e.kind === "reply") ?? [];
    expect(replies.at(-1)).toMatchObject({ done: true });
    expect(replies.at(-1)).toHaveProperty("text", expect.stringContaining("word19999 "));
    // Session B's view is the very object it was: nothing about A touched it.
    expect(store.getState().bySession.get(OTHER)).toBe(other);
  });

  /** Applies `frames` in batches of `size`; the total and the worst batch, in ms. */
  const timed = (store: ReturnType<typeof createOctetStore>, frames: BatchItem[], size: number) => {
    let worst = 0;
    const started = performance.now();
    for (let i = 0; i < frames.length; i += size) {
      const t = performance.now();
      store.getState().applyBatch(frames.slice(i, i + size));
      worst = Math.max(worst, performance.now() - t);
    }
    return { took: performance.now() - started, worst };
  };
  /** One turn of `n` calls, each started then done, with no text between them. */
  const calls = (n: number, made: (i: number) => { name: string; target: string; detail: string }): BatchItem[] => {
    const frames: BatchItem[] = [
      { v: 1, type: "subscribed", session: SESSION, from: "start", journals: ["j1"] } as BatchItem,
      event(SESSION, { type: "user", data: "go" }),
    ];
    for (let i = 0; i < n; i++) {
      const { name, target, detail } = made(i);
      frames.push(event(SESSION, { type: "tool", data: { id: `c${i}`, name, target, phase: "running", detail: "" } }));
      frames.push(event(SESSION, { type: "tool", data: { id: `c${i}`, name, target, phase: "done", detail } }));
    }
    return frames;
  };
  const toolsOf = (store: ReturnType<typeof createOctetStore>) =>
    (store.getState().bySession.get(SESSION)?.transcript.entries ?? []).flatMap((e) => (e.kind === "tools" ? [e] : []));

  it(`holds a hostile turn of ${HOSTILE_CALLS} empty calls in bounded memory, in under ${HOSTILE_BUDGET_MS} ms`, () => {
    const store = createOctetStore();
    const { took, worst } = timed(store, calls(HOSTILE_CALLS, () => ({ name: "", target: "", detail: "" })), 200);
    console.log(`perf: ${HOSTILE_CALLS} empty calls took ${took.toFixed(0)} ms, worst batch ${worst.toFixed(1)} ms`);
    const transcript = store.getState().bySession.get(SESSION)?.transcript;
    const entries = toolsOf(store);
    // Every call is charged for: the entry is not free, and none holds more than MAX_CALLS.
    expect(transcript?.bytes).toBeGreaterThan(0);
    expect(transcript?.bytes).toBeLessThanOrEqual(MAX_BYTES);
    expect(transcript?.entries.length).toBeLessThanOrEqual(MAX_ENTRIES);
    for (const e of entries) expect(e.calls.length).toBeLessThanOrEqual(MAX_CALLS);
    // What the phone does not hold is counted, for the "on the Mac" row.
    expect(entries.reduce((n, e) => n + e.calls.length + (e.more ?? 0), 0)).toBe(HOSTILE_CALLS);
    expect(took).toBeLessThan(HOSTILE_BUDGET_MS);
  });

  it(`takes a turn of ${READ_CALLS} Read calls in under ${READ_BUDGET_MS} ms`, () => {
    const store = createOctetStore();
    const { took, worst } = timed(store, calls(READ_CALLS, (i) => ({ name: "Read", target: `src/f${i}.rs`, detail: "" })), BATCH);
    console.log(`perf: ${READ_CALLS} Read calls took ${took.toFixed(0)} ms, worst batch ${worst.toFixed(1)} ms`);
    for (const e of toolsOf(store)) expect(e.calls.length).toBeLessThanOrEqual(MAX_CALLS);
    expect(took).toBeLessThan(READ_BUDGET_MS);
  });

  it("keeps a session's transcript inside its limits however much streams", () => {
    const store = createOctetStore();
    const frames = stream();
    for (let i = 0; i < frames.length; i += BATCH) store.getState().applyBatch(frames.slice(i, i + BATCH));
    const { transcript } = store.getState().bySession.get(SESSION) ?? { transcript: null };
    expect(transcript?.entries.length).toBeLessThanOrEqual(1_000);
    expect(transcript?.bytes).toBeLessThanOrEqual(4 * 1024 * 1024);
  });
});
