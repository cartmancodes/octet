import { act, fireEvent, screen, waitFor } from "@testing-library/react-native";
import AsyncStorage from "@react-native-async-storage/async-storage";
import { router } from "expo-router";
import { renderRouter } from "expo-router/testing-library";
import * as Notifications from "expo-notifications";
import * as SecureStore from "expo-secure-store";
import * as SplashScreen from "expo-splash-screen";
import * as SystemUI from "expo-system-ui";
import * as ReactNative from "react-native";
import { AppState, Text } from "react-native";

import RootLayout from "../app/_layout";
import Pair from "../app/pair";
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY, UNPAIRED_NOTICE } from "./hub";
import { saveSettings } from "./persist";
import { createOctetStore } from "./store";
import { dark, light } from "./theme";

jest.mock("expo-camera", () => ({
  CameraView: () => null,
  useCameraPermissions: () => [{ granted: false, canAskAgain: false, status: "denied" }, jest.fn()],
}));

let mockProject: string | undefined;
jest.mock("expo-constants", () => ({
  __esModule: true,
  default: {
    get expoConfig() {
      return { extra: { eas: { projectId: mockProject } } };
    },
  },
}));

const TOKEN = "cd".repeat(32);
const n = Notifications as jest.Mocked<typeof Notifications>;
const LINK = `https://mac.tail1234.ts.net/#pair=${"ab".repeat(32)}`;

let changeAppState: (state: string) => void = () => {};

beforeEach(async () => {
  mockProject = undefined;
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  jest.spyOn(AppState, "addEventListener").mockImplementation((_type, listener) => {
    changeAppState = listener as (state: string) => void;
    return { remove: jest.fn() };
  });
});

afterEach(() => jest.restoreAllMocks());

async function launch(opts: { paired?: boolean } = {}) {
  if (opts.paired) {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
  }
  const sockets = new Sockets();
  const clock = new FakeClock();
  const hub = new HubController({
    factory: sockets.factory,
    store: createOctetStore(),
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
  });
  const Layout = () => (
    <HubContext.Provider value={hub}>
      <RootLayout />
    </HubContext.Provider>
  );
  const view = renderRouter({
    _layout: Layout,
    index: () => <Text>Sessions list</Text>,
    pair: Pair,
    "session/[id]": () => null,
    recent: () => null,
    new: () => null,
    settings: () => null,
  });
  await view;
  return { hub, sockets, clock, view };
}

/** Walks the connection to ready: hello, welcome. */
function connected({ sockets }: { sockets: Sockets }): void {
  sockets.last.open();
  sockets.last.receive({ type: "welcome", device: "iPhone" });
}

describe("the app shell", () => {
  it("opens on Pair without a token", async () => {
    const { sockets } = await launch();
    expect(await screen.findByText("Scan the code from octet pair")).toBeTruthy();
    expect(screen.queryByText("Sessions list")).toBeNull();
    expect(sockets.all).toHaveLength(0);
  });

  it("a link to a session before pairing lands on Pair, not the session", async () => {
    const sockets = new Sockets();
    const hub = new HubController({ factory: sockets.factory, store: createOctetStore(), platform: "ios" });
    const Layout = () => (
      <HubContext.Provider value={hub}>
        <RootLayout />
      </HubContext.Provider>
    );
    const view = renderRouter(
      { _layout: Layout, index: () => <Text>Sessions list</Text>, pair: Pair, "session/[id]": () => <Text>Session x</Text>, recent: () => null, new: () => null, settings: () => null },
      { initialUrl: "/session/x" },
    );
    await view;
    expect(await screen.findByText("Scan the code from octet pair")).toBeTruthy();
    expect(screen.queryByText("Session x")).toBeNull();
    expect(view.getPathname()).toBe("/pair");
    expect(sockets.all).toHaveLength(0);
  });

  it("opens on Sessions with a token, and connects once", async () => {
    const { sockets } = await launch({ paired: true });
    expect(await screen.findByText("Sessions list")).toBeTruthy();
    expect(screen.queryByText("Scan the code from octet pair")).toBeNull();
    expect(sockets.all).toHaveLength(1);
  });

  it("pairing goes to Sessions and connects with the new token", async () => {
    const { sockets } = await launch();
    await fireEvent.changeText(await screen.findByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    sockets.last.open();
    const [command] = sockets.last.commandsOf("pair");
    sockets.last.receive({ type: "paired", req: command?.req, token: TOKEN, device: "iPhone" });
    expect(await screen.findByText("Sessions list")).toBeTruthy();
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    sockets.last.open();
    expect(sockets.last.commandsOf("hello")).toEqual([expect.objectContaining({ token: TOKEN })]);
  });

  it("closes the socket in the background and connects again in the foreground", async () => {
    const { sockets, clock } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    connected({ sockets });
    const first = sockets.last;
    await act(async () => changeAppState("inactive"));
    expect(first.closed).toBe(false); // a Face ID prompt must not drop the link
    await act(async () => changeAppState("background"));
    expect(first.closed).toBe(true);
    expect(clock.pending).toBe(0); // no timer runs in the background
    await act(async () => changeAppState("active"));
    expect(sockets.all).toHaveLength(2);
    expect(sockets.last.closed).toBe(false);
  });

  it("clears the notifications it left in the tray each time the app comes to the foreground", async () => {
    const { sockets } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    connected({ sockets });
    n.dismissAllNotificationsAsync.mockClear();
    await act(async () => changeAppState("inactive"));
    await act(async () => changeAppState("background"));
    expect(n.dismissAllNotificationsAsync).not.toHaveBeenCalled();
    await act(async () => changeAppState("active"));
    expect(n.dismissAllNotificationsAsync).toHaveBeenCalledTimes(1);
    // One that cannot be cleared is no reason to stop: the app still connects.
    n.dismissAllNotificationsAsync.mockRejectedValueOnce(new Error("no native module"));
    await act(async () => changeAppState("background"));
    await act(async () => changeAppState("active"));
    expect(n.dismissAllNotificationsAsync).toHaveBeenCalledTimes(2);
    expect(sockets.last.closed).toBe(false);
  });

  it("revoked_unpairs: the Mac revoking the phone deletes the token and shows Pair with the reason", async () => {
    const { sockets } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    connected({ sockets });
    await act(async () => {
      sockets.last.receive({ type: "refused", req: 0, reason: "This device was revoked", code: "revoked" });
    });
    expect(await screen.findByText("Scan the code from octet pair")).toBeTruthy();
    expect(screen.getByText(UNPAIRED_NOTICE)).toBeTruthy();
    expect(screen.queryByText("Sessions list")).toBeNull();
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
  });

  it("a network drop leaves the user on Sessions with the token kept", async () => {
    const { sockets, clock } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    connected({ sockets });
    await act(async () => {
      sockets.last.drop();
      clock.advance(60_000);
    });
    expect(screen.getByText("Sessions list")).toBeTruthy();
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    await waitFor(() => expect(screen.queryByText(UNPAIRED_NOTICE)).toBeNull());
  });

  describe("the root view behind the screens (Android's, and iOS's under a sheet)", () => {
    it("takes the dark theme's background", async () => {
      jest.mocked(SystemUI.setBackgroundColorAsync).mockClear();
      jest.spyOn(ReactNative, "useColorScheme").mockReturnValue("dark");
      await launch();
      await screen.findByText("Scan the code from octet pair");
      expect(SystemUI.setBackgroundColorAsync).toHaveBeenLastCalledWith(dark.bg);
    });

    it("takes the light theme's background", async () => {
      jest.mocked(SystemUI.setBackgroundColorAsync).mockClear();
      jest.spyOn(ReactNative, "useColorScheme").mockReturnValue("light");
      await launch();
      await screen.findByText("Scan the code from octet pair");
      expect(SystemUI.setBackgroundColorAsync).toHaveBeenLastCalledWith(light.bg);
    });
  });
});

describe("notifications", () => {
  const tap = (data: unknown, id = "n1") =>
    ({ notification: { request: { identifier: id, content: { data } } } }) as never;
  const listener = () => n.addNotificationResponseReceivedListener.mock.calls[0]?.[0];
  const SESSION = "4821-1790000000";

  it("is configured at start-up: nothing shows in the foreground, and the splash lifts once the state is known", async () => {
    await launch({ paired: true });
    await screen.findByText("Sessions list");
    expect(n.setNotificationHandler).toHaveBeenCalledTimes(1);
    expect(SplashScreen.hideAsync).toHaveBeenCalled();
  });

  it("keeps the splash up while the saved pairing is being read", async () => {
    const hub = new HubController({
      factory: new Sockets().factory,
      store: createOctetStore(),
      platform: "ios",
      tokens: { load: () => new Promise(() => {}), save: async () => {}, clear: async () => {} },
    });
    const Layout = () => (
      <HubContext.Provider value={hub}>
        <RootLayout />
      </HubContext.Provider>
    );
    await renderRouter({ _layout: Layout, index: () => null, pair: Pair });
    expect(SplashScreen.hideAsync).not.toHaveBeenCalled();
  });

  it("a tap while paired opens that session", async () => {
    const { sockets, view } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    connected({ sockets });
    await act(async () => listener()?.(tap({ session: SESSION })));
    expect(view.getPathname()).toBe(`/session/${SESSION}`);
  });

  it("a tap while unpaired goes to Pair", async () => {
    const { view } = await launch();
    await screen.findByText("Scan the code from octet pair");
    await act(async () => listener()?.(tap({ session: SESSION })));
    expect(view.getPathname()).toBe("/pair");
  });

  it("a tap with bad data goes to Sessions and opens no session", async () => {
    const { view } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    await act(async () => router.push("/settings"));
    expect(view.getPathname()).toBe("/settings");
    await act(async () => listener()?.(tap({ session: "../../settings" })));
    expect(view.getPathname()).toBe("/");
  });

  it("a cold start from a tap opens the session once the app knows it is paired", async () => {
    n.getLastNotificationResponse.mockReturnValue(tap({ session: SESSION }));
    const { view } = await launch({ paired: true });
    await waitFor(() => expect(view.getPathname()).toBe(`/session/${SESSION}`));
  });

  it("a cold start from a tap while unpaired lands on Pair", async () => {
    n.getLastNotificationResponse.mockReturnValue(tap({ session: SESSION }));
    const { view } = await launch();
    await screen.findByText("Scan the code from octet pair");
    expect(view.getPathname()).toBe("/pair");
  });

  it("a cold start tap with bad data lands on Sessions", async () => {
    n.getLastNotificationResponse.mockReturnValue(tap({}));
    const { view } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    expect(view.getPathname()).toBe("/");
  });

  it("a cold start's tap is opened once, however often the shell mounts", async () => {
    let last: unknown = tap({ session: SESSION });
    n.getLastNotificationResponse.mockImplementation(() => last as never);
    n.clearLastNotificationResponse.mockImplementation(() => {
      last = null;
    });
    const first = await launch({ paired: true });
    await waitFor(() => expect(first.view.getPathname()).toBe(`/session/${SESSION}`));
    await act(async () => screen.unmount());
    const second = await launch({ paired: true });
    await screen.findByText("Sessions list");
    expect(second.view.getPathname()).toBe("/");
  });

  it("a tap during the notifications step waits, and opens after Not now", async () => {
    mockProject = "11111111-2222-3333-4444-555555555555";
    const { sockets, view } = await launch();
    await fireEvent.changeText(await screen.findByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    sockets.last.open();
    const [command] = sockets.last.commandsOf("pair");
    sockets.last.receive({ type: "paired", req: command?.req, token: TOKEN, device: "iPhone" });
    const skip = await screen.findByRole("button", { name: "Not now" });
    await act(async () => listener()?.(tap({ session: SESSION })));
    expect(view.getPathname()).toBe("/pair");
    await fireEvent.press(skip);
    await waitFor(() => expect(view.getPathname()).toBe(`/session/${SESSION}`));
  });

  it("stops listening when the shell goes", async () => {
    const remove = jest.fn();
    n.addNotificationResponseReceivedListener.mockReturnValue({ remove } as never);
    await launch({ paired: true });
    await screen.findByText("Sessions list");
    await act(async () => screen.unmount());
    expect(remove).toHaveBeenCalled();
  });

  it("pairing with a project id holds on the notifications step until it is answered, then goes to Sessions", async () => {
    mockProject = "11111111-2222-3333-4444-555555555555";
    const { sockets, view } = await launch();
    await fireEvent.changeText(await screen.findByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    sockets.last.open();
    const [command] = sockets.last.commandsOf("pair");
    sockets.last.receive({ type: "paired", req: command?.req, token: TOKEN, device: "iPhone" });
    const skip = await screen.findByRole("button", { name: "Not now" });
    expect(screen.queryByText("Sessions list")).toBeNull();
    expect(view.getPathname()).toBe("/pair");
    await fireEvent.press(skip);
    expect(await screen.findByText("Sessions list")).toBeTruthy();
    expect(view.getPathname()).toBe("/");
  });
});
