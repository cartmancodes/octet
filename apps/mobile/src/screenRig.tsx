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
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY } from "./hub";
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
