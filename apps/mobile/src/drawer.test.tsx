import { act, fireEvent, screen, within } from "@testing-library/react-native";
import { router } from "expo-router";
import * as SecureStore from "expo-secure-store";
import { AccessibilityInfo, Animated, BackHandler, StyleSheet, Text } from "react-native";

import Session from "../app/session/[id]";
import { DRAWER_MS, DRAWER_WIDTH, EDGE_BOTTOM, EDGE_WIDTH, edgeBottom } from "./components/Drawer";
import { clearEdgeBottom, closeDrawer, drawerStore, openDrawer, setEdgeBottom } from "./drawer";
import { hear, info, launch, mount, pairedStorage } from "./screenRig";
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
  new: () => <Text>New session screen</Text>,
};

beforeEach(async () => {
  await pairedStorage();
  closeDrawer();
});
afterEach(() => {
  closeDrawer();
  drawerStore.setState({ edge: null });
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

/** Opens the drawer straight from its store, as the edge swipe and the Conversation's ☰ do. */
async function openFromStore() {
  await act(async () => openDrawer());
  await wait(DRAWER_MS + 20);
}

/** A finger on the screen, in page coordinates: `at` is a touch event for the finger at `x`, `y`. */
function finger() {
  let t = 1_000;
  let last: { x: number; y: number } | null = null;
  let down = { x: 0, y: 0 };
  return {
    at(x: number, y: number) {
      const previous = last ?? { x, y };
      if (last === null) down = { x, y };
      last = { x, y };
      t += 16;
      return {
        nativeEvent: { pageX: x, pageY: y, touches: [{ pageX: x, pageY: y }] },
        touchHistory: {
          indexOfSingleActiveTouch: 0,
          mostRecentTimeStamp: t,
          numberActiveTouches: 1,
          touchBank: [
            {
              touchActive: true,
              startPageX: down.x,
              startPageY: down.y,
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

/**
 * A drag as the responder system plays it on `node`: the touch starts at `from`; each move asks the
 * node (capture phase, then bubble phase) whether it takes the touch; once it has, it is granted
 * and gets the rest of the moves. `offsets` are from `from`. Returns whether the node took it.
 */
async function drag(
  node: { props: Record<string, (...args: never[]) => unknown> },
  from: [number, number],
  offsets: [number, number][],
  { start = true, end = "release" }: { start?: boolean; end?: "release" | "terminate" } = {},
) {
  const f = finger();
  const at = ([dx, dy]: [number, number]) => f.at(from[0] + dx, from[1] + dy);
  const p = node.props as Record<string, (e: unknown) => unknown>;
  let took = false;
  const down = f.at(from[0], from[1]);
  // Without its start, the touch went down where the node did not hear it (another responder had it).
  if (start) {
    await act(async () => {
      p.onStartShouldSetResponderCapture?.(down);
    });
  }
  for (const offset of offsets) {
    const event = at(offset);
    await act(async () => {
      if (took) {
        p.onResponderMove?.(event);
        return;
      }
      took = p.onMoveShouldSetResponderCapture?.(event) === true || p.onMoveShouldSetResponder?.(event) === true;
      if (took) p.onResponderGrant?.(event);
    });
  }
  await act(async () => {
    const last = at(offsets[offsets.length - 1] ?? [0, 0]);
    if (end === "terminate") p.onResponderTerminate?.(last);
    else p.onResponderRelease?.(last);
  });
  return took;
}

/** The native screens the stack has made, in order (what react-native-screens received). */
function nativeScreens() {
  const found: { props: Record<string, unknown> }[] = [];
  const walk = (node: unknown) => {
    if (!node || typeof node === "string") return;
    const n = node as { type: string; props: Record<string, unknown>; children?: unknown[] };
    if (n.type === "RNSScreen") found.push(n);
    for (const child of n.children ?? []) walk(child);
  };
  walk(screen.toJSON());
  return found;
}

/** A drag that starts in the 18 pt gutter at the screen's left edge (mid-height by default). */
const dragEdge = (offsets: [number, number][], from: [number, number] = [8, 300], how?: Parameters<typeof drag>[3]) =>
  drag(screen.getByTestId("drawer-host", { hidden: true }), from, offsets, how);
/** A drag on the open drawer. */
const dragPanel = (offsets: [number, number][]) => drag(drawer(), [250, 300], offsets);

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

    it("keeps a toast up over the drawer, and its action still works", async () => {
      const run = jest.fn();
      await open([info("a")]);
      await act(async () => showToast("b needs approval", run, "View"));
      await openFromMenu();
      expect(isOpen()).toBe(true);
      // (The drawer is a modal view to a screen reader, which sets its siblings aside: the toast is for touch here.)
      const hidden = { includeHiddenElements: true };
      expect(screen.getByTestId("toast", hidden)).toBeTruthy();
      // The host is drawn after the drawer, so it is over the drawer and its scrim.
      const order: string[] = [];
      const walk = (node: unknown): void => {
        if (Array.isArray(node)) return node.forEach(walk);
        if (node === null || typeof node !== "object") return;
        const { props, children } = node as { props?: { testID?: string }; children?: unknown };
        if (props?.testID) order.push(props.testID);
        walk(children);
      };
      walk(screen.toJSON());
      expect(order.indexOf("toast-wrap")).toBeGreaterThan(order.indexOf("drawer"));
      await fireEvent.press(screen.getByRole("button", { name: "View", ...hidden }));
      expect(run).toHaveBeenCalledTimes(1);
      expect(screen.queryByTestId("toast", hidden)).toBeNull();
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

    it("a tap on a session already in the stack goes back to it: no second copy", async () => {
      const rig = await open([info("a"), info("b")]);
      await act(async () => router.push("/session/a"));
      await act(async () => router.push("/settings"));
      await openFromStore();
      await fireEvent.press(inDrawer().getByRole("button", { name: /^a/ }));
      expect(rig.view.getPathname()).toBe("/session/a");
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
      expect(router.canGoBack()).toBe(false);
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
      expect(rig.view.getPathname()).toBe("/new");
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

  describe("while it is open", () => {
    it("closes when the screen changes: a notification tap, a link, any navigation", async () => {
      await open([info("a")]);
      await openFromMenu();
      await act(async () => router.push("/session/a"));
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
      await openFromStore();
      await act(async () => router.replace("/settings"));
      expect(drawerStore.getState().open).toBe(false);
    });

    it("keeps the ago texts moving: the clock ticks every 30 s, and is read afresh when it opens", async () => {
      const ago = (id: string) => within(screen.getByTestId(`drawer-row-${id}`)).getByText(/^\d+[mhd]$/);
      await open([info("a", { last_activity: Date.now() / 1000 - 60 })]);
      // shut for ten minutes: nothing is drawn, and it opens on the time it is
      await wait(600_000);
      await openFromMenu();
      expect(ago("a").props.children).toBe("11m");
      await wait(150_000);
      expect(ago("a").props.children).toBe("13m");
    });

    it("hides what is under it from a screen reader on Android too", async () => {
      await open([info("a")]);
      const host = () => screen.getByTestId("drawer-host", { hidden: true });
      expect(host().props.importantForAccessibility).toBe("auto");
      await openFromMenu();
      expect(host().props.importantForAccessibility).toBe("no-hide-descendants");
      await fireEvent.press(scrim());
      expect(host().props.importantForAccessibility).toBe("auto");
    });
  });

  describe("gestures", () => {
    it("a horizontal drag of more than 30 pt from the 18 pt gutter opens the drawer", async () => {
      await open([info("a")]);
      expect(EDGE_WIDTH).toBe(18);
      expect(
        await dragEdge([
          [12, 1],
          [34, 4],
        ]),
      ).toBe(true);
      expect(drawerStore.getState().open).toBe(true);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(true);
    });

    it("opens on a single fast move past 30 pt too", async () => {
      await open([info("a")]);
      await dragEdge([[45, 3]]);
      expect(drawerStore.getState().open).toBe(true);
    });

    it("starts only in the gutter: 18 pt in, not 19, and not up in the header or down at the composer", async () => {
      await open([info("a")]);
      await dragEdge([[60, 0]], [19, 300]);
      expect(drawerStore.getState().open).toBe(false);
      await dragEdge([[60, 0]], [8, 10]);
      expect(drawerStore.getState().open).toBe(false);
      await dragEdge([[60, 0]], [8, 1_300]);
      expect(drawerStore.getState().open).toBe(false);
      await dragEdge([[60, 0]], [18, 300]);
      expect(drawerStore.getState().open).toBe(true);
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

    it("a tap or a vertical scroll that starts in the gutter is not taken: the content under still gets it", async () => {
      await open([info("a")]);
      const host = screen.getByTestId("drawer-host", { hidden: true });
      const p = host.props as Record<string, (e: unknown) => unknown>;
      const f = finger();
      // The gutter claims nothing on touch start, so a tap lands on what is under it.
      expect(p.onStartShouldSetResponderCapture?.(f.at(8, 300))).toBeFalsy();
      expect(p.onStartShouldSetResponder?.(f.at(8, 300))).toBeFalsy();
      expect(await dragEdge([[1, 0]])).toBe(false);
      expect(await dragEdge([[0, 40], [1, 120]])).toBe(false);
      expect(drawerStore.getState().open).toBe(false);
      // and no strip sits over the gutter to swallow a tap
      expect(screen.queryByTestId("drawer-edge", { hidden: true })).toBeNull();
    });

    it("is on the Conversation and Settings too", async () => {
      await open([info("a")]);
      await act(async () => router.push("/session/a"));
      await dragEdge([[40, 0]]);
      expect(drawerStore.getState().open).toBe(true);
      await act(async () => closeDrawer());
      await act(async () => router.push("/settings"));
      await dragEdge([[40, 0]]);
      expect(drawerStore.getState().open).toBe(true);
    });

    it("forgets where a touch began once it is released: a later move with no start of its own is not the gutter's", async () => {
      await open([info("a")]);
      // Taken from the gutter, released short of 30 pt.
      expect(await dragEdge([[12, 0]])).toBe(true);
      expect(drawerStore.getState().open).toBe(false);
      // A touch that went down out in the screen, which the host never heard start.
      expect(await dragEdge([[20, 0], [60, 0]], [200, 300], { start: false })).toBe(false);
      expect(drawerStore.getState().open).toBe(false);
    });

    it("forgets where a touch began once another responder takes it away", async () => {
      await open([info("a")]);
      expect(await dragEdge([[12, 0]], [8, 300], { end: "terminate" })).toBe(true);
      expect(drawerStore.getState().open).toBe(false);
      expect(await dragEdge([[20, 0], [60, 0]], [200, 300], { start: false })).toBe(false);
      expect(drawerStore.getState().open).toBe(false);
    });

    it("leaves out what is under the prompt line, measured: its top, as the keyboard moves it", async () => {
      await launch("/", { ...ROUTES, "session/[id]": Session }, [info("a")]);
      await act(async () => router.push("/session/a"));
      await screen.findByTestId("prompt-area");
      await fireEvent(screen.getByTestId("prompt-area"), "layout", { nativeEvent: { layout: { x: 0, y: 900, width: 400, height: 140 } } });
      expect(drawerStore.getState().edge?.bottom).toBe(900);
      await dragEdge([[60, 0]], [8, 950]);
      expect(drawerStore.getState().open).toBe(false);
      await dragEdge([[60, 0]], [8, 850]);
      expect(drawerStore.getState().open).toBe(true);
      await act(async () => closeDrawer());
      // The keyboard lifts the prompt line: the strip ends higher up.
      await fireEvent(screen.getByTestId("prompt-area"), "layout", { nativeEvent: { layout: { x: 0, y: 500, width: 400, height: 140 } } });
      expect(drawerStore.getState().edge?.bottom).toBe(500);
      await dragEdge([[60, 0]], [8, 600]);
      expect(drawerStore.getState().open).toBe(false);
      // Leaving the conversation drops its bound.
      await act(async () => router.push("/settings"));
      expect(drawerStore.getState().edge).toBeNull();
    });

    it("without a measured prompt line, ends the safe-area inset plus a fixed offset above the bottom", () => {
      expect(EDGE_BOTTOM).toBe(120);
      expect(edgeBottom(1_000, 34, null)).toBe(1_000 - 34 - 120);
      expect(edgeBottom(1_000, 0, null)).toBe(880);
      expect(edgeBottom(1_000, 34, 700)).toBe(700);
    });

    it("a bound is dropped only by the screen that set it", () => {
      const a = {};
      const b = {};
      setEdgeBottom(a, 700);
      setEdgeBottom(b, 650);
      clearEdgeBottom(a);
      expect(drawerStore.getState().edge?.bottom).toBe(650);
      clearEdgeBottom(b);
      expect(drawerStore.getState().edge).toBeNull();
    });

    it("leaves the screens' own back-swipe off: the drawer owns the left edge", async () => {
      await open([info("a")]);
      await act(async () => router.push("/session/a"));
      await act(async () => router.push("/settings"));
      const screens = nativeScreens();
      expect(screens).toHaveLength(3);
      for (const s of screens) {
        expect(s.props.gestureEnabled).toBe(false);
        // react-native-screens takes this one as the string "false"
        expect(String(s.props.fullScreenSwipeEnabled)).toBe("false");
      }
    });

    it("a single fast drag left of more than 40 pt closes the drawer too", async () => {
      await open([info("a")]);
      await openFromMenu();
      await dragPanel([[-70, 2]]);
      expect(drawerStore.getState().open).toBe(false);
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
    it("has no edge swipe and no drawer on Pair", async () => {
      await SecureStore.deleteItemAsync(TOKEN_KEY);
      await mount("/pair", ROUTES);
      expect(await dragEdge([[60, 0]])).toBe(false);
      expect(drawerStore.getState().open).toBe(false);
      expect(screen.queryByTestId("drawer")).toBeNull();
    });

    it("closes and disables itself when the screen is not the Inbox, a Conversation or Settings", async () => {
      await open([info("a")]);
      await openFromMenu();
      await act(async () => router.push("/recent"));
      expect(drawerStore.getState().open).toBe(false);
      await wait(DRAWER_MS + 20);
      expect(isOpen()).toBe(false);
      expect(await dragEdge([[60, 0]])).toBe(false);
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
