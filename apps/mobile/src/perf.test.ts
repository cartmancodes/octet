// Performance budgets: the hub's busiest hour, applied through the real reducer and store. The
// budget is generous for a phone's CPU but tight for the work, so a change that makes a batch cost
// more per event (or more per session) fails here rather than on a train.
import type { BatchItem } from "./client/HubClient";
import { createOctetStore } from "./store";

const SESSION = "a";
const OTHER = "b";
const TEXT_EVENTS = 20_000;
const TOOL_EVENTS = 2_000;
/** The client delivers at most one batch per 50 ms; a busy one holds a few dozen frames. */
const BATCH = 50;
const BUDGET_MS = 1_500;

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

  it("keeps a session's transcript inside its limits however much streams", () => {
    const store = createOctetStore();
    const frames = stream();
    for (let i = 0; i < frames.length; i += BATCH) store.getState().applyBatch(frames.slice(i, i + BATCH));
    const { transcript } = store.getState().bySession.get(SESSION) ?? { transcript: null };
    expect(transcript?.entries.length).toBeLessThanOrEqual(1_000);
    expect(transcript?.bytes).toBeLessThanOrEqual(4 * 1024 * 1024);
  });
});
