// Render budgets: streaming into one session draws that session's transcript about once per
// batch, and nothing of another session. The harness is the hooks and memoised components the
// Session and Sessions screens use, without the router, so the count is the app's own.
import { act, render, screen } from "@testing-library/react-native";
import { Profiler, useCallback, useMemo, useState } from "react";
import { Text, View } from "react-native";

import type { Entry } from "./model/transcript";

import type { BatchItem } from "./client/HubClient";
import { createAnswerLedger } from "./approvals";
import { ConvoControls } from "./components/ConvoControls";
import { InboxCard } from "./components/InboxCard";
import { TranscriptRow } from "./components/TranscriptRow";
import * as cardWords from "./inbox";
import { bindHooks, createOctetStore } from "./store";
import type { SessionInfo } from "./protocol";
import * as rowWords from "./session";

const BATCHES = 10;
const EVENTS = 100;
/** A reply of this many paragraphs is 3 bytes each: well inside the limits on text. */
const PARAGRAPHS = 16_000;
const CHUNKS = 10;
// Times on a Mac: alone, beside the other suites, and on a busy machine. Budgets leave room
// for the last and still fail what the code did before.
/** 10 chunks: 6-7 ms alone, 14-32 ms beside the other suites, 131 ms busy (it was 2,446 ms:
 * every chunk split and drew all 16,000 paragraphs). */
const STREAM_BUDGET_MS = 500;
/** One finished reply of 16,000 fences: 62-64 ms alone, 125-222 ms beside the other suites (it
 * was 501-650 ms alone). The time is a guard; what failed before is the count of text nodes:
 * one view per block, 16,000 of them. */
const DONE_BUDGET_MS = 1_000;
/** Text nodes one reply may draw, however long it is. */
const MAX_TEXT_NODES = 1_000;

/** Draws of the prompt line and the key bar, counted by a wrapper that adds nothing: each is
 * memoised, and the wrapper draws the real one exactly when its memo lets the props through. */
const mockDraws = { prompt: 0, keys: 0 };
jest.mock("./components/PromptLine", () => {
  const actual = jest.requireActual("./components/PromptLine");
  const { createElement, memo } = jest.requireActual("react");
  const Counted = (props: object) => {
    mockDraws.prompt += 1;
    return createElement(actual.PromptLine, props);
  };
  return { ...actual, PromptLine: memo(Counted) };
});
jest.mock("./components/KeyBar", () => {
  const actual = jest.requireActual("./components/KeyBar");
  const { createElement, memo } = jest.requireActual("react");
  const Counted = (props: object) => {
    mockDraws.keys += 1;
    return createElement(actual.KeyBar, props);
  };
  return { ...actual, KeyBar: memo(Counted) };
});

const store = createOctetStore();
const { useEntries, useSessions, useApprovals, useCatalog, useSessionState } = bindHooks(store);

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

/** A conversation's foot as its screen mounts it, under a body that draws with every batch: the
 * Permission card, the prompt line and the key bar, with the props the screen gives them. */
function Conversation({ session }: { session: string }) {
  const entries = useEntries(session);
  const approvals = useApprovals(session);
  const catalog = useCatalog(session);
  const phase = useSessionState(session)?.phase ?? "running";
  const send = useCallback(async () => undefined, []);
  return (
    <View>
      <Text testID={`entries-${session}`}>{entries.length}</Text>
      <ConvoControls
        session={session}
        phase={phase}
        engine="claude"
        mode={catalog?.current_mode ?? "ask"}
        modes={catalog?.modes ?? null}
        model={null}
        approvals={approvals}
        send={send}
        offline={false}
        covered={false}
        bottom={34}
      />
    </View>
  );
}

/** The Inbox's cards, as its screen makes them. */
function Cards() {
  const sessions = useSessions();
  const items = useMemo(() => {
    const inbox = cardWords.classify(sessions, new Map(sessions.map((s) => [s.id, s.approval ?? null])), { mac: "mac", now: 0 });
    return [...inbox.needs, ...inbox.running, ...inbox.done];
  }, [sessions]);
  const open = useCallback(() => undefined, []);
  const send = useCallback(async () => undefined, []);
  const isFront = useCallback(() => false, []);
  const [ledger] = useState(createAnswerLedger);
  return (
    <View>
      {items.map((item) => (
        <InboxCard key={item.id} item={item} offline={false} covered={false} send={send} ledger={ledger} isFront={isFront} onOpen={open} />
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
    const cards = jest.spyOn(cardWords, "cardLabel");
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

  it("streams 100 entries into a conversation: its prompt line and key bar draw 0 times", async () => {
    store.getState().applyBatch([
      { v: 1, type: "sessions", sessions: [info("c")] },
      { v: 1, type: "subscribed", session: "c", from: "start", journals: ["j1"] },
      event("c", "start"),
    ] as BatchItem[]);
    await render(<Conversation session="c" />);
    expect(mockDraws.prompt).toBeGreaterThan(0);
    expect(mockDraws.keys).toBeGreaterThan(0);
    mockDraws.prompt = 0;
    mockDraws.keys = 0;

    for (let b = 0; b < BATCHES; b++) {
      await act(async () => {
        store.getState().applyBatch(
          Array.from({ length: EVENTS / BATCHES }, (_, i) => ({
            v: 1,
            type: "event",
            session: "c",
            journal: "j1",
            seq: ++seq,
            event: i % 2 === 0 ? { type: "user", data: `ask ${b}-${i}` } : { type: "notice", data: `note ${b}-${i}` },
          })) as BatchItem[],
        );
      });
    }

    console.log(`render: over ${EVENTS} entries the prompt line drew ${mockDraws.prompt} times, the key bar ${mockDraws.keys}`);
    expect(screen.getByTestId("entries-c").props.children).toBeGreaterThanOrEqual(EVENTS);
    expect(mockDraws.prompt).toBe(0);
    expect(mockDraws.keys).toBe(0);

    // The same probes do fire when what the controls show changes, so the zeros mean something.
    await act(async () => {
      store.getState().applyBatch([
        { v: 1, type: "event", session: "c", journal: "j1", seq: ++seq, event: { type: "approval", data: { id: 1, detail: "", summary: { action: "Run", command: "make", files: [], scope: "once" } } } },
      ] as BatchItem[]);
    });
    expect(mockDraws.prompt).toBeGreaterThan(0);
    expect(mockDraws.keys).toBeGreaterThan(0);
  });
});
