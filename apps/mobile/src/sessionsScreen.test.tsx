import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, within } from "@testing-library/react-native";
import { useLocalSearchParams } from "expo-router";
import { renderRouter, screen } from "expo-router/testing-library";
import * as SecureStore from "expo-secure-store";
import { AccessibilityInfo, AppState, StyleSheet, Text } from "react-native";

import RootLayout from "../app/_layout";
import Pair from "../app/pair";
import Sessions from "../app/index";
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY } from "./hub";
import { saveSettings } from "./persist";
import type { SessionInfo } from "./protocol";
import * as words from "./sessions";
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
  phase: "idle",
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

/** Opens the app on Sessions, paired and connected to a Mac that lists `list`. */
async function launch(list: SessionInfo[] | null) {
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
  // renderRouter hangs `getPathname` on the render result, which RNTL 14 returns as a promise.
  const view = renderRouter({
    _layout: Layout,
    index: Sessions,
    pair: Pair,
    "session/[id]": function Session() {
      return <Text testID="opened">{useLocalSearchParams<{ id: string }>().id}</Text>;
    },
    recent: () => <Text>Recent screen</Text>,
    settings: () => <Text>Settings screen</Text>,
  });
  await view;
  const rig = { hub, sockets, clock, view };
  if (list) {
    await screen.findByTestId("sessions-list");
    await hear(rig, list);
  }
  return rig;
}

type Rig = Awaited<ReturnType<typeof launch>>;

/** The Mac welcomes the phone and lists `list`. */
async function hear({ sockets, clock }: Rig, list: SessionInfo[]) {
  await act(async () => {
    if (sockets.last.sent.length === 0) {
      sockets.last.open();
      sockets.last.receive({ type: "welcome", device: "iPhone" });
    }
    sockets.last.receive({ type: "sessions", sessions: list });
    clock.advance(50);
  });
}

const cards = () => screen.getAllByRole("button", { name: /· |Codex|Demo|Claude/ });

describe("Sessions", () => {
  it("shows a card per session: engine and model, folder, the NOW line, the goal", async () => {
    await launch([
      info("octet", {
        model: "sonnet-5",
        phase: "running",
        now: "Running Bash cargo test",
        goal: "Make the hub survive restarts",
      }),
      info("notes", { engine: "codex", phase: "idle" }),
    ]);
    const card = screen.getByRole("button", { name: /^octet, / });
    expect(within(card).getByText("Claude · sonnet-5")).toBeTruthy();
    expect(within(card).getByText("octet")).toBeTruthy(); // the folder, not the whole path
    expect(within(card).getByText("Running Bash cargo test")).toBeTruthy();
    expect(within(card).getByText("Make the hub survive restarts")).toBeTruthy();
    expect(screen.queryByText("/Users/me/Projects/octet")).toBeNull();
    // no NOW line: the phase
    const other = screen.getByRole("button", { name: /^notes, / });
    expect(within(other).getByText("Codex")).toBeTruthy();
    expect(within(other).getByText("Idle")).toBeTruthy();
  });

  it("marks approvals waiting with an amber badge and the count", async () => {
    await launch([info("a", { approvals: 2 }), info("b")]);
    const a = screen.getByRole("button", { name: /^a, / });
    const badge = within(a).getByLabelText("2 approvals waiting");
    expect(within(badge).getByText("2")).toBeTruthy();
    expect([dark.amber, light.amber]).toContain(StyleSheet.flatten(badge.props.style).backgroundColor);
    expect(within(screen.getByRole("button", { name: /^b, / })).queryByLabelText(/approval/)).toBeNull();
  });

  it("says one approval in the singular", async () => {
    await launch([info("a", { approvals: 1 })]);
    expect(screen.getByLabelText("1 approval waiting")).toBeTruthy();
  });

  it("sorts approvals waiting first, then the newest activity", async () => {
    await launch([
      info("old", { last_activity: 100 }),
      info("new", { last_activity: 300 }),
      info("waiting", { approvals: 1, last_activity: 10 }),
    ]);
    expect(cards().map((c) => within(c).getByTestId("folder").children.join(""))).toEqual(["waiting", "new", "old"]);
  });

  it("renders a hub string as plain text", async () => {
    await launch([info("a", { goal: "<b>bold</b> [x](https://evil.example)", now: "Running Bash $(rm -rf /)" })]);
    expect(screen.getByText("<b>bold</b> [x](https://evil.example)")).toBeTruthy();
    expect(screen.getByText("Running Bash $(rm -rf /)")).toBeTruthy();
  });

  it("an empty list says so and offers Recent and new", async () => {
    const { view } = await launch([]);
    expect(screen.getByText("No Octet sessions are running on your Mac")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Recent and new" }));
    expect(view.getPathname()).toBe("/recent");
  });

  it("has a Settings gear in the header that opens Settings, with or without sessions", async () => {
    const { view } = await launch([info("a")]);
    const gear = screen.getByRole("button", { name: "Settings" });
    expect(StyleSheet.flatten(gear.props.style).minHeight).toBeGreaterThanOrEqual(44);
    expect(StyleSheet.flatten(gear.props.style).minWidth).toBeGreaterThanOrEqual(44);
    await fireEvent.press(gear);
    expect(view.getPathname()).toBe("/settings");
    expect(screen.getByText("Settings screen")).toBeTruthy();
  });

  it("tapping a card opens that session", async () => {
    const { view } = await launch([info("a"), info("b")]);
    await fireEvent.press(screen.getByRole("button", { name: /^b, / }));
    expect(view.getPathname()).toBe("/session/b");
  });

  it("opens a session whose id is odd as that session, not as another route", async () => {
    const { view } = await launch([info("a/../b?x=1")]);
    await fireEvent.press(screen.getByRole("button", { name: /Claude/ }));
    expect(view.getSegments()).toEqual(["session", "[id]"]);
    expect(screen.getByTestId("opened").children).toEqual(["a/../b?x=1"]);
  });

  it("offline: a banner over the last list, greyed, and its cards do nothing", async () => {
    const rig = await launch([info("a", { approvals: 1 })]);
    const { view } = rig;
    await act(async () => {
      rig.sockets.last.drop();
    });
    expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
    const card = screen.getByRole("button", { name: /^a, / });
    expect(card.props.accessibilityState).toMatchObject({ disabled: true });
    expect(StyleSheet.flatten(card.props.style).opacity).toBeLessThan(1);
    await fireEvent.press(card);
    expect(view.getPathname()).toBe("/");
    expect(screen.queryByText("No Octet sessions are running on your Mac")).toBeNull();
  });

  it("is usable again when the Mac comes back", async () => {
    const rig = await launch([info("a")]);
    await act(async () => {
      rig.sockets.last.drop();
    });
    await act(async () => {
      rig.clock.advance(60_000);
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    expect(screen.queryByText("Can't reach your Mac")).toBeNull();
    expect(screen.getByRole("button", { name: /^a, / }).props.accessibilityState).toMatchObject({ disabled: false });
  });

  it("pull to refresh says hello again on a fresh connection", async () => {
    const rig = await launch([info("a")]);
    const before = rig.sockets.all.length;
    const first = rig.sockets.last;
    const list = screen.getByTestId("sessions-list");
    await act(async () => {
      list.props.refreshControl.props.onRefresh();
    });
    expect(first.closed).toBe(true);
    expect(rig.sockets.all).toHaveLength(before + 1);
    await act(async () => {
      rig.sockets.last.open();
    });
    expect(rig.sockets.last.commandsOf("hello")).toEqual([expect.objectContaining({ token: TOKEN })]);
  });

  it("shows a spinner while pulling, until the Mac answers", async () => {
    const rig = await launch([info("a")]);
    const list = () => screen.getByTestId("sessions-list");
    expect(list().props.refreshControl.props.refreshing).toBe(false);
    await act(async () => {
      list().props.refreshControl.props.onRefresh();
    });
    expect(list().props.refreshControl.props.refreshing).toBe(true);
    await hear(rig, [info("a")]);
    expect(list().props.refreshControl.props.refreshing).toBe(false);
  });

  it("re-renders only the card whose own data changed", async () => {
    const rig = await launch([info("a"), info("b"), info("c")]);
    const drawn = jest.spyOn(words, "engineLine");
    await hear(rig, [info("a"), info("b", { phase: "running", now: "Thinking" }), info("c")]);
    expect(within(screen.getByRole("button", { name: /^b, / })).getByText("Thinking")).toBeTruthy();
    expect(drawn).toHaveBeenCalledTimes(1);
    drawn.mockClear();
    // the same list again changes nothing on screen
    await hear(rig, [info("a"), info("b", { phase: "running", now: "Thinking" }), info("c")]);
    expect(drawn).not.toHaveBeenCalled();
  });

  it("says it is loading until the Mac sends the list, not that there are none", async () => {
    const rig = await launch(null);
    await screen.findByTestId("sessions-list");
    await act(async () => {
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    expect(screen.getByText("Loading sessions…")).toBeTruthy();
    expect(screen.queryByText("No Octet sessions are running on your Mac")).toBeNull();
    await hear(rig, []);
    expect(screen.queryByText("Loading sessions…")).toBeNull();
    expect(screen.getByText("No Octet sessions are running on your Mac")).toBeTruthy();
  });

  it("the offline banner is a polite live region and is announced when its words change", async () => {
    const announce = jest.spyOn(AccessibilityInfo, "announceForAccessibility");
    const rig = await launch([info("a")]);
    announce.mockClear();
    await act(async () => {
      rig.sockets.last.drop();
    });
    expect(screen.getByLabelText("Can't reach your Mac").props.accessibilityLiveRegion).toBe("polite");
    expect(announce).toHaveBeenCalledWith("Can't reach your Mac");
    expect(announce).toHaveBeenCalledTimes(1);
    await act(async () => {
      rig.clock.advance(60_000); // the reconnect timer opens a new socket
    });
    expect(announce).toHaveBeenLastCalledWith("Connecting to your Mac…");
  });

  it("the refresh spinner stops once the Mac answers and does not come back on a later drop", async () => {
    const rig = await launch([info("a")]);
    const list = () => screen.getByTestId("sessions-list");
    await act(async () => {
      list().props.refreshControl.props.onRefresh();
    });
    await hear(rig, [info("a")]);
    expect(list().props.refreshControl.props.refreshing).toBe(false);
    await act(async () => {
      rig.sockets.last.drop();
    });
    expect(list().props.refreshControl.props.refreshing).toBe(false);
  });

  it("names a session with a blank workspace Session", async () => {
    await launch([info("x", { workspace: "" }), info("y", { workspace: "/" })]);
    expect(screen.getAllByRole("button", { name: /^Session, Claude/ })).toHaveLength(2);
    expect(screen.getAllByText("Session")).toHaveLength(2);
  });

  it("clips the goal and NOW line in a card's label, and shows them whole", async () => {
    const goal = "g".repeat(500);
    const now = `Running Bash ${"n".repeat(500)}`;
    await launch([info("a", { goal, now, phase: "running" })]);
    const card = screen.getByRole("button", { name: /^a, / });
    const parts = String(card.props.accessibilityLabel).split(", ");
    expect(parts.every((p) => p.length <= 120)).toBe(true);
    expect(parts.some((p) => p.startsWith("ggg") && p.endsWith("…"))).toBe(true);
    expect(parts.some((p) => p.startsWith("Running Bash nnn") && p.endsWith("…"))).toBe(true);
    expect(within(card).getByText(goal)).toBeTruthy();
  });

  describe("the header mascot", () => {
    const mascot = () => screen.getByRole("image");
    it("sleeps while the Mac cannot be reached", async () => {
      const rig = await launch([info("a", { approvals: 1 })]);
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(mascot().props.accessibilityLabel).toBe("Octet, sleeping");
    });
    it("shows an approval waiting, over running work", async () => {
      await launch([info("a", { phase: "running", now: "Running Edit x" }), info("b", { approvals: 1 })]);
      expect(mascot().props.accessibilityLabel).toBe("Octet, approval");
    });
    it("follows the running session's NOW line", async () => {
      await launch([info("a", { phase: "running", now: "Running Grep needle" })]);
      expect(mascot().props.accessibilityLabel).toBe("Octet, searching");
    });
    it("is idle when nothing runs, and is 59 wide", async () => {
      await launch([info("a")]);
      expect(mascot().props.accessibilityLabel).toBe("Octet, idle");
      expect(mascot().props.width).toBe(59);
    });
  });
});
