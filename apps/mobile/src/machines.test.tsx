// Phase D's last part: Settings › Machines (each Mac's row, Rename, Unpair, "+ Pair another
// machine"), Pair reaching a pairing with no link, a push tap routed by the Mac it names, and the
// screens of a Mac that is unpaired going back to the Inbox.
import { act, fireEvent, screen, within } from "@testing-library/react-native";
import * as Notifications from "expo-notifications";
import { router } from "expo-router";
import * as SecureStore from "expo-secure-store";
import { Alert, StyleSheet, type AlertButton } from "react-native";

import Changes from "../app/changes";
import Session from "../app/session/[id]";
import Settings from "../app/settings";
import Usage from "../app/usage";
import { tokenKeyOf } from "./hub";
import { keyOf } from "./macs";
import { loadSettings } from "./persist";
import { MACS, hearOn, info, launchMacs, mount, pairedMacs, socketTo, type Rig } from "./screenRig";
import { octetStore } from "./store";

const [STUDIO, MINI, AIR] = MACS;
const inStudio = (id: string) => keyOf(STUDIO.id, id);
const inMini = (id: string) => keyOf(MINI.id, id);
const n = Notifications as jest.Mocked<typeof Notifications>;

const ROUTES = { settings: Settings };

afterEach(() => jest.restoreAllMocks());

// renderRouter runs on fake timers.
const flush = () => act(async () => void jest.advanceTimersByTime(60));
const row = (id: string) => within(screen.getByTestId(`machine-${id}`));
const disabled = (node: { props: { accessibilityState?: { disabled?: boolean } } }) => node.props.accessibilityState?.disabled === true;
const minHeight = (node: { props: { style?: unknown } }) => (StyleSheet.flatten(node.props.style as never) as { minHeight?: number }).minHeight ?? 0;

function confirm() {
  return jest.spyOn(Alert, "alert").mockImplementation(() => {});
}
const buttons = (alert: jest.SpyInstance): AlertButton[] => (alert.mock.calls.at(-1)?.[2] ?? []) as AlertButton[];

async function openSettings(rig: Rig) {
  await act(async () => router.push("/settings"));
  await flush();
  return rig;
}

describe("Settings › Machines", () => {
  it("lists each Mac in pairing order with its host, muted, and its own state", async () => {
    await pairedMacs(3);
    // air's link is still trying: its first attempt is not over.
    const rig = await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] });
    await openSettings(rig);
    expect(screen.getAllByTestId(/^machine-/).map((r) => r.props.testID)).toEqual(MACS.slice(0, 3).map((m) => `machine-${m.id}`));
    expect(row(STUDIO.id).getByText("● studio")).toBeTruthy();
    expect(row(STUDIO.id).getByText("online")).toBeTruthy();
    expect(row(STUDIO.id).getByText("studio.tail1234.ts.net")).toBeTruthy();
    expect(row(AIR.id).getByText("○ air")).toBeTruthy();
    expect(row(AIR.id).getByText("connecting…")).toBeTruthy();
    // mini drops its link: only its own row says so.
    await act(async () => {
      socketTo(rig, "mini").drop();
    });
    expect(row(MINI.id).getByText("○ mini")).toBeTruthy();
    expect(row(MINI.id).getByText("asleep")).toBeTruthy();
    expect(row(STUDIO.id).getByText("online")).toBeTruthy();
    expect(screen.getByText("Shubh's iPhone")).toBeTruthy();
  });

  it("Rename opens a field with the name, and Save keeps the cleaned name through the Fleet", async () => {
    await pairedMacs(2);
    const rig = await openSettings(await launchMacs("/", ROUTES, { studio: [], mini: [] }));
    await fireEvent.press(row(MINI.id).getByRole("button", { name: "Rename mini" }));
    const field = screen.getByLabelText("New name for mini");
    expect(field.props.value).toBe("mini");
    expect(field.props.maxLength).toBe(40);
    await fireEvent.changeText(field, "  Mac   mini‮  ");
    await fireEvent.press(row(MINI.id).getByRole("button", { name: "Save the name" }));
    await flush();
    expect(row(MINI.id).getByText("● Mac mini")).toBeTruthy();
    expect(screen.queryByLabelText("New name for mini")).toBeNull();
    expect(rig.hub.snapshot().macs.map((m) => m.macName)).toEqual(["studio", "Mac mini"]);
    expect((await loadSettings()).pairings.map((p) => p.macName)).toEqual(["studio", "Mac mini"]);
  });

  it("an empty name can't be saved, and Cancel keeps the name", async () => {
    await pairedMacs(2);
    await openSettings(await launchMacs("/", ROUTES, { studio: [], mini: [] }));
    await fireEvent.press(row(STUDIO.id).getByRole("button", { name: "Rename studio" }));
    await fireEvent.changeText(screen.getByLabelText("New name for studio"), "  ‏ ");
    expect(disabled(row(STUDIO.id).getByRole("button", { name: "Save the name" }))).toBe(true);
    await fireEvent.press(row(STUDIO.id).getByRole("button", { name: "Cancel renaming" }));
    expect(row(STUDIO.id).getByText("● studio")).toBeTruthy();
    expect((await loadSettings()).pairings.map((p) => p.macName)).toEqual(["studio", "mini"]);
  });

  it("Unpair asks first, in these words, and Cancel changes nothing", async () => {
    await pairedMacs(2);
    const alert = confirm();
    const rig = await openSettings(await launchMacs("/", ROUTES, { studio: [], mini: [] }));
    await fireEvent.press(row(MINI.id).getByRole("button", { name: "Unpair mini" }));
    expect(alert).toHaveBeenCalledTimes(1);
    expect(alert.mock.calls[0]?.[0]).toBe("Unpair mini?");
    expect(alert.mock.calls[0]?.[1]).toBe("Its sessions leave this phone.");
    expect(buttons(alert).map((b) => [b.text, b.style])).toEqual([
      ["Cancel", "cancel"],
      ["Unpair", "destructive"],
    ]);
    await act(async () => buttons(alert)[0]?.onPress?.());
    expect(rig.hub.snapshot().macs).toHaveLength(2);
    expect(await SecureStore.getItemAsync(tokenKeyOf(MINI.id))).not.toBeNull();
  });

  it("Unpair drops only that Mac: its sessions leave, the other Mac's stay, and Settings stays", async () => {
    await pairedMacs(2);
    const alert = confirm();
    const rig = await openSettings(await launchMacs("/", ROUTES, { studio: [info("a")], mini: [info("b")] }));
    await fireEvent.press(row(MINI.id).getByRole("button", { name: "Unpair mini" }));
    await act(async () => {
      buttons(alert)[1]?.onPress?.();
    });
    await flush();
    expect(rig.hub.snapshot().macs.map((m) => m.id)).toEqual([STUDIO.id]);
    expect(octetStore.getState().sessions.map((s) => s.id)).toEqual([inStudio("a")]);
    expect(await SecureStore.getItemAsync(tokenKeyOf(MINI.id))).toBeNull();
    expect(screen.queryByTestId(`machine-${MINI.id}`)).toBeNull();
    expect(rig.view.getPathname()).toBe("/settings");
  });

  it("+ Pair another machine opens Pair to add one", async () => {
    await pairedMacs(2);
    const rig = await openSettings(await launchMacs("/", ROUTES, { studio: [], mini: [] }));
    const add = screen.getByRole("button", { name: "Pair another machine" });
    expect(disabled(add)).toBe(false);
    expect(screen.queryByText("4 Macs is the most")).toBeNull();
    await fireEvent.press(add);
    await flush();
    expect(rig.view.getPathname()).toBe("/pair");
    expect(screen.getByRole("header", { name: "Pair another machine" })).toBeTruthy();
  });

  it("+ Pair another machine is disabled at 4, and says why", async () => {
    await pairedMacs(4);
    const rig = await openSettings(await launchMacs("/", ROUTES, { studio: [], mini: [], air: [], pro: [] }));
    const add = screen.getByRole("button", { name: "Pair another machine" });
    expect(disabled(add)).toBe(true);
    expect(screen.getByText("4 Macs is the most")).toBeTruthy();
    await fireEvent.press(add);
    await flush();
    expect(rig.view.getPathname()).toBe("/settings");
  });

  it("sizes every row's controls for a thumb", async () => {
    await pairedMacs(2);
    await openSettings(await launchMacs("/", ROUTES, { studio: [], mini: [] }));
    for (const name of ["Rename studio", "Unpair studio", "Rename mini", "Unpair mini", "Pair another machine"]) {
      expect(minHeight(screen.getByRole("button", { name }))).toBeGreaterThanOrEqual(44);
    }
  });
});

describe("a pairing with no link", () => {
  // The keychain is jest.setup's in-memory mock: a lock wraps its implementation, and the
  // implementation is put back after each test.
  const getItem = SecureStore.getItemAsync as jest.MockedFunction<typeof SecureStore.getItemAsync>;
  const real = getItem.getMockImplementation();
  afterEach(() => {
    if (real) getItem.mockImplementation(real);
  });
  /** The keychain will not give the tokens of `macs`: their pairings stay, with no link. */
  const lock = (...macs: string[]) => {
    getItem.mockImplementation((key, options) =>
      macs.some((id) => key === tokenKeyOf(id)) ? Promise.reject(new Error("locked")) : (real?.(key, options) ?? Promise.resolve(null)),
    );
  };

  it("is listed offline in Settings, and can be unpaired there", async () => {
    await pairedMacs(2);
    lock(MINI.id);
    const alert = confirm();
    const rig = await openSettings(await launchMacs("/", ROUTES, { studio: [] }));
    expect(row(MINI.id).getByText("○ mini")).toBeTruthy();
    expect(row(MINI.id).getByText("offline")).toBeTruthy();
    await fireEvent.press(row(MINI.id).getByRole("button", { name: "Unpair mini" }));
    await act(async () => {
      buttons(alert)[1]?.onPress?.();
    });
    await flush();
    expect(rig.hub.snapshot().macs.map((m) => m.id)).toEqual([STUDIO.id]);
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual([STUDIO.id]);
  });

  it("is listed on Pair while no Mac is linked, with Unpair, so the phone can be cleared", async () => {
    await pairedMacs(2);
    lock(MINI.id, STUDIO.id);
    const alert = confirm();
    const rig = await mount("/", {});
    await flush();
    expect(rig.hub.snapshot().status).toBe("unpaired");
    expect(rig.view.getPathname()).toBe("/pair");
    expect(screen.getByText("○ studio")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Unpair mini" }));
    expect(alert.mock.calls[0]?.[0]).toBe("Unpair mini?");
    await act(async () => {
      buttons(alert)[1]?.onPress?.();
    });
    await flush();
    expect(rig.hub.snapshot().macs.map((m) => m.id)).toEqual([STUDIO.id]);
    expect(screen.queryByRole("button", { name: "Unpair mini" })).toBeNull();
    await fireEvent.press(screen.getByRole("button", { name: "Unpair studio" }));
    await act(async () => {
      buttons(alert)[1]?.onPress?.();
    });
    await flush();
    expect(rig.hub.snapshot().macs).toEqual([]);
    expect(screen.getByText("No Mac yet")).toBeTruthy();
    expect((await loadSettings()).pairings).toEqual([]);
  });
});

describe("tap_routes_by_mac", () => {
  const tap = (data: unknown, id: string) => ({ notification: { request: { identifier: id, content: { data } } } }) as never;
  const listener = () => n.addNotificationResponseReceivedListener.mock.calls[0]?.[0];
  const host = (label: string) => `${label}.tail1234.ts.net`;
  const sent = (rig: Rig) => rig.sockets.all.reduce((count, s) => count + s.sent.length, 0);

  beforeEach(() => pairedMacs(2));

  // Both Macs list the same raw id: the host alone says whose it is.
  const both = { studio: [info("1-1")], mini: [info("1-1")] };

  it("a known host opens that Mac's session, whatever its case", async () => {
    const rig = await launchMacs("/", {}, both);
    await act(async () => listener()?.(tap({ session: "1-1", mac: host("mini") }, "t1")));
    expect(rig.view.getPathname()).toBe(`/session/${inMini("1-1")}`);
    await act(async () => listener()?.(tap({ session: "1-1", mac: "STUDIO.Tail1234.TS.net" }, "t2")));
    expect(rig.view.getPathname()).toBe(`/session/${inStudio("1-1")}`);
  });

  it("a known host whose session has ended opens the Inbox, and nothing is sent", async () => {
    const rig = await launchMacs("/", {}, both);
    await act(async () => router.push("/settings"));
    const before = sent(rig);
    await act(async () => listener()?.(tap({ session: "9-9", mac: host("mini") }, "t1")));
    expect(rig.view.getPathname()).toBe("/");
    expect(sent(rig)).toBe(before);
  });

  it("an unknown host, or none with two Macs, opens the Inbox", async () => {
    const rig = await launchMacs("/", {}, both);
    await act(async () => router.push("/settings"));
    await act(async () => listener()?.(tap({ session: "1-1", mac: host("air") }, "t1")));
    expect(rig.view.getPathname()).toBe("/");
    await act(async () => router.push("/settings"));
    await act(async () => listener()?.(tap({ session: "1-1" }, "t2")));
    expect(rig.view.getPathname()).toBe("/");
  });

  it("a cold start's tap waits for the pairings, then opens the Mac it names", async () => {
    n.getLastNotificationResponse.mockReturnValue(tap({ session: "1-1", mac: host("mini") }, "t1"));
    const rig = await mount("/", {});
    await flush();
    await hearOn(rig, "mini", [info("1-1")]);
    expect(rig.view.getPathname()).toBe(`/session/${inMini("1-1")}`);
  });
});

describe("a screen of a Mac being unpaired goes back to the Inbox", () => {
  const ROUTED = { "session/[id]": Session, changes: Changes, usage: Usage };
  const lists = { studio: [info("a")], mini: [info("b")] };

  beforeEach(() => pairedMacs(2));

  it.each([
    ["Conversation", (key: string) => `/session/${encodeURIComponent(key)}`],
    ["Changes", (key: string) => `/changes?id=${encodeURIComponent(key)}`],
    ["Usage", (key: string) => `/usage?id=${encodeURIComponent(key)}`],
  ])("%s", async (_name, path) => {
    const rig = await launchMacs("/", ROUTED, lists);
    await act(async () => router.push(path(inMini("b"))));
    await flush();
    expect(rig.view.getPathname()).not.toBe("/");
    await act(async () => {
      await rig.hub.unpair(MINI.id);
    });
    await flush();
    expect(rig.view.getPathname()).toBe("/");
    expect(router.canGoBack()).toBe(false);
  });

  it("the other Mac's screens stay", async () => {
    const rig = await launchMacs("/", ROUTED, lists);
    await act(async () => router.push(`/session/${encodeURIComponent(inStudio("a"))}`));
    await flush();
    await act(async () => {
      await rig.hub.unpair(MINI.id);
    });
    await flush();
    expect(rig.view.getPathname()).toBe(`/session/${inStudio("a")}`);
  });

  it("one under the other Mac's screen goes when it is in front again", async () => {
    const rig = await launchMacs("/", ROUTED, lists);
    await act(async () => router.push(`/session/${encodeURIComponent(inMini("b"))}`));
    await act(async () => router.push(`/usage?id=${encodeURIComponent(inStudio("a"))}`));
    await flush();
    await act(async () => {
      await rig.hub.unpair(MINI.id);
    });
    await flush();
    expect(rig.view.getPathname()).toBe("/usage");
    await act(async () => router.back());
    await flush();
    expect(rig.view.getPathname()).toBe("/");
  });

  it("the Mac revoking the phone does the same", async () => {
    const rig = await launchMacs("/", ROUTED, lists);
    await act(async () => router.push(`/session/${encodeURIComponent(inMini("b"))}`));
    await flush();
    await act(async () => {
      socketTo(rig, "mini").receive({ type: "refused", req: 0, reason: "This device was revoked", code: "revoked" });
      rig.clock.advance(50);
    });
    await flush();
    expect(rig.view.getPathname()).toBe("/");
  });
});
