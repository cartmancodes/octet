// Render budgets: streaming into one session draws that session's transcript about once per
// batch, and nothing of another session. The harness is the hooks and memoised components the
// Session and Sessions screens use, without the router, so the count is the app's own.
import { act, render, screen } from "@testing-library/react-native";
import { Profiler, useCallback } from "react";
import { View } from "react-native";

import type { Entry } from "./model/transcript";

import type { BatchItem } from "./client/HubClient";
import { SessionCard } from "./components/SessionCard";
import { TranscriptRow } from "./components/TranscriptRow";
import { bindHooks, createOctetStore } from "./store";
import type { SessionInfo } from "./protocol";
import * as cardWords from "./sessions";
import * as rowWords from "./session";

const BATCHES = 10;
const EVENTS = 100;
/** A reply of this many paragraphs is 3 bytes each: well inside the limits on text. */
const PARAGRAPHS = 16_000;
const CHUNKS = 10;
// About 3x the time beside the other suites, so the budgets hold under the whole run's load.
/** 10 chunks: 6-7 ms alone, 14-27 ms beside the other suites (it was 2,446 ms: every chunk
 * split and drew all 16,000 paragraphs). */
const STREAM_BUDGET_MS = 100;
/** One finished reply of 16,000 fences: 62-64 ms alone, 125-222 ms beside the other suites (it
 * was 501-650 ms: one view per block). */
const DONE_BUDGET_MS = 450;
/** Text nodes one reply may draw, however long it is. */
const MAX_TEXT_NODES = 1_000;

const store = createOctetStore();
const { useEntries, useSessions } = bindHooks(store);

let seq = 0;
const event = (session: string, data: string): BatchItem =>
  ({ v: 1, type: "event", session, journal: "j1", seq: ++seq, event: { type: "text", data } }) as BatchItem;

const info = (id: string): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/work/${id}`,
  mode: "ask",
  phase: "running",
  approvals: 0,
  from_phone: false,
});

function Transcript({ session }: { session: string }) {
  const entries = useEntries(session);
  const toggle = useCallback(() => undefined, []);
  return (
    <View testID={`transcript-${session}`}>
      {entries.map((entry) => (
        <TranscriptRow key={entry.key} entry={entry} open={false} onToggle={toggle} />
      ))}
    </View>
  );
}

function Cards() {
  const sessions = useSessions();
  const open = useCallback(() => undefined, []);
  return (
    <View>
      {sessions.map((s) => (
        <SessionCard key={s.id} info={s} disabled={false} onOpen={open} />
      ))}
    </View>
  );
}

afterEach(() => jest.restoreAllMocks());

/** A reply of `n` short paragraphs, streaming or done. */
const reply = (n: number, done: boolean, extra = ""): Extract<Entry, { kind: "reply" }> => ({
  kind: "reply",
  key: "1",
  text: Array.from({ length: n }, (_, i) => `p${i}`).join("\n\n") + extra,
  done,
});
const toggle = () => undefined;

describe("one long reply", () => {
  it(`streams ${CHUNKS} chunks into a reply of ${PARAGRAPHS} paragraphs in under ${STREAM_BUDGET_MS} ms`, async () => {
    let entry = reply(PARAGRAPHS, false);
    const view = await render(<TranscriptRow entry={entry} open={false} onToggle={toggle} />);
    const started = performance.now();
    for (let i = 0; i < CHUNKS; i++) {
      entry = { ...entry, text: `${entry.text} more${i}` };
      await view.rerender(<TranscriptRow entry={entry} open={false} onToggle={toggle} />);
    }
    const took = performance.now() - started;
    console.log(`render: ${CHUNKS} chunks into ${PARAGRAPHS} paragraphs took ${took.toFixed(0)} ms`);
    expect(took).toBeLessThan(STREAM_BUDGET_MS);
    // What is drawn is bounded too, whatever the reply's length.
    expect(screen.container.queryAll((n) => n.type === "Text").length).toBeLessThan(MAX_TEXT_NODES);
  }, 60_000); // a slow run fails on the budget, with its time, not on jest's timeout

  it(`draws a finished reply of ${PARAGRAPHS} code fences in under ${DONE_BUDGET_MS} ms`, async () => {
    const entry: Entry = { kind: "reply", key: "2", text: "```\n".repeat(PARAGRAPHS), done: true };
    const started = performance.now();
    await render(<TranscriptRow entry={entry} open={false} onToggle={toggle} />);
    const took = performance.now() - started;
    console.log(`render: a finished reply of ${PARAGRAPHS} fences took ${took.toFixed(0)} ms`);
    expect(took).toBeLessThan(DONE_BUDGET_MS);
    expect(screen.container.queryAll((n) => n.type === "Text").length).toBeLessThan(MAX_TEXT_NODES);
  });
});

describe("render budgets", () => {
  it("streams 100 text events into session A: its transcript draws once per batch, B's not at all", async () => {
    store.getState().applyBatch([
      { v: 1, type: "sessions", sessions: [info("a"), info("b")] },
      { v: 1, type: "subscribed", session: "a", from: "start", journals: ["j1"] },
      event("a", "start"),
      event("b", "quiet"),
    ] as BatchItem[]);

    const commits = { a: 0, b: 0, cards: 0 };
    await render(
      <>
        <Profiler id="a" onRender={() => void (commits.a += 1)}>
          <Transcript session="a" />
        </Profiler>
        <Profiler id="b" onRender={() => void (commits.b += 1)}>
          <Transcript session="b" />
        </Profiler>
        <Profiler id="cards" onRender={() => void (commits.cards += 1)}>
          <Cards />
        </Profiler>
      </>,
    );
    const rows = jest.spyOn(rowWords, "rowRole");
    const cards = jest.spyOn(cardWords, "engineLine");
    commits.a = commits.b = commits.cards = 0;

    for (let b = 0; b < BATCHES; b++) {
      await act(async () => {
        const batch = Array.from({ length: EVENTS / BATCHES }, (_, i) => event("a", ` ${b * 10 + i}`));
        store.getState().applyBatch(batch);
      });
    }

    console.log(`render: A drew ${commits.a} times (${rows.mock.calls.length} row bodies), B ${commits.b}, cards ${commits.cards}`);
    expect(screen.getByTestId("transcript-a")).toBeTruthy();
    expect(screen.getByText(/start 0 1 2 .* 99$/)).toBeTruthy();
    expect(commits.a).toBeGreaterThan(0);
    expect(commits.a).toBeLessThanOrEqual(EVENTS / BATCHES + 2);
    expect(rows.mock.calls.length).toBeGreaterThan(0);
    expect(rows.mock.calls.length).toBeLessThanOrEqual(EVENTS / BATCHES + 2);
    expect(commits.b).toBe(0);
    expect(commits.cards).toBe(0);
    expect(cards).not.toHaveBeenCalled();

    // The same probes do fire when B's own data changes, so the zeros above mean something.
    await act(async () => {
      store.getState().applyBatch([
        event("b", " more"),
        { v: 1, type: "sessions", sessions: [info("a"), { ...info("b"), phase: "idle" }] },
      ] as BatchItem[]);
    });
    expect(commits.b).toBeGreaterThan(0);
    expect(commits.cards).toBeGreaterThan(0);
    expect(cards).toHaveBeenCalled();
  });
});
