import { act, fireEvent, screen, within } from "@testing-library/react-native";
import { router } from "expo-router";
import * as SecureStore from "expo-secure-store";
import { AccessibilityInfo, Animated, BackHandler, StyleSheet, Text } from "react-native";

import { DRAWER_MS, DRAWER_WIDTH, EDGE_WIDTH } from "./components/Drawer";
import { closeDrawer, drawerStore, openDrawer } from "./drawer";
import { hear, info, launch, mount, pairedStorage, type Rig } from "./screenRig";
import type { ApprovalInfo, SessionInfo } from "./protocol";
import { hideToast, showToast } from "./toast";
import { TOKEN_KEY } from "./hub";
import { dark, light } from "./theme";

const gist = (id: number): ApprovalInfo => ({
  id,
  summary: { action: "Bash", command: `make test-${id}`, files: [], scope: "this command once", cwd: "/w" },
});
const waiting = (id: string, approval: ApprovalInfo, extra: Partial<SessionInfo> = {}): SessionInfo =>
  info(id, { approvals: 1, phase: "running", approval, ...extra });

const ROUTES = {
  settings: () => <Text>Settings screen</Text>,
  recent: () => <Text>Recent screen</Text>,
};

beforeEach(async () => {
  await pairedStorage();
  closeDrawer();
});
afterEach(() => {
  closeDrawer();
  hideToast();
  jest.restoreAllMocks();
});

const open = (list: SessionInfo[]) => launch("/", ROUTES, list);
// renderRouter runs on fake timers.
const wait = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));
const drawer = () => screen.getByTestId("drawer");
const inDrawer = () => within(drawer());
const menu = () => screen.getByRole("button", { name: "Menu" });
const scrim = () => screen.getByRole("button", { name: "Close menu" });
const isOpen = () => screen.queryByTestId("drawer") !== null;
const style = (node: { props: { style?: unknown } }) => StyleSheet.flatten(node.props.style as never) as Record<string, unknown>;

async function openFromMenu() {
  await fireEvent.press(menu());
  await wait(DRAWER_MS + 20);
}

/** The Conversation has no ☰ yet: its drawer opens from the edge, or here. */
async function openFromStore() {
  await act(async () => openDrawer());
  await wait(DRAWER_MS + 20);
}

/** A finger on the screen: each `at` is a touch event `x`, `y` points from where it came down. */
function finger() {
  let t = 1_000;
  let last = { x: 0, y: 0 };
  return {
    at(x: number, y: number) {
      const previous = last;
      last = { x, y };
      t += 16;
      return {
        nativeEvent: { touches: [{ pageX: x, pageY: y }] },
        touchHistory: {
          indexOfSingleActiveTouch: 0,
          mostRecentTimeStamp: t,
          numberActiveTouches: 1,
          touchBank: [
            {
              touchActive: true,
              startPageX: 0,
              startPageY: 0,
              startTimeStamp: 1_000,
              currentPageX: x,
              currentPageY: y,
              currentTimeStamp: t,
              previousPageX: previous.x,
              previousPageY: previous.y,
              previousTimeStamp: t - 16,
            },
          ],
        },
      };
    },
  };
}

/** A drag on the edge zone: down at its left, then through `points`, then up. */
async function dragEdge(points: [number, number][]) {
  const zone = screen.getByTestId("drawer-edge");
  const f = finger();
  await fireEvent(zone, "responderGrant", f.at(0, 0));
  for (const [x, y] of points) await fireEvent(zone, "responderMove", f.at(x, y));
  const [x, y] = points[points.length - 1] ?? [0, 0];
  await fireEvent(zone, "responderRelease", f.at(x, y));
}

/** A drag on the open drawer, as the responder system plays it: the panel is asked to take a move
 * (before its children), then is granted the touch, then gets the moves. Returns whether it took it. */
async function dragPanel(points: [number, number][]): Promise<boolean> {
  const panel = drawer();
  const f = finger();
  const [first, ...rest] = points;
  if (!first) return false;
  let took = false;
  await act(async () => {
    panel.props.onStartShouldSetResponderCapture(f.at(0, 0));
  });
  await act(async () => {
    took = panel.props.onMoveShouldSetResponderCapture(f.at(first[0], first[1])) === true;
  });
  if (!took) return false;
  await act(async () => {
    panel.props.onResponderGrant(f.at(first[0], first[1]));
  });
  for (const [x, y] of rest) {
    await act(async () => {
      panel.props.onResponderMove(f.at(x, y));
    });
  }
  await act(async () => {
    panel.props.onResponderRelease(f.at(0, 0));
  });
  return true;
}

describe("the drawer", () => {
  describe("opening and closing", () => {
    it("opens from the menu button and closes on a tap on the scrim", async () => {
      await open([info("a")]);
      expect(isOpen()).toBe(false);
      await openFromMenu();
      expect(isOpen()).toBe(true);
      expect(drawerStore.getState().open).toBe(true);
      await fireEvent.press(scrim());
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
    });

    it("is 304 pt wide over a scrim of 0.6 opacity", async () => {
      await open([info("a")]);
      await openFromMenu();
      expect(DRAWER_WIDTH).toBe(304);
      expect(Number(style(drawer()).width)).toBe(304);
      expect(Number(style(screen.getByTestId("drawer-scrim")).opacity)).toBeCloseTo(0.6, 5);
    });

    it("slides in over 180 ms", async () => {
      const timing = jest.spyOn(Animated, "timing");
      await open([info("a")]);
      await fireEvent.press(menu());
      await wait(DRAWER_MS + 20);
      expect(timing).toHaveBeenCalledWith(expect.anything(), expect.objectContaining({ toValue: 1, duration: 180 }));
      await fireEvent.press(scrim());
      expect(timing).toHaveBeenLastCalledWith(expect.anything(), expect.objectContaining({ toValue: 0, duration: 180 }));
    });

    it("opens and closes instantly with reduced motion", async () => {
      jest.spyOn(AccessibilityInfo, "isReduceMotionEnabled").mockResolvedValue(true);
      const timing = jest.spyOn(Animated, "timing");
      await open([info("a")]);
      await fireEvent.press(menu());
      expect(timing).toHaveBeenLastCalledWith(expect.anything(), expect.objectContaining({ toValue: 1, duration: 0 }));
      await fireEvent.press(scrim());
      expect(timing).toHaveBeenLastCalledWith(expect.anything(), expect.objectContaining({ toValue: 0, duration: 0 }));
      expect(isOpen()).toBe(false);
    });

    it("closes on Android back, before back goes anywhere else", async () => {
      const handlers: ((event: never) => boolean | null | undefined)[] = [];
      jest.spyOn(BackHandler, "addEventListener").mockImplementation((_name, handler) => {
        handlers.push(handler);
        return { remove: () => void handlers.splice(handlers.indexOf(handler), 1) };
      });
      await open([info("a")]);
      const before = handlers.length;
      await openFromMenu();
      expect(handlers).toHaveLength(before + 1);
      // the latest listener is asked first
      let handled: boolean | null | undefined;
      await act(async () => {
        handled = handlers[handlers.length - 1]?.(undefined as never);
      });
      expect(handled).toBe(true);
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
      expect(handlers).toHaveLength(before);
    });

    it("takes a toast down when it opens", async () => {
      await open([info("a")]);
      await act(async () => showToast("Copied"));
      expect(screen.getByTestId("toast")).toBeTruthy();
      await fireEvent.press(menu());
      expect(screen.queryByTestId("toast")).toBeNull();
    });
  });

  describe("contents", () => {
    it("says Octet, how many need you and run, and counts the needs on the Inbox row", async () => {
      await open([
        waiting("a", gist(1), { last_activity: 30 }),
        waiting("b", gist(2), { last_activity: 20 }),
        info("c", { phase: "running", last_activity: 10 }),
        info("d"),
      ]);
      await openFromMenu();
      expect(inDrawer().getByText("Octet")).toBeTruthy();
      // a and b wait with a turn open too; "running" counts the sessions that run with nothing waiting.
      expect(inDrawer().getByText("2 need you · 1 running")).toBeTruthy();
      const inbox = inDrawer().getByRole("button", { name: /^Inbox/ });
      expect(within(inbox).getByText("2")).toBeTruthy();
    });

    it("says just how many run, and shows no badge, when nothing needs you", async () => {
      await open([info("c", { phase: "running" })]);
      await openFromMenu();
      expect(inDrawer().getByText("1 running")).toBeTruthy();
      expect(within(inDrawer().getByRole("button", { name: /^Inbox/ })).queryByTestId("drawer-badge")).toBeNull();
    });

    it("labels the Mac online, then offline when the link drops", async () => {
      const rig = await open([info("a")]);
      await openFromMenu();
      expect(inDrawer().getByText("● mac · online")).toBeTruthy();
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(inDrawer().getByText("○ mac · offline")).toBeTruthy();
    });

    it("lists the sessions like the Inbox with a status word each", async () => {
      const now = Date.now() / 1000;
      await open([
        info("old", { last_activity: now - 3 * 3600 }),
        info("run", { phase: "running", last_activity: now - 20, now: "Thinking" }),
        waiting("ask", gist(1), { last_activity: now - 600 }),
      ]);
      await openFromMenu();
      const rows = inDrawer().getAllByTestId(/^drawer-row-/);
      expect(rows.map((r) => r.props.testID)).toEqual(["drawer-row-ask", "drawer-row-run", "drawer-row-old"]);
      const word = (id: string, text: string) => within(screen.getByTestId(`drawer-row-${id}`)).getByText(text);
      expect([dark.amber, light.amber]).toContain(style(word("ask", "approve")).color);
      expect([dark.accent, light.accent]).toContain(style(word("run", "thinking")).color);
      expect([dark.muted, light.muted]).toContain(style(word("old", "3h")).color);
    });

    it("draws a session's name as text and presses as a 44 pt button", async () => {
      await open([info("<b>x")]);
      await openFromMenu();
      const row = inDrawer().getByRole("button", { name: /^<b>x/ });
      expect(Number(style(row).minHeight)).toBeGreaterThanOrEqual(44);
      expect(within(row).getByText("<b>x")).toBeTruthy();
    });
  });

  describe("navigation", () => {
    it("a tap on a session opens it and closes the drawer", async () => {
      const rig = await open([info("a"), info("b")]);
      await openFromMenu();
      await fireEvent.press(inDrawer().getByRole("button", { name: /^b/ }));
      expect(rig.view.getPathname()).toBe("/session/b");
      expect(screen.getByTestId("opened").children).toEqual(["b"]);
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
    });

    it("from one session, a tap on another replaces it: back goes to the Inbox", async () => {
      const rig = await open([info("a"), info("b")]);
      await act(async () => router.push("/session/a"));
      await openFromStore();
      await fireEvent.press(inDrawer().getByRole("button", { name: /^b/ }));
      expect(rig.view.getPathname()).toBe("/session/b");
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
    });

    it("Settings navigates and closes the drawer", async () => {
      const rig = await open([info("a")]);
      await openFromMenu();
      await fireEvent.press(inDrawer().getByRole("button", { name: "Settings" }));
      expect(rig.view.getPathname()).toBe("/settings");
      expect(screen.getByText("Settings screen")).toBeTruthy();
      expect(drawerStore.getState().open).toBe(false);
    });

    it("Inbox from another screen goes back to it", async () => {
      const rig = await open([info("a")]);
      await act(async () => router.push("/settings"));
      await act(async () => openDrawer());
      await wait(DRAWER_MS + 20);
      await fireEvent.press(inDrawer().getByRole("button", { name: /^Inbox/ }));
      expect(rig.view.getPathname()).toBe("/");
      expect(drawerStore.getState().open).toBe(false);
    });

    it("the + button starts a new session and closes the drawer", async () => {
      const rig = await open([info("a")]);
      await openFromMenu();
      const plus = inDrawer().getByRole("button", { name: "New session" });
      expect(Number(style(plus).height)).toBeGreaterThanOrEqual(44);
      expect(Number(style(plus).width)).toBeGreaterThanOrEqual(44);
      await fireEvent.press(plus);
      expect(rig.view.getPathname()).toBe("/recent");
      expect(drawerStore.getState().open).toBe(false);
    });

    it("marks the screen it is on", async () => {
      await open([info("a"), info("b")]);
      await act(async () => router.push("/session/a"));
      await openFromStore();
      const selected = (name: string | RegExp) => inDrawer().getByRole("button", { name }).props.accessibilityState?.selected === true;
      expect(selected(/^a/)).toBe(true);
      expect(selected(/^b/)).toBe(false);
      expect(selected(/^Inbox/)).toBe(false);
      expect(selected("Settings")).toBe(false);
    });
  });

  describe("gestures", () => {
    it("an 18 pt edge zone sits on the Inbox, and a horizontal drag of more than 30 pt opens the drawer", async () => {
      await open([info("a")]);
      expect(EDGE_WIDTH).toBe(18);
      expect(Number(style(screen.getByTestId("drawer-edge")).width)).toBe(18);
      await dragEdge([
        [12, 1],
        [34, 4],
      ]);
      expect(drawerStore.getState().open).toBe(true);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(true);
      // and the zone gets out of the way
      expect(screen.queryByTestId("drawer-edge")).toBeNull();
    });

    it("a_vertical_drag_never_opens_the_drawer", async () => {
      await open([info("a")]);
      await dragEdge([
        [2, 25],
        [3, 60],
        [5, 140],
      ]);
      expect(drawerStore.getState().open).toBe(false);
      // a long diagonal that is not twice as wide as it is tall
      await dragEdge([
        [20, 20],
        [50, 30],
        [70, 40],
      ]);
      expect(drawerStore.getState().open).toBe(false);
      // a slight horizontal drag
      await dragEdge([[28, 0]]);
      expect(drawerStore.getState().open).toBe(false);
    });

    it("a drag that turns vertical before it passes 30 pt does not open it", async () => {
      await open([info("a")]);
      await dragEdge([
        [25, 0],
        [26, 90],
        [40, 95],
      ]);
      expect(drawerStore.getState().open).toBe(false);
    });

    it("is on the Conversation and Settings too", async () => {
      await open([info("a")]);
      await act(async () => router.push("/session/a"));
      expect(screen.getByTestId("drawer-edge")).toBeTruthy();
      await act(async () => router.push("/settings"));
      expect(screen.getByTestId("drawer-edge")).toBeTruthy();
      await dragEdge([[40, 0]]);
      expect(drawerStore.getState().open).toBe(true);
    });

    it("a drag left of more than 40 pt on the drawer closes it", async () => {
      await open([info("a")]);
      await openFromMenu();
      expect(await dragPanel([[-15, 1], [-60, 3]])).toBe(true);
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
    });

    it("a short or vertical drag on the drawer leaves it open, and does not take the touch", async () => {
      await open([info("a")]);
      await openFromMenu();
      expect(await dragPanel([[-15, 0], [-35, 2]])).toBe(true);
      expect(drawerStore.getState().open).toBe(true);
      // a vertical drag (a scroll of the sessions) is left to the list
      expect(await dragPanel([[-2, -30], [-3, -80]])).toBe(false);
      expect(drawerStore.getState().open).toBe(true);
      // and so is a drag to the right
      expect(await dragPanel([[20, 0], [70, 0]])).toBe(false);
      expect(drawerStore.getState().open).toBe(true);
    });
  });

  describe("hidden", () => {
    it("has no edge zone and no drawer on Pair", async () => {
      await SecureStore.deleteItemAsync(TOKEN_KEY);
      await mount("/pair", ROUTES);
      expect(screen.queryByTestId("drawer-edge")).toBeNull();
      expect(screen.queryByTestId("drawer")).toBeNull();
    });

    it("closes and disables itself when the screen is not the Inbox, a Conversation or Settings", async () => {
      await open([info("a")]);
      await openFromMenu();
      await act(async () => router.push("/recent"));
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
      expect(screen.queryByTestId("drawer-edge")).toBeNull();
      // and nothing opens it there
      await act(async () => openDrawer());
      expect(drawerStore.getState().open).toBe(false);
      expect(isOpen()).toBe(false);
    });

    it("closes when the Mac is unpaired from under it", async () => {
      const rig = await open([info("a")]);
      await openFromMenu();
      await act(async () => {
        await rig.hub.unpair();
      });
      expect(drawerStore.getState().open).toBe(false);
      expect(isOpen()).toBe(false);
    });
  });

  it("keeps listing the sessions the Mac sends while it is open", async () => {
    const rig = await open([info("a")]);
    await openFromMenu();
    await hear(rig, [info("a"), info("b")]);
    expect(inDrawer().getByRole("button", { name: /^b/ })).toBeTruthy();
  });
});
