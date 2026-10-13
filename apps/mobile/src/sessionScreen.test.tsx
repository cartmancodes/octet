import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, within } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { router } from "expo-router";
import { renderRouter, screen } from "expo-router/testing-library";
import * as SecureStore from "expo-secure-store";
import { AppState, ScrollView, StyleSheet } from "react-native";

import RootLayout from "../app/_layout";
import Pair from "../app/pair";
import Sessions from "../app/index";
import Session from "../app/session/[id]";
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY } from "./hub";
import * as parser from "./markdown/parse";
import { saveSettings } from "./persist";
import type { JournalEvent, SessionInfo, StateInfo, ToolData } from "./protocol";
import * as words from "./session";
import * as cardWords from "./sessions";
import { octetStore } from "./store";
import { dark, light } from "./theme";

jest.mock("expo-camera", () => ({
  CameraView: () => null,
  useCameraPermissions: () => [{ granted: false, canAskAgain: false, status: "denied" }, jest.fn()],
}));

const TOKEN = "cd".repeat(32);

const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/Users/me/Projects/${id}`,
  mode: "ask",
  phase: "running",
  approvals: 0,
  from_phone: false,
  ...extra,
});

beforeEach(async () => {
  await AsyncStorage.clear();
  await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
  await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
  octetStore.getState().reset();
  jest.spyOn(AppState, "addEventListener").mockImplementation(() => ({ remove: jest.fn() }));
});
afterEach(() => jest.restoreAllMocks());

let seq = 0;

/** Opens the app connected to a Mac that lists `list`; `at` deep-links straight to a screen. */
async function launch(list: SessionInfo[], at?: string) {
  seq = 0;
  const sockets = new Sockets();
  const clock = new FakeClock();
  const hub = new HubController({
    factory: sockets.factory,
    store: octetStore,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
  });
  const Layout = () => (
    <HubContext.Provider value={hub}>
      <RootLayout />
    </HubContext.Provider>
  );
  const view = renderRouter(
    { _layout: Layout, index: Sessions, pair: Pair, "session/[id]": Session, recent: () => null, settings: () => null },
    at ? { initialUrl: at } : undefined,
  );
  await view;
  await act(async () => {
    sockets.last.open();
    sockets.last.receive({ type: "welcome", device: "iPhone" });
    sockets.last.receive({ type: "sessions", sessions: list });
    clock.advance(50);
  });
  return { hub, sockets, clock, view };
}
type Rig = Awaited<ReturnType<typeof launch>>;

/** The hub sends `frames`, then a batch is delivered. */
async function feed({ sockets, clock }: Rig, ...frames: object[]) {
  await act(async () => {
    for (const frame of frames) sockets.last.receive(frame);
    clock.advance(50);
  });
}

const ev = (event: JournalEvent, session = "a") => ({ type: "event", session, journal: "j1", seq: ++seq, event });
const subscribed = (session = "a") => ({ type: "subscribed", session, from: "start", journals: ["j1"] });
const tool = (id: string, name: string, target: string, phase: ToolData["phase"], detail = "") =>
  ev({ type: "tool", data: { id, name, target, phase, detail } });
const stateFrame = (state: Partial<StateInfo>, session = "a") => ({
  type: "state",
  session,
  state: { phase: "running", queue: [], queued: 0, mode: "ask", approvals: [], ...state },
});

/** Taps the card of `id` on Sessions and waits for its screen. */
async function open(rig: Rig, id: string) {
  await fireEvent.press(screen.getByRole("button", { name: new RegExp(`^${id}, `) }));
  await screen.findByTestId("transcript");
}

const color = (node: { props: { style?: unknown } }): unknown =>
  (StyleSheet.flatten(node.props.style as never) as { color?: unknown } | undefined)?.color;

describe("the Session screen", () => {
  it("streams the session while shown, and stops when the user leaves", async () => {
    const rig = await launch([info("a")]);
    await open(rig, "a");
    expect(rig.sockets.last.commandsOf("subscribe")).toEqual([expect.objectContaining({ session: "a" })]);
    expect(rig.sockets.last.commandsOf("subscribe")[0]).not.toHaveProperty("after"); // fresh: nothing held
    await act(async () => router.back());
    expect(rig.sockets.last.commandsOf("unsubscribe")).toEqual([expect.objectContaining({ session: "a" })]);
  });

  it("shows the prompt, the reply, and notes and errors", async () => {
    const rig = await launch([info("a")]);
    await open(rig, "a");
    await feed(
      rig,
      subscribed(),
      ev({ type: "user", data: "Fix the hub" }),
      ev({ type: "text", data: "On it." }),
      ev({ type: "notice", data: "Model changed" }),
      ev({ type: "error", data: "Rate limited" }),
    );
    expect(screen.getByText("Fix the hub")).toBeTruthy();
    expect(screen.getByText("On it.")).toBeTruthy();
    expect(screen.getByText("Model changed")).toBeTruthy();
    expect([dark.amber, light.amber]).toContain(color(screen.getByText("Rate limited")));
  });

  describe("grouped tools", () => {
    it("are one collapsed row in words, and open with a tap", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(
        rig,
        subscribed(),
        tool("1", "Read", "a.rs", "done"),
        tool("2", "Read", "b.rs", "done"),
        tool("3", "Read", "c.rs", "done"),
        tool("4", "commandExecution", "cargo build", "done", "Compiling\nFinished"),
      );
      const row = screen.getByRole("button", { name: /Read 3 files · Ran 1 command/ });
      expect(within(row).getByText("▸ Read 3 files · Ran 1 command")).toBeTruthy();
      expect(screen.queryByText("✓ Read a.rs")).toBeNull();
      expect(row.props.accessibilityState).toMatchObject({ expanded: false });
      await fireEvent.press(row);
      expect(screen.getByText("✓ Read a.rs")).toBeTruthy();
      expect(screen.getByText("✓ Bash cargo build")).toBeTruthy();
      expect(StyleSheet.flatten(screen.getByText("Compiling\nFinished").props.style).fontFamily).toBe("Menlo");
      expect(screen.getByRole("button", { name: /Read 3 files/ }).props.accessibilityState).toMatchObject({ expanded: true });
      await fireEvent.press(screen.getByRole("button", { name: /Read 3 files/ }));
      expect(screen.queryByText("✓ Read a.rs")).toBeNull();
    });

    it("a fresh replay closes the groups the user opened", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), tool("1", "Read", "a.rs", "done"));
      await fireEvent.press(screen.getByRole("button", { name: /Read 1 file/ }));
      expect(screen.getByText("✓ Read a.rs")).toBeTruthy();
      await feed(rig, subscribed(), tool("1", "Read", "a.rs", "done"));
      expect(screen.queryByText("✓ Read a.rs")).toBeNull();
      expect(screen.getByText("▸ Read 1 file")).toBeTruthy();
    });

    it("show a running call with its target", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), tool("1", "Read", "a.rs", "done"), tool("2", "Bash", "cargo test", "running"));
      expect(screen.getByText("▸ Read 1 file")).toBeTruthy();
      expect(screen.getByText("… Bash cargo test")).toBeTruthy();
    });

    it("say stopped for a call its turn ended under", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), tool("2", "Bash", "cargo test", "running"), ev({ type: "stopped", data: null }));
      expect(screen.getByText("… Bash cargo test · stopped")).toBeTruthy();
    });

    it("open a failed call in amber, with its last 8 lines and a count of the rest", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      const out = Array.from({ length: 12 }, (_, i) => `line ${i + 1}`).join("\n");
      await feed(rig, subscribed(), tool("1", "Read", "a.rs", "done"), tool("2", "Bash", "cargo test", "failed", out));
      const head = screen.getByText("✗ Bash cargo test");
      expect([dark.amber, light.amber]).toContain(color(head));
      expect(screen.getByText(Array.from({ length: 8 }, (_, i) => `line ${i + 5}`).join("\n"))).toBeTruthy();
      expect(screen.queryByText(/line 4\b/)).toBeNull();
      expect(screen.getByText("+4 more lines")).toBeTruthy();
    });
  });

  it("draws a reply as plain text while it streams, and as Markdown once done", async () => {
    const rig = await launch([info("a")]);
    await open(rig, "a");
    const parse = jest.spyOn(parser, "parse");
    await feed(rig, subscribed(), ev({ type: "text", data: "Use **bold** and " }), ev({ type: "text", data: "`code`" }));
    expect(screen.getByText("Use **bold** and `code`")).toBeTruthy();
    expect(parse).not.toHaveBeenCalled();
    await feed(rig, ev({ type: "finished", data: "" }));
    expect(screen.queryByText("Use **bold** and `code`")).toBeNull();
    expect(StyleSheet.flatten(screen.getByText("bold").props.style).fontWeight).toBe("700");
    expect(parse).toHaveBeenCalled();
  });

  it("draws a streaming reply in paragraphs, so a chunk lays out only the last", async () => {
    const rig = await launch([info("a")]);
    await open(rig, "a");
    await feed(rig, subscribed(), ev({ type: "text", data: "First paragraph.\n\nSecond" }));
    // Each paragraph is its own <Text>: the one a chunk grows is the only one with new text.
    expect(screen.getByText("First paragraph.")).toBeTruthy();
    await feed(rig, ev({ type: "text", data: " grows" }));
    expect(screen.getByText("Second grows")).toBeTruthy();
    expect(screen.getByText("First paragraph.")).toBeTruthy();
  });

  describe("following the output", () => {
    /** The native scroll view's scrollToEnd, which FlashList's calls in the end. React Native's
     * mock makes it one jest.fn shared by every instance and test, so it starts cleared. */
    const scrollToEnd = () => {
      const spy = jest.spyOn(ScrollView.prototype, "scrollToEnd");
      spy.mockClear();
      return spy;
    };
    /** The transcript scrolled to `y` of `height`, in a window 900 high. */
    const scrolled = async (y: number, height = 3_000) => {
      const list = screen.getByTestId("transcript");
      await act(async () => {
        list.props.onScrollBeginDrag?.({ nativeEvent: { contentOffset: { x: 0, y } } });
        list.props.onScroll({
          nativeEvent: { contentOffset: { x: 0, y }, contentSize: { width: 400, height }, layoutMeasurement: { width: 400, height: 900 } },
        });
        list.props.onScrollEndDrag?.({ nativeEvent: { contentOffset: { x: 0, y } } });
      });
    };
    /** The content grew, as a layout would say. */
    const grew = async (height: number) => {
      const list = screen.getByTestId("transcript");
      await act(async () => {
        list.props.onContentSizeChange(400, height);
        jest.runOnlyPendingTimers();
      });
    };

    it("sticks to the bottom while new output streams in", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "user", data: "go" }), ev({ type: "text", data: "one" }));
      const spy = scrollToEnd();
      await feed(rig, ev({ type: "text", data: " two" }));
      await grew(3_000);
      expect(spy).toHaveBeenCalled();
      expect(screen.queryByRole("button", { name: "New output below" })).toBeNull();
    });

    it("stays put when the user scrolled up, offers New output below, and the pill scrolls down", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "user", data: "go" }), ev({ type: "text", data: "one" }));
      await scrolled(2_100);
      await scrolled(400); // the user dragged up
      const spy = scrollToEnd();
      await feed(rig, ev({ type: "text", data: " two" }));
      await grew(3_200);
      expect(spy).not.toHaveBeenCalled();
      const pill = screen.getByRole("button", { name: "New output below" });
      await fireEvent.press(pill);
      await act(async () => {
        jest.runOnlyPendingTimers();
      });
      expect(spy).toHaveBeenCalled();
      expect(screen.queryByRole("button", { name: "New output below" })).toBeNull();
    });

    it("scrolled up, content that only grew (a group opened, sizes measured) shows no pill", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "user", data: "go" }), tool("1", "Read", "a.rs", "done"));
      await scrolled(400);
      const spy = scrollToEnd();
      await fireEvent.press(screen.getByRole("button", { name: /Read 1 file/ }));
      await grew(3_600);
      expect(screen.queryByRole("button", { name: "New output below" })).toBeNull();
      expect(spy).not.toHaveBeenCalled();
      await feed(rig, ev({ type: "text", data: "new" }));
      expect(screen.getByRole("button", { name: "New output below" })).toBeTruthy();
    });

    it("follows again once the user scrolls back to the bottom", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "text", data: "one" }));
      await scrolled(400);
      await feed(rig, ev({ type: "text", data: " two" }));
      expect(screen.getByRole("button", { name: "New output below" })).toBeTruthy();
      await scrolled(2_100);
      expect(screen.queryByRole("button", { name: "New output below" })).toBeNull();
      const spy = scrollToEnd();
      await feed(rig, ev({ type: "text", data: " three" }));
      await grew(3_300);
      expect(spy).toHaveBeenCalled();
    });
  });

  describe("the Task strip", () => {
    it("shows the goal and its turns, NOW, and the queue's first lines and count, and collapses", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(
        rig,
        subscribed(),
        stateFrame({
          goal: { objective: "Make the hub survive restarts", status: "active", turns: 3, max: 20 },
          now: "Running Bash cargo test",
          queue: ["Then update the docs\nwith the new flags", "Run the gate", "Commit", "Push"],
          queued: 5,
        }),
      );
      const strip = screen.getByTestId("task-strip");
      // Folded at first: one line, NOW and the count.
      expect(within(strip).getByText("Running Bash cargo test · 5 queued")).toBeTruthy();
      expect(within(strip).queryByText("Make the hub survive restarts")).toBeNull();
      const toggle = within(strip).getByRole("button", { name: /^Task/ });
      expect(toggle.props.accessibilityLabel).toMatch(/5 queued/);
      expect(toggle.props.accessibilityState).toMatchObject({ expanded: false });
      await fireEvent.press(toggle);
      expect(within(strip).getByText("Make the hub survive restarts")).toBeTruthy();
      expect(within(strip).getByText("active · 3/20 turns")).toBeTruthy();
      expect(within(strip).getByText("Running Bash cargo test")).toBeTruthy();
      expect(within(strip).getByText("Then update the docs")).toBeTruthy();
      expect(within(strip).queryByText(/with the new flags/)).toBeNull();
      expect(within(strip).getByText("Run the gate")).toBeTruthy();
      expect(within(strip).getByText("Commit")).toBeTruthy();
      expect(within(strip).queryByText("Push")).toBeNull();
      expect(within(strip).getByText("+2 more")).toBeTruthy();
      expect(within(strip).getByRole("button", { name: /^Task/ }).props.accessibilityState).toMatchObject({ expanded: true });
      await fireEvent.press(within(strip).getByRole("button", { name: /^Task/ }));
      expect(within(strip).queryByText("Make the hub survive restarts")).toBeNull();
      expect(within(strip).queryByText("Run the gate")).toBeNull();
      expect(within(strip).getByText("Running Bash cargo test · 5 queued")).toBeTruthy();
    });

    it("remembers being opened for the next session's screen", async () => {
      const rig = await launch([info("a"), info("b")]);
      await open(rig, "a");
      await fireEvent.press(within(screen.getByTestId("task-strip")).getByRole("button", { name: /^Task/ }));
      await act(async () => router.back());
      await open(rig, "b");
      const strips = screen.getAllByTestId("task-strip");
      const strip = strips[strips.length - 1];
      if (!strip) throw new Error("no strip");
      expect(within(strip).getByRole("button", { name: /^Task/ }).props.accessibilityState).toMatchObject({ expanded: true });
      expect(within(strip).getByText("GOAL")).toBeTruthy();
    });

    it("says when there is no goal and nothing queued, and falls back to the phase", async () => {
      const rig = await launch([info("a", { phase: "idle" })]);
      await open(rig, "a");
      await feed(rig, subscribed(), stateFrame({ phase: "idle" }));
      const strip = screen.getByTestId("task-strip");
      await fireEvent.press(within(strip).getByRole("button", { name: /^Task/ }));
      expect(within(strip).getByText("No goal")).toBeTruthy();
      expect(within(strip).getByText("Idle")).toBeTruthy();
      expect(within(strip).getByText("Nothing queued")).toBeTruthy();
    });
  });

  describe("states", () => {
    it("an ended session says so and keeps its transcript, and leaving drops it", async () => {
      const rig = await launch([info("a"), info("b")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "user", data: "Fix the hub" }));
      await feed(rig, { type: "sessions", sessions: [info("b")] });
      expect(screen.getByText("This session ended")).toBeTruthy();
      expect(screen.getByText("Fix the hub")).toBeTruthy();
      expect(octetStore.getState().bySession.has("a")).toBe(true);
      await act(async () => router.back());
      expect(octetStore.getState().bySession.has("a")).toBe(false);
    });

    it("a session the hub refused to stream shows the reason", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      const [command] = rig.sockets.last.commandsOf("subscribe");
      await feed(rig, { type: "refused", req: command?.req, reason: "That session is not available", code: "unavailable" });
      expect(screen.getByText("That session is not available")).toBeTruthy();
    });

    it("a session the phone knows nothing of and the Mac does not list is gone, with a way back", async () => {
      const rig = await launch([info("a")], "/session/zzz");
      expect(await screen.findByText("This session isn't running anymore")).toBeTruthy();
      await fireEvent.press(screen.getByRole("button", { name: "Back to sessions" }));
      expect(rig.view.getPathname()).toBe("/");
    });

    it("a deep-linked session the hub refuses is gone too, with a way back, and its view is dropped on leaving", async () => {
      const rig = await launch([info("a")], "/session/zzz");
      const [command] = rig.sockets.last.commandsOf("subscribe");
      expect(command).toMatchObject({ session: "zzz" });
      await feed(rig, { type: "refused", req: command?.req, reason: "No such session", code: "invalid" });
      expect(octetStore.getState().bySession.get("zzz")?.failed).toBe("No such session");
      expect(screen.getByText("This session isn't running anymore")).toBeTruthy();
      expect(screen.queryByTestId("transcript")).toBeNull();
      await fireEvent.press(screen.getByRole("button", { name: "Back to sessions" }));
      expect(rig.view.getPathname()).toBe("/");
      expect(octetStore.getState().bySession.has("zzz")).toBe(false);
    });

    it("offline, the transcript stays under a banner", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "user", data: "Fix the hub" }));
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
      expect(screen.getByText("Fix the hub")).toBeTruthy();
    });

    it("a fresh replay keeps the old transcript up until its first page arrives", async () => {
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), ev({ type: "user", data: "before" }));
      await feed(rig, subscribed());
      expect(screen.getByText("before")).toBeTruthy();
      await feed(rig, ev({ type: "user", data: "after" }));
      expect(screen.queryByText("before")).toBeNull();
      expect(screen.getByText("after")).toBeTruthy();
    });
  });

  it("100 text deltas re-render only the streaming row, once per batch, and no Sessions card", async () => {
    const rig = await launch([info("a"), info("b"), info("c")]);
    await open(rig, "a");
    await feed(
      rig,
      subscribed(),
      ev({ type: "user", data: "go" }),
      tool("1", "Read", "a.rs", "done"),
      ev({ type: "notice", data: "note" }),
      ev({ type: "text", data: "start" }),
    );
    const rows = jest.spyOn(words, "rowRole");
    const cards = jest.spyOn(cardWords, "engineLine");
    const batches = 10;
    for (let b = 0; b < batches; b++) {
      await act(async () => {
        for (let i = 0; i < 100 / batches; i++) rig.sockets.last.receive(ev({ type: "text", data: ` ${b * 10 + i}` }));
        rig.clock.advance(50);
      });
    }
    expect(screen.getByText(/start 0 1 2 .* 99$/)).toBeTruthy();
    expect(rows.mock.calls.length).toBeGreaterThan(0);
    expect(rows.mock.calls.length).toBeLessThanOrEqual(100 / batches + 2);
    expect(cards).not.toHaveBeenCalled();
    // Another session's output, straight into the store: nothing on this screen draws again.
    rows.mockClear();
    await act(async () => {
      octetStore.getState().applyBatch(
        Array.from({ length: 50 }, (_, i) => ({ v: 1, ...ev({ type: "text", data: `b${i}` }, "b") }) as never),
      );
    });
    expect(octetStore.getState().bySession.get("b")?.transcript.entries).toHaveLength(1);
    expect(rows).not.toHaveBeenCalled();
  });

  describe("approvals, the composer and the controls", () => {
    const approvalEvent = (id: number, action: string) =>
      ev({ type: "approval", data: { id, detail: `full ${id}`, summary: { action, command: "make test", files: [], cwd: "/w", scope: "once" } } });
    const lastCommand = (rig: Rig, type: string) => rig.sockets.last.commandsOf(type).at(-1);
    // renderRouter runs on fake timers.
    const wait = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));

    it("a live session has the composer and, idle, End; a running one has Cancel", async () => {
      const rig = await launch([info("a", { phase: "idle" })]);
      await open(rig, "a");
      await feed(rig, subscribed(), stateFrame({ phase: "idle" }));
      expect(screen.getByLabelText("Message")).toBeTruthy();
      expect(screen.getByRole("button", { name: "End session" })).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
      await feed(rig, stateFrame({ phase: "running", queued: 2 }));
      expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
      expect(screen.getByText("Queue · 2 queued")).toBeTruthy();
    });

    it("sends a prompt over the wire and clears the box when the Mac acks", async () => {
      const rig = await launch([info("a", { phase: "idle" })]);
      await open(rig, "a");
      await feed(rig, subscribed(), stateFrame({ phase: "idle" }));
      await fireEvent.changeText(screen.getByLabelText("Message"), "fix the hub");
      await fireEvent.press(screen.getByRole("button", { name: "Send" }));
      const sent = lastCommand(rig, "prompt");
      expect(sent).toMatchObject({ session: "a", text: "fix the hub" });
      expect(screen.getByLabelText("Message").props.value).toBe("fix the hub");
      await feed(rig, { type: "ack", req: sent?.req, message: "sent" });
      expect(screen.getByLabelText("Message").props.value).toBe("");
    });

    it("shows an approval, answers it over the wire, and drops one closed elsewhere", async () => {
      await saveSettings({ faceId: false });
      const rig = await launch([info("a")]);
      await open(rig, "a");
      await feed(rig, subscribed(), approvalEvent(1, "Run tests"), approvalEvent(2, "Edit config"));
      expect(screen.getByText("Run tests")).toBeTruthy();
      expect(screen.getByText("$ make test")).toBeTruthy();
      await wait(450);
      await fireEvent.press(screen.getByRole("button", { name: "Allow" }));
      await act(async () => {});
      const answer = lastCommand(rig, "answer");
      expect(answer).toMatchObject({ session: "a", approval: 1, allow: true });
      await feed(rig, { type: "ack", req: answer?.req, message: "ok" });
      expect(screen.getByText("Edit config")).toBeTruthy();
      await feed(rig, ev({ type: "approval_closed", data: 2 }));
      expect(screen.queryByText("Edit config")).toBeNull();
      expect(screen.queryByRole("button", { name: "Allow" })).toBeNull();
    });

    it("hides the composer, the controls and the approvals once the session has ended", async () => {
      const rig = await launch([info("a", { phase: "idle" }), info("b")]);
      await open(rig, "a");
      await feed(rig, subscribed(), stateFrame({ phase: "idle" }), approvalEvent(1, "Run tests"));
      expect(screen.getByText("Run tests")).toBeTruthy();
      await feed(rig, { type: "sessions", sessions: [info("b")] });
      expect(screen.getByText("This session ended")).toBeTruthy();
      expect(screen.queryByLabelText("Message")).toBeNull();
      expect(screen.queryByRole("button", { name: "End session" })).toBeNull();
      expect(screen.queryByText("Run tests")).toBeNull();
      expect(screen.queryByRole("button", { name: "Allow" })).toBeNull();
    });

    it("changes the mode over the wire, and the catalog moves the current mark", async () => {
      const rig = await launch([info("a", { phase: "idle" })]);
      await open(rig, "a");
      const catalog = (current_mode: string) => ({
        type: "catalog",
        session: "a",
        catalog: { modes: ["ask", "auto"], current_mode, models: [], current: null },
      });
      expect(screen.queryByRole("button", { name: "Mode and model" })).toBeNull();
      await feed(rig, subscribed(), stateFrame({ phase: "idle" }), catalog("ask"));
      await fireEvent.press(screen.getByRole("button", { name: "Mode and model" }));
      await fireEvent.press(screen.getByRole("button", { name: "Auto" }));
      await act(async () => {});
      const sent = lastCommand(rig, "set_mode");
      expect(sent).toMatchObject({ session: "a", mode: "auto" });
      await feed(rig, { type: "ack", req: sent?.req, message: "ok" });
      expect(screen.queryByRole("button", { name: "Auto" })).toBeNull(); // the sheet closed
      await feed(rig, catalog("auto"));
      await fireEvent.press(screen.getByRole("button", { name: "Mode and model" }));
      expect(screen.getByRole("button", { name: "Auto, current" })).toBeTruthy();
      // An ended session takes no commands: the button goes with the controls.
      await feed(rig, { type: "sessions", sessions: [] });
      expect(screen.queryByRole("button", { name: "Mode and model" })).toBeNull();
    });

    it("has none of them for a session that is gone", async () => {
      await launch([info("a")], "/session/zzz");
      expect(await screen.findByText("This session isn't running anymore")).toBeTruthy();
      expect(screen.queryByLabelText("Message")).toBeNull();
    });

    it("sends no Allow when the session ended while Face ID was up", async () => {
      await saveSettings({ faceId: true });
      let pass: () => void = () => {};
      jest.mocked(LocalAuthentication.authenticateAsync).mockImplementationOnce(
        () => new Promise((resolve) => void (pass = () => resolve({ success: true }))),
      );
      const rig = await launch([info("a"), info("b")]);
      await open(rig, "a");
      await feed(rig, subscribed(), approvalEvent(1, "Run tests"));
      await wait(450);
      await fireEvent.press(screen.getByRole("button", { name: "Allow" }));
      await act(async () => {});
      await feed(rig, { type: "sessions", sessions: [info("b")] });
      expect(screen.getByText("This session ended")).toBeTruthy();
      await act(async () => pass());
      expect(rig.sockets.last.commandsOf("answer")).toEqual([]);
    });
  });
});
