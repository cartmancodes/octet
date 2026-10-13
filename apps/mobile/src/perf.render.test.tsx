// Render budgets: streaming into one session draws that session's transcript about once per
// batch, and nothing of another session. The harness is the hooks and memoised components the
// Session and Sessions screens use, without the router, so the count is the app's own.
import { act, render, screen } from "@testing-library/react-native";
import { Profiler, useCallback } from "react";
import { View } from "react-native";

import type { BatchItem } from "./client/HubClient";
import { SessionCard } from "./components/SessionCard";
import { TranscriptRow } from "./components/TranscriptRow";
import { bindHooks, createOctetStore } from "./store";
import type { SessionInfo } from "./protocol";
import * as cardWords from "./sessions";
import * as rowWords from "./session";

const BATCHES = 10;
const EVENTS = 100;

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
    expect(rows.mock.calls.length).toBeLessThanOrEqual(EVENTS / BATCHES + 2);
    expect(commits.b).toBe(0);
    expect(commits.cards).toBe(0);
    expect(cards).not.toHaveBeenCalled();
    jest.restoreAllMocks();
  });
});
