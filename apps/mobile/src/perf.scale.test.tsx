// The fleet at its stated scale: four Macs of 200 sessions each, streaming their lists, with the
// whole shell up, the Inbox, the drawer (open), the Live Activity controller and the approval
// toasts, each reading every row. Each store commit fits in the 50 ms between commits, each Mac's
// lists are cleaned once per commit, and a list change redraws only what changed.
import { act, fireEvent, screen } from "@testing-library/react-native";
// renderRouter runs on jest's fake timers, which stop the global `performance`; this one is real.
import { performance as wall } from "node:perf_hooks";
import type { ComponentType } from "react";

import { openDrawer } from "./drawer";
import * as inboxWords from "./inbox";
import { FakeLive } from "./liveRig";
import { MAX_MESSAGE, MAX_SESSION_ROWS, type SessionInfo } from "./protocol";
import { MACS, hearOn, info, mount, pairedMacs, socketTo } from "./screenRig";
import { octetStore } from "./store";
import * as rowWords from "./store/rows";
import * as toastWords from "./toast";

/** Draws of the Inbox's cards, counted by a wrapper that adds nothing: it is memoised with the
 * card's own comparison, so it draws the real one exactly when the real one would draw. */
const mockDraws = { cards: 0 };
jest.mock("./components/InboxCard", () => {
  const actual = jest.requireActual("./components/InboxCard");
  const { createElement: make, memo: keep } = jest.requireActual("react");
  const Counted = (props: object) => {
    mockDraws.cards += 1;
    return make(actual.InboxCard, props);
  };
  return { ...actual, InboxCard: keep(Counted, actual.InboxCard.compare) };
});

/** Rows per Mac: the protocol's cap, which the hub keeps to. */
const ROWS = MAX_SESSION_ROWS;
/** Of each Mac's rows: running ones, whose NOW lines move, and ones with an approval waiting. */
const RUNNING = 20;
const WAITING = 8;
/** Windows streamed and measured: one second of the client's 50 ms batches. */
const WINDOWS = 20;
/** Windows streamed first, not measured: V8 compiles the hot paths in these (the first window
 * took 24-55 ms where the rest took 5-15), and Hermes on the phone has no such warm-up to pay. */
const WARMUP = 5;
/** Lists each Mac sends in one window. The hub sends at most four a second (one leading, then one
 * per 250 ms), so a window sees one, or two when a hello's answer meets a flush; three is past
 * anything it sends and still has to fit. */
const PER_WINDOW = 3;
/** Rows of a Mac whose NOW line changes in each list: a busy fleet's tool calls. */
const MOVING = 5;
/**
 * The commit budget: the store commits at most once per 50 ms (src/coalesce.ts), so one commit (every
 * Mac's batch applied, and each store listener run, the Live Activity controller's model, owner and
 * App Group entries among them) must take less than the 50 ms to the next, or commits queue and
 * the JS thread never idles. The review measured 2.65 ms per list change in V8 before any React
 * work, and Hermes runs this kind of code several times slower: the room left to the phone is what
 * this leaves of the 50 ms. Measured on a Mac, alone: about 4 ms a commit (9-14 ms before the phone
 * reconciled only a Mac's last list in a batch). The worst commit and each whole window (the lists
 * parsed, the commit, React drawing the Inbox and the drawer) are logged, not held: beside the
 * other suites, with Spotlight indexing what `npm ci` wrote, a window took 190 ms where it takes
 * 15-40 alone, so a wall-clock limit on them says more about the machine than the code. The counts
 * below hold under any load, and fail what the code did before.
 */
const COMMIT_BUDGET_MS = 50;

const BASE = 1_790_000_000;
const IDS = Array.from({ length: 4 }, (_, mac) => Array.from({ length: ROWS }, (_, i) => `${100 + i}-${BASE + mac}`));
const ids = (mac: number): string[] => IDS[mac] ?? [];

/** One Mac's row `i` in list `n`: a 200-character goal, a NOW line, a usage line and a real-length
 * workspace, as the review's frame-size probe had them. */
function row(mac: number, i: number, n: number): SessionInfo {
  const id = ids(mac)[i] ?? "";
  const running = i < RUNNING;
  const waiting = i >= RUNNING && i < RUNNING + WAITING;
  const moving = running && i % RUNNING < MOVING;
  return info(id, {
    workspace: `/Users/me/Projects/octet-${mac}/worktrees/feature-${i}`,
    phase: running ? "running" : "idle",
    goal: `Ship the ${i}th change to the hub: ${"keep the sessions list bounded and shared ".repeat(5)}`.slice(0, 200),
    now: running ? `Bash · cargo test -p octet-hub (${moving ? n : 0})` : null,
    usage: "5h 12% · week 3% · resets 14:00",
    last_activity: BASE + (moving ? n * 10 + i : i),
    ...(waiting
      ? { approvals: 1, approval: { id: i, summary: { action: "Run a command", command: `cargo test ${i}`, files: [], scope: "once" } } }
      : {}),
  });
}

const list = (mac: number, n: number): SessionInfo[] => Array.from({ length: ROWS }, (_, i) => row(mac, i, n));
const frameText = (mac: number, n: number): string => JSON.stringify({ v: 1, type: "sessions", sessions: list(mac, n) });

const median = (xs: number[]): number => {
  const sorted = [...xs].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)] ?? 0;
};

beforeEach(async () => {
  await pairedMacs(4);
});
afterEach(() => jest.restoreAllMocks());

describe("four Macs of 200 sessions each, streaming", () => {
  it(`commits in under ${COMMIT_BUDGET_MS} ms, cleans each Mac's list once a commit, and redraws only what changed`, async () => {
    const live = new FakeLive();
    const rig = await mount("/", {} as Record<string, ComponentType>, { live });
    const hosts = MACS.map((m) => m.host);
    await act(async () => undefined);
    for (const [m, host] of hosts.entries()) await hearOn(rig, host, list(m, 0));
    // The Running tab, whose cards are the rows that move; then the drawer opens over it.
    await act(async () => fireEvent.press(screen.getByRole("tab", { name: /^Running/ })));
    await act(async () => openDrawer());
    expect(octetStore.getState().sessions).toHaveLength(4 * ROWS);
    // A realistic list is a whole message, near the hub's bound: the work below is the real size.
    const size = frameText(0, 1).length;
    expect(size).toBeGreaterThan(MAX_MESSAGE * 0.6);
    expect(size).toBeLessThanOrEqual(MAX_MESSAGE);

    // The lists, as text, made before the clock starts: on the phone they arrive from the socket.
    const texts = Array.from({ length: WARMUP + WINDOWS }, (_, w) =>
      hosts.map((_, m) => Array.from({ length: PER_WINDOW }, (_, f) => frameText(m, 1 + w * PER_WINDOW + f))),
    );
    const sockets = hosts.map((host) => socketTo(rig, host));
    /** One window: every list in it arrives (and is parsed), then 50 ms pass on the links' clock and
     * on the app's own timers (the Island's, the cards' "ago"). Its time on the wall. */
    const stream = async (window: string[][]): Promise<number> => {
      const t = wall.now();
      await act(async () => {
        for (const [m, frames] of window.entries()) for (const text of frames) sockets[m]?.receive(text);
        rig.clock.advance(50);
        jest.advanceTimersByTime(50);
      });
      return wall.now() - t;
    };
    for (const window of texts.slice(0, WARMUP)) await stream(window);

    // What each consumer does, counted from here.
    const real = octetStore.getState().applyBatches;
    let commits = 0;
    const commitMs: number[] = [];
    octetStore.setState({
      applyBatches: (batches) => {
        commits += 1;
        const t = wall.now();
        try {
          return real(batches);
        } finally {
          commitMs.push(wall.now() - t);
        }
      },
    });
    const inboxes = jest.spyOn(inboxWords, "inboxOf");
    const drawers = jest.spyOn(inboxWords, "drawerList");
    const toasts = jest.spyOn(toastWords, "showToast");
    const lists = jest.spyOn(rowWords, "reconcile");
    const shownBefore = live.shown().length;
    const entriesBefore = live.of("setEntries").length;
    mockDraws.cards = 0;
    // The open drawer is modal, so the Inbox under it is hidden from queries, not from drawing.
    const onScreen = screen.queryAllByTestId(/^inbox-card-/, { hidden: true }).length;
    expect(onScreen).toBeGreaterThan(0);
    expect(onScreen).toBeLessThan(4 * ROWS); // the list draws a window of cards, not every row

    const windows: number[] = [];
    for (const window of texts.slice(WARMUP)) windows.push(await stream(window));
    const streamedMs = WINDOWS * 50;
    octetStore.setState({ applyBatches: real });

    const meanOf = (xs: number[]): number => xs.reduce((a, b) => a + b, 0) / Math.max(xs.length, 1);
    const commitMean = meanOf(commitMs);
    console.log(
      `perf: ${hosts.length} Macs × ${ROWS} rows, ${PER_WINDOW} lists per Mac per window: commits mean ${commitMean.toFixed(1)} ms, worst ${Math.max(...commitMs).toFixed(1)} ms; ` +
        `windows mean ${meanOf(windows).toFixed(1)} ms, median ${median(windows).toFixed(1)} ms, worst ${Math.max(...windows).toFixed(1)} ms; ` +
        `${commits} commits, ${lists.mock.calls.length} lists reconciled, ${mockDraws.cards} card draws (${onScreen} on screen), inboxOf ${inboxes.mock.calls.length}, drawerList ${drawers.mock.calls.length}, ` +
        `activity shown ${live.shown().length - shownBefore}, entries ${live.of("setEntries").length - entriesBefore}, toasts ${toasts.mock.calls.length}`,
    );

    // The last list of every Mac is what shows.
    const last = (m: number) => octetStore.getState().sessions.find((s) => s.id.endsWith(`/${ids(m)[0]}`));
    for (const m of hosts.keys()) expect(last(m)?.now).toBe(`Bash · cargo test -p octet-hub (${(WARMUP + WINDOWS) * PER_WINDOW})`);

    // At most one commit per window (the coalescer's 50 ms), whatever the Macs send.
    expect(commits).toBeGreaterThan(0);
    expect(commits).toBeLessThanOrEqual(WINDOWS + 1); // the first commits at once, then one per 50 ms
    // Each commit cleans one list per Mac, its last: the lists before it are whole lists it replaces.
    expect(lists.mock.calls.length).toBeLessThanOrEqual(commits * hosts.length);
    // Each commit draws at most the cards on screen whose rows moved, never a card per row; each
    // derivation runs once per commit.
    expect(mockDraws.cards).toBeGreaterThan(0);
    expect(mockDraws.cards).toBeLessThanOrEqual(commits * Math.min(onScreen, hosts.length * MOVING));
    expect(inboxes.mock.calls.length).toBeLessThanOrEqual(commits + 1);
    expect(drawers.mock.calls.length).toBeLessThanOrEqual(commits + 1);
    // No approval came or went: no toast, and the widget's entries are not written again.
    expect(toasts).not.toHaveBeenCalled();
    expect(live.of("setEntries").length).toBe(entriesBefore);
    // The Island changes at most once a second, however fast the lists come.
    expect(live.shown().length - shownBefore).toBeLessThanOrEqual(Math.ceil(streamedMs / 1_000) + 1);
    // And a commit fits between two.
    expect(commitMean).toBeLessThan(COMMIT_BUDGET_MS);
  }, 60_000); // a slow run fails on the budget, with its numbers, not on jest's timeout
});
