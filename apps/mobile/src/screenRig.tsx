// A paired app on a fake Mac, for the tests of screens that need the router, the hub controller
// and the store together. Not imported by the app.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, waitFor } from "@testing-library/react-native";
import { useLocalSearchParams } from "expo-router";
import { renderRouter } from "expo-router/testing-library";
import * as SecureStore from "expo-secure-store";
import { AppState, Text } from "react-native";
import type { ComponentType } from "react";

import RootLayout from "../app/_layout";
import Pair from "../app/pair";
import Sessions from "../app/index";
import { FakeClock, Sockets, type FakeSocket } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY, tokenKeyOf } from "./hub";
import { saveSettings } from "./persist";
import type { SessionInfo } from "./protocol";
import { octetStore } from "./store";
import { newM } from "./testKeys";

jest.mock("expo-camera", () => ({
  CameraView: () => null,
  useCameraPermissions: () => [{ granted: false, canAskAgain: false, status: "denied" }, jest.fn()],
}));

export const TOKEN = "cd".repeat(32);
export const HUB_URL = "wss://mac.tail1234.ts.net/";

export const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/Users/me/Projects/${id}`,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

/** Call in `beforeEach`: a saved token and hub address, an empty store. */
export async function pairedStorage(): Promise<void> {
  await AsyncStorage.clear();
  await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
  await saveSettings({ hubUrl: HUB_URL, deviceName: "Shubh's iPhone" });
  octetStore.getState().reset();
  jest.spyOn(AppState, "addEventListener").mockImplementation(() => ({ remove: jest.fn() }));
}

/** Opens the app at `url` with `routes` beside the shell, Sessions and Pair. Nothing is waited for:
 * a phone that is not paired never opens a socket. */
export async function mount(url: string, routes: Record<string, ComponentType>) {
  const sockets = new Sockets();
  const clock = new FakeClock();
  const hub = new HubController({
    factory: sockets.factory,
    store: octetStore,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
    newId: newM,
  });
  const Layout = () => (
    <HubContext.Provider value={hub}>
      <RootLayout />
    </HubContext.Provider>
  );
  const view = renderRouter(
    {
      _layout: Layout,
      index: Sessions,
      pair: Pair,
      "session/[id]": function Session() {
        return <Text testID="opened">{useLocalSearchParams<{ id: string }>().id}</Text>;
      },
      recent: () => null,
      usage: () => null,
      changes: () => null,
      new: () => null,
      settings: () => null,
      ...routes,
    },
    { initialUrl: url },
  );
  await view;
  return { hub, sockets, clock, view };
}

/** Opens the app at `url` with `routes` beside the shell, Sessions and Pair. The Mac welcomes the
 * phone and lists `list`. */
export async function launch(url: string, routes: Record<string, ComponentType>, list: SessionInfo[] = []) {
  const rig = await mount(url, routes);
  const { sockets } = rig;
  await waitFor(() => expect(sockets.all.length).toBeGreaterThan(0));
  await hear(rig, list);
  return rig;
}

export type Rig = Awaited<ReturnType<typeof launch>>;

/** The Mac welcomes the phone (once) and lists `list`. */
export async function hear({ sockets, clock }: Rig, list: SessionInfo[]) {
  await act(async () => {
    if (sockets.last.sent.length === 0) {
      sockets.last.open();
      sockets.last.receive({ type: "welcome", device: "iPhone" });
    }
    sockets.last.receive({ type: "sessions", sessions: list });
    clock.advance(50);
  });
}

/** The Macs of the tests that pair several, in pairing order: each one's id and host label. */
export const MACS = [
  { id: "m00000001", host: "studio" },
  { id: "m00000002", host: "mini" },
  { id: "m00000003", host: "air" },
] as const;

/** The hub address of the Mac whose host label is `host`. */
export const macUrl = (host: string): string => `wss://${host}.tail1234.ts.net/`;

/** Call in `beforeEach`: the first `count` of `MACS` paired (each its token in its own keychain
 * entry), Face ID off, an empty store. */
export async function pairedMacs(count: number = 2): Promise<void> {
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  const macs = MACS.slice(0, count);
  for (const m of macs) await SecureStore.setItemAsync(tokenKeyOf(m.id), TOKEN);
  await saveSettings({
    deviceName: "Shubh's iPhone",
    faceId: false,
    pairings: macs.map((m) => ({ id: m.id, hubUrl: macUrl(m.host), deviceName: "Shubh's iPhone", macName: m.host })),
  });
  octetStore.getState().reset();
  jest.spyOn(AppState, "addEventListener").mockImplementation(() => ({ remove: jest.fn() }));
}

/** The newest socket to the Mac whose host label is `host`. */
export function socketTo({ sockets }: Pick<Rig, "sockets">, host: string): FakeSocket {
  const all = sockets.all.filter((s) => s.url === macUrl(host));
  const socket = all[all.length - 1];
  if (!socket) throw new Error(`no socket to ${host}`);
  return socket;
}

/** The Mac `host` welcomes the phone (once on each socket) and lists `list`. */
export async function hearOn(rig: Pick<Rig, "sockets" | "clock">, host: string, list: SessionInfo[]) {
  await act(async () => {
    const socket = socketTo(rig, host);
    if (socket.sent.length === 0) {
      socket.open();
      socket.receive({ type: "welcome", device: "iPhone" });
    }
    socket.receive({ type: "sessions", sessions: list });
    rig.clock.advance(50);
  });
}

/** Opens the app at `url` on the Macs `pairedMacs` saved, each listing its own list of `lists`
 * (by host label). */
export async function launchMacs(url: string, routes: Record<string, ComponentType>, lists: Record<string, SessionInfo[]>) {
  const rig = await mount(url, routes);
  const hosts = Object.keys(lists);
  await waitFor(() => expect(rig.sockets.all.length).toBeGreaterThanOrEqual(hosts.length));
  for (const host of hosts) await hearOn(rig, host, lists[host] ?? []);
  return rig;
}
