// The screens across Macs: with two or more paired, the Inbox, the drawer, New session and Recent
// show every Mac's sessions, each named by its Mac, and a Mac that is asleep greys only its own.
// With one Mac they look as they always did (the one-Mac suites: inboxScreen, drawer, newScreen,
// recentScreen).
import { act, fireEvent, screen, within } from "@testing-library/react-native";
import { router } from "expo-router";
import { StyleSheet, Text } from "react-native";

import NewSession from "../app/new";
import Recent from "../app/recent";
import Session from "../app/session/[id]";
import { DRAWER_MS } from "./components/Drawer";
import { closeDrawer, openDrawer } from "./drawer";
import * as inboxWords from "./inbox";
import * as macWords from "./macs";
import { keyOf } from "./macs";
import type { ApprovalInfo, RecentInfo, SessionInfo } from "./protocol";
import { MACS, hearOn, info, launch, launchMacs, pairedMacs, pairedStorage, socketTo, type Rig } from "./screenRig";
import * as recentWords from "./recent";
import { mergeRecent } from "./recent";
import { hideToast } from "./toast";
import { k } from "./testKeys";

const [STUDIO, MINI] = MACS;
const inStudio = (id: string) => keyOf(STUDIO.id, id);
const inMini = (id: string) => keyOf(MINI.id, id);

const gist = (id: number): ApprovalInfo => ({
  id,
  summary: { action: "Bash", command: `make test-${id}`, files: [], scope: "this command once", cwd: "/w" },
});
const waiting = (id: string, approval: ApprovalInfo, extra: Partial<SessionInfo> = {}): SessionInfo =>
  info(id, { approvals: 1, phase: "running", approval, ...extra });

const ROUTES = {
  settings: () => <Text>Settings screen</Text>,
  recent: Recent,
  new: NewSession,
  "session/[id]": Session,
};

afterEach(() => {
  closeDrawer();
  hideToast();
  jest.restoreAllMocks();
});

// renderRouter runs on fake timers.
const wait = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));
const flush = () => act(async () => void jest.advanceTimersByTime(60));
const card = (key: string) => screen.getByTestId(`inbox-card-${key}`);
const inCard = (key: string) => within(card(key));
const style = (node: { props: { style?: unknown } }) => StyleSheet.flatten(node.props.style as never) as Record<string, unknown>;
const disabled = (node: { props: { accessibilityState?: { disabled?: boolean } } }) => node.props.accessibilityState?.disabled === true;
const tab = (name: RegExp | string) => screen.getByRole("tab", { name });
const list = () => screen.getByTestId("inbox-list");
/** Every text under `node`, joined. */
const textOf = (node: { children: unknown[] }): string =>
  node.children.map((c) => (typeof c === "string" ? c : textOf(c as { children: unknown[] }))).join("");

/** The Mac `host` drops its link: it is asleep (tried, and not ready). */
async function sleep(rig: Rig, host: string) {
  await act(async () => {
    socketTo(rig, host).drop();
  });
}

async function openTheDrawer() {
  await act(async () => openDrawer());
  await wait(DRAWER_MS + 20);
}

describe("cards name their Mac", () => {
  it("only when two or more are paired: one Mac's cards carry no label", async () => {
    await pairedStorage();
    await launch("/", ROUTES, [waiting("a", gist(1))]);
    expect(inCard(k("a")).queryByText("mac")).toBeNull();
    expect(inCard(k("a")).getByRole("button", { name: /^a, / }).props.accessibilityLabel).not.toMatch(/\bmac\b/);
  });

  it("with two, each card names its own Mac, muted, and says so to a screen reader", async () => {
    await pairedMacs(2);
    await launchMacs("/", ROUTES, { studio: [waiting("a", gist(1))], mini: [waiting("b", gist(2))] });
    expect(inCard(inStudio("a")).getByText("studio")).toBeTruthy();
    expect(inCard(inMini("b")).getByText("mini")).toBeTruthy();
    expect(inCard(inStudio("a")).getByRole("button", { name: /^a, / }).props.accessibilityLabel).toMatch(/studio/);
  });

  it("a long Mac name gives way: it shrinks to one line, and how long ago stays whole", async () => {
    await pairedMacs(2);
    await launchMacs("/", ROUTES, { studio: [info("a", { last_activity: 100 })], mini: [] });
    await fireEvent.press(tab("Done"));
    const name = inCard(inStudio("a")).getByText("studio");
    expect(name.props.numberOfLines).toBe(1);
    expect(style(name).flexShrink).toBe(1);
    const ago = inCard(inStudio("a")).getByText(/^\d+w$/);
    expect(style(ago).flexShrink).toBe(0);
  });

  it("orders as one list across Macs, newest first, a tie by the Mac's name, then by key", async () => {
    await pairedMacs(2);
    await launchMacs("/", ROUTES, {
      studio: [info("x", { last_activity: 100 }), info("y", { last_activity: 300 })],
      mini: [info("z", { last_activity: 100 }), info("w", { last_activity: 200 })],
    });
    await fireEvent.press(tab("Done"));
    const ids = screen.getAllByTestId(/^inbox-card-/).map((n) => String(n.props.testID).replace("inbox-card-", ""));
    // y (300), w (200), then the tie at 100: mini before studio.
    expect(ids).toEqual([inStudio("y"), inMini("w"), inMini("z"), inStudio("x")]);
  });
});

describe("one_mac_asleep", () => {
  beforeEach(() => pairedMacs(2));

  it("greys the asleep Mac's cards and stops its Allow; the other Mac's Allow sends on that Mac", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [waiting("a", gist(1))], mini: [waiting("b", gist(2))] });
    await sleep(rig, "studio");
    await wait(400);
    expect(screen.getByText(/2 Macs · 1 online/)).toBeTruthy();
    expect(style(card(inStudio("a"))).opacity).toBe(0.5);
    expect(style(card(inMini("b"))).opacity).toBeUndefined();
    const allowA = inCard(inStudio("a")).getByRole("button", { name: "Allow once for a" });
    const allowB = inCard(inMini("b")).getByRole("button", { name: "Allow once for b" });
    expect(disabled(allowA)).toBe(true);
    expect(disabled(allowB)).toBe(false);
    await fireEvent.press(allowA);
    await fireEvent.press(allowB);
    await flush();
    const studioAnswers = rig.sockets.all.filter((s) => s.url.includes("studio")).flatMap((s) => s.commandsOf("answer"));
    expect(studioAnswers).toEqual([]);
    expect(socketTo(rig, "mini").commandsOf("answer")).toEqual([expect.objectContaining({ session: "b", approval: 2, allow: true })]);
  });

  it("leaves the asleep Mac's sessions out of Allow all", async () => {
    const rig = await launchMacs("/", ROUTES, {
      studio: [waiting("a", gist(1))],
      mini: [waiting("b", gist(2)), waiting("c", gist(3))],
    });
    await sleep(rig, "studio");
    await wait(400);
    await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
    await flush();
    expect(rig.sockets.all.filter((s) => s.url.includes("studio")).flatMap((s) => s.commandsOf("answer"))).toEqual([]);
    expect(socketTo(rig, "mini").commandsOf("answer").map((c) => c.session)).toEqual(["b", "c"]);
  });

  it("the asleep Mac's prompt is off; the other Mac's prompt sends on it", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await sleep(rig, "studio");
    await act(async () => router.push(`/session/${encodeURIComponent(inStudio("a"))}`));
    await flush();
    await fireEvent.changeText(screen.getByLabelText("Message"), "hello");
    await flush();
    expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
    expect(disabled(screen.getByRole("button", { name: "Send (⏎)" }))).toBe(true);
    await act(async () => router.replace(`/session/${encodeURIComponent(inMini("b"))}`));
    await flush();
    await fireEvent.changeText(screen.getByLabelText("Message"), "hello");
    await flush();
    await fireEvent.press(screen.getByRole("button", { name: "Send (⏎)" }));
    await flush();
    expect(socketTo(rig, "mini").commandsOf("prompt")).toEqual([expect.objectContaining({ session: "b", text: "hello" })]);
  });

  it("with no Mac online, the header says so and the banner speaks of the Macs", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await sleep(rig, "studio");
    await sleep(rig, "mini");
    expect(screen.getByText(/2 Macs · 0 online/)).toBeTruthy();
    expect(screen.getByText("Can't reach your Macs")).toBeTruthy();
  });

  it("no banner while some Mac is online", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await sleep(rig, "studio");
    expect(screen.queryByText(/Can't reach your Mac/)).toBeNull();
  });
});

describe("pull to refresh across Macs", () => {
  beforeEach(() => pairedMacs(2));

  it("waits for every ready Mac's next list", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await act(async () => {
      list().props.refreshControl.props.onRefresh();
    });
    expect(list().props.refreshControl.props.refreshing).toBe(true);
    await hearOn(rig, "mini", [info("b")]);
    expect(list().props.refreshControl.props.refreshing).toBe(true);
    await hearOn(rig, "studio", [info("a")]);
    expect(list().props.refreshControl.props.refreshing).toBe(false);
  });

  it("tries an asleep Mac again now, and stops once it answers", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await sleep(rig, "studio");
    const before = rig.sockets.all.filter((s) => s.url.includes("studio")).length;
    await act(async () => {
      list().props.refreshControl.props.onRefresh();
    });
    expect(rig.sockets.all.filter((s) => s.url.includes("studio"))).toHaveLength(before + 1);
    await hearOn(rig, "mini", [info("b")]);
    expect(list().props.refreshControl.props.refreshing).toBe(true);
    await act(async () => {
      socketTo(rig, "studio").open();
      socketTo(rig, "studio").receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    expect(list().props.refreshControl.props.refreshing).toBe(false);
  });
});

describe("the banner shows the Fleet's notice", () => {
  it("a Mac that unpaired this phone is named, and Dismiss clears it", async () => {
    await pairedMacs(2);
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await act(async () => {
      socketTo(rig, "mini").receive({ type: "refused", req: 0, reason: "This device was revoked", code: "revoked" });
      rig.clock.advance(50);
    });
    await flush();
    expect(screen.getByText("This phone was unpaired on mini")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("This phone was unpaired on mini")).toBeNull();
    expect(rig.hub.snapshot().notice).toBeNull();
  });
});

describe("the drawer groups by Mac", () => {
  it("one header per Mac, in pairing order, with its sessions under it in the Inbox's order", async () => {
    await pairedMacs(2);
    await launchMacs("/", ROUTES, {
      studio: [info("a", { last_activity: 100 }), info("c", { last_activity: 300 })],
      mini: [waiting("b", gist(1))],
    });
    await openTheDrawer();
    const drawer = within(screen.getByTestId("drawer"));
    const order = drawer
      .getAllByTestId(/^drawer-(mac|row)-/)
      .map((n) => String(n.props.testID));
    expect(order).toEqual([
      `drawer-mac-${STUDIO.id}`,
      `drawer-row-${inStudio("c")}`,
      `drawer-row-${inStudio("a")}`,
      `drawer-mac-${MINI.id}`,
      `drawer-row-${inMini("b")}`,
    ]);
    expect(textOf(drawer.getByTestId(`drawer-mac-${STUDIO.id}`))).toMatch(/studio/);
    expect(drawer.queryByText(/· online/)).toBeNull();
  });

  it("says asleep for a Mac that is not ready, and greys and disables its rows", async () => {
    await pairedMacs(2);
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await sleep(rig, "studio");
    await openTheDrawer();
    const drawer = within(screen.getByTestId("drawer"));
    expect(textOf(drawer.getByTestId(`drawer-mac-${STUDIO.id}`))).toMatch(/studio · asleep/);
    expect(textOf(drawer.getByTestId(`drawer-mac-${MINI.id}`))).not.toMatch(/asleep/);
    const asleep = drawer.getByTestId(`drawer-row-${inStudio("a")}`);
    const awake = drawer.getByTestId(`drawer-row-${inMini("b")}`);
    expect(disabled(asleep)).toBe(true);
    expect(style(asleep).opacity).toBe(0.5);
    expect(disabled(awake)).toBe(false);
    await fireEvent.press(asleep);
    expect(rig.view.getPathname()).toBe("/");
    await fireEvent.press(awake);
    await wait(DRAWER_MS + 20);
    expect(rig.view.getPathname()).toBe(`/session/${inMini("b")}`);
  });

  it("says connecting… for a Mac whose first try is not over", async () => {
    await pairedMacs(2);
    await launchMacs("/", ROUTES, { mini: [info("b")] });
    await openTheDrawer();
    const drawer = within(screen.getByTestId("drawer"));
    expect(textOf(drawer.getByTestId(`drawer-mac-${STUDIO.id}`))).toMatch(/studio · connecting…/);
  });

  it("with one Mac there are no group headers", async () => {
    await pairedStorage();
    await launch("/", ROUTES, [info("a")]);
    await openTheDrawer();
    expect(within(screen.getByTestId("drawer")).queryAllByTestId(/^drawer-mac-/)).toEqual([]);
  });
});

describe("New session's Mac chip", () => {
  const macChip = () => screen.getByRole("button", { name: /^Mac / });

  it("cycles only the online Macs, and Start goes to the one chosen", async () => {
    await pairedMacs(3);
    // studio never answers: not online.
    const rig = await launchMacs("/", ROUTES, { mini: [info("on-mini")], air: [info("on-air")] });
    await act(async () => router.push("/new"));
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac mini");
    await fireEvent.press(macChip());
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac air");
    await fireEvent.press(macChip());
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac mini");
    await fireEvent.press(macChip());
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac air");
    await fireEvent.changeText(screen.getByLabelText("What should the agent do?"), "fix the build");
    await flush();
    await fireEvent.press(screen.getByRole("button", { name: "Start session" }));
    await flush();
    expect(socketTo(rig, "air").commandsOf("start")).toEqual([expect.objectContaining({ workspace: "/Users/me/Projects/on-air" })]);
    expect(socketTo(rig, "mini").commandsOf("start")).toEqual([]);
  });

  it("keeps the Mac a start went to: another Mac connecting, or a tap, moves neither the chip nor the first prompt", async () => {
    await pairedMacs(2);
    // studio, first in pairing order, is still connecting: mini is the first online Mac.
    const rig = await launchMacs("/", ROUTES, { mini: [info("on-mini")] });
    await act(async () => router.push("/new"));
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac mini");
    await fireEvent.changeText(screen.getByLabelText("What should the agent do?"), "fix the build");
    await flush();
    await fireEvent.press(screen.getByRole("button", { name: "Start session" }));
    await flush();
    const mini = socketTo(rig, "mini");
    expect(mini.commandsOf("start")).toHaveLength(1);
    await act(async () => {
      mini.receive({ type: "ack", req: Number(mini.commandsOf("start").at(-1)?.req), message: "Starting octet-a1b2c3 in /Users/me/Projects/on-mini" });
    });
    await flush();
    // studio connects: it is now the first online Mac.
    await hearOn(rig, "studio", [info("on-studio")]);
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac mini");
    // A tap while the launch is awaited changes nothing.
    await fireEvent.press(macChip());
    await flush();
    expect(macChip().props.accessibilityLabel).toBe("Mac mini");
    await hearOn(rig, "mini", [info("on-mini"), info("fresh", { tmux: "octet-a1b2c3", from_phone: true })]);
    await flush();
    expect(socketTo(rig, "mini").commandsOf("prompt")).toEqual([expect.objectContaining({ session: "fresh", text: "fix the build" })]);
    expect(rig.sockets.all.filter((x) => x.url.includes("studio")).flatMap((x) => x.commandsOf("prompt"))).toEqual([]);
    expect(rig.sockets.all.flatMap((x) => x.commandsOf("start"))).toHaveLength(1);
  });

  it("offers only the chosen Mac's folders", async () => {
    await pairedMacs(2);
    await launchMacs("/", ROUTES, { studio: [info("on-studio")], mini: [info("on-mini")] });
    await act(async () => router.push("/new"));
    await flush();
    expect(screen.getByRole("button", { name: /^Folder/ }).props.accessibilityLabel).toBe("Folder on-studio");
    await fireEvent.press(macChip());
    await flush();
    expect(screen.getByRole("button", { name: /^Folder/ }).props.accessibilityLabel).toBe("Folder on-mini");
  });
});

describe("Recent across Macs", () => {
  const recent = (session: string, started: number, title = session): RecentInfo => ({
    engine: "claude",
    workspace: `/w/${session}`,
    session,
    title,
    started,
  });
  const lastReq = (rig: Rig, host: string) => Number(socketTo(rig, host).commandsOf("recent").at(-1)?.req);
  async function answer(rig: Rig, host: string, items: RecentInfo[]) {
    await act(async () => {
      socketTo(rig, host).receive({ type: "recent", req: lastReq(rig, host), items });
      rig.clock.advance(50);
    });
    await flush();
  }
  async function refuse(rig: Rig, host: string, reason: string) {
    await act(async () => {
      socketTo(rig, host).receive({ type: "refused", req: lastReq(rig, host), reason, code: "internal" });
    });
    await flush();
  }

  it("asks every online Mac, merges by time and labels each by its Mac; Resume goes to that Mac", async () => {
    await pairedMacs(2);
    const rig = await launchMacs("/", ROUTES, { studio: [], mini: [] });
    await act(async () => router.push("/recent"));
    await flush();
    expect(socketTo(rig, "studio").commandsOf("recent")).toHaveLength(1);
    expect(socketTo(rig, "mini").commandsOf("recent")).toHaveLength(1);
    // Both in one go, so the list draws once (FlashList keeps a recycled cell where it was).
    await act(async () => {
      socketTo(rig, "studio").receive({ type: "recent", req: lastReq(rig, "studio"), items: [recent("s1", 1_000)] });
      socketTo(rig, "mini").receive({ type: "recent", req: lastReq(rig, "mini"), items: [recent("m1", 2_000), recent("m2", 500)] });
      rig.clock.advance(50);
    });
    await flush();
    const rows = screen.getAllByRole("button", { name: /^Resume / }).map((b) => String(b.props.accessibilityLabel));
    expect(rows).toEqual(["Resume m1, m1, on mini", "Resume s1, s1, on studio", "Resume m2, m2, on mini"]);
    expect(screen.getAllByText("mini")).toHaveLength(2);
    expect(screen.getAllByText("studio")).toHaveLength(1);
    await fireEvent.press(screen.getByRole("button", { name: "Resume s1, s1, on studio" }));
    await flush();
    expect(socketTo(rig, "studio").commandsOf("start")).toEqual([expect.objectContaining({ resume: "s1", workspace: "/w/s1" })]);
    expect(socketTo(rig, "mini").commandsOf("start")).toEqual([]);
  });

  it("a Mac that fails shows its error as one muted line under its name; the others still show", async () => {
    await pairedMacs(2);
    const rig = await launchMacs("/", ROUTES, { studio: [], mini: [] });
    await act(async () => router.push("/recent"));
    await flush();
    await refuse(rig, "studio", "The Mac's history is unreadable");
    await answer(rig, "mini", [recent("m1", 2_000)]);
    expect(screen.getByText("The Mac's history is unreadable")).toBeTruthy();
    expect(textOf(screen.getByTestId(`recent-failed-${STUDIO.id}`))).toMatch(/studio/);
    expect(screen.getByRole("button", { name: "Resume m1, m1, on mini" })).toBeTruthy();
  });

  it("keeps a Mac's rows as they were when another Mac answers: only the new rows draw", async () => {
    await pairedMacs(2);
    const rig = await launchMacs("/", ROUTES, { studio: [], mini: [] });
    await act(async () => router.push("/recent"));
    await flush();
    await answer(rig, "studio", [recent("s1", 1_000), recent("s2", 100)]);
    const drawn = jest.spyOn(recentWords, "startedAgo");
    await answer(rig, "mini", [recent("m1", 2_000)]);
    expect(drawn.mock.calls.map(([started]) => started)).toEqual([2_000]);
  });

  it("leaves out a Mac that is asleep, and disables nothing of the others", async () => {
    await pairedMacs(2);
    const rig = await launchMacs("/", ROUTES, { studio: [], mini: [] });
    await sleep(rig, "studio");
    await act(async () => router.push("/recent"));
    await flush();
    expect(rig.sockets.all.filter((s) => s.url.includes("studio")).flatMap((s) => s.commandsOf("recent"))).toEqual([]);
    await answer(rig, "mini", [recent("m1", 2_000)]);
    expect(disabled(screen.getByRole("button", { name: "Resume m1, m1, on mini" }))).toBe(false);
  });
});

describe("a link change on one Mac re-renders only that Mac's rows", () => {
  beforeEach(() => pairedMacs(2));

  it("on the Inbox", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a1"), info("a2")], mini: [info("b1"), info("b2")] });
    await fireEvent.press(tab("Done"));
    const drawn = jest.spyOn(inboxWords, "cardLabel");
    await sleep(rig, "studio");
    const ids = drawn.mock.calls.map(([item]) => item.id);
    expect(ids.length).toBeGreaterThan(0);
    expect(ids.every((id) => id.startsWith(`${STUDIO.id}/`))).toBe(true);
  });

  it("in the drawer: its header and rows only", async () => {
    const rig = await launchMacs("/", ROUTES, { studio: [info("a1"), info("a2")], mini: [info("b1"), info("b2")] });
    await openTheDrawer();
    const rows = jest.spyOn(inboxWords, "drawerRowLabel");
    const headers = jest.spyOn(macWords, "macStatus");
    await sleep(rig, "studio");
    const rowIds = rows.mock.calls.map(([row]) => row.id);
    expect(rowIds.length).toBeGreaterThan(0);
    expect(rowIds.every((id) => id.startsWith(`${STUDIO.id}/`))).toBe(true);
    expect(headers).toHaveBeenCalledTimes(1);
    expect(within(screen.getByTestId("drawer")).getByTestId(`drawer-mac-${STUDIO.id}`)).toHaveTextContent(/asleep/);
  });
});

describe("the words", () => {
  it("fleetLine counts the Macs and those online", () => {
    expect(macWords.fleetLine(2, 1)).toBe("2 Macs · 1 online");
    expect(macWords.fleetLine(4, 4)).toBe("4 Macs · 4 online");
  });

  it("macStatus: nothing when ready, asleep once tried, connecting… before", () => {
    expect(macWords.macStatus("ready", true)).toBeNull();
    expect(macWords.macStatus("ready", false)).toBeNull();
    expect(macWords.macStatus("offline", true)).toBe("asleep");
    expect(macWords.macStatus("connecting", true)).toBe("asleep");
    expect(macWords.macStatus("connecting", false)).toBe("connecting…");
    expect(macWords.macStatus("offline", false)).toBe("connecting…");
  });

  it("classify with the Macs' names labels each card and breaks a tie by the name, then the key", () => {
    const names = new Map([
      ["m1", "zeta"],
      ["m2", "alpha"],
    ]);
    const rows = [
      { ...info("m1/x", { last_activity: 5 }), mac: "m1" },
      { ...info("m2/y", { last_activity: 5 }), mac: "m2" },
      { ...info("m2/a", { last_activity: 5 }), mac: "m2" },
    ];
    const { done } = inboxWords.classify(rows, new Map(), { mac: "", now: 10_000, names });
    expect(done.map((i) => [i.id, i.macName])).toEqual([
      ["m2/a", "alpha"],
      ["m2/y", "alpha"],
      ["m1/x", "zeta"],
    ]);
    // The card names its Mac once: a quiet card's status is the engine alone.
    expect(done[0]?.status).toBe("claude");
    expect(inboxWords.cardLabel(done[0]!).match(/alpha/g)).toHaveLength(1);
  });

  it("mergeRecent orders newest start first, a tie by the Mac's name, then by key", () => {
    const r = (session: string, started: number): RecentInfo => ({ engine: "claude", workspace: "/w", session, title: "", started });
    const merged = mergeRecent([
      { mac: "m1", macName: "zeta", items: [r("a", 5), r("b", 9)] },
      { mac: "m2", macName: "alpha", items: [r("c", 5), r("d", 1)] },
    ]);
    expect(merged.map((x) => [x.mac, x.session])).toEqual([
      ["m1", "b"],
      ["m2", "c"],
      ["m1", "a"],
      ["m2", "d"],
    ]);
  });

  it("drawerList with the Macs' names groups the rows under a header per Mac, every Mac listed", () => {
    const names = new Map([
      ["m1", "studio"],
      ["m2", "mini"],
      ["m3", "air"],
    ]);
    const rows = [
      { ...info("m2/b"), mac: "m2" },
      { ...info("m1/a"), mac: "m1" },
    ];
    const { items } = inboxWords.drawerList(rows, 10_000, names);
    expect(items.map((i) => i.id)).toEqual(["mac:m1", "m1/a", "mac:m2", "m2/b", "mac:m3"]);
    expect(items.map((i) => i.kind)).toEqual(["mac", "row", "mac", "row", "mac"]);
  });
});
