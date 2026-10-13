import { act, fireEvent, screen, waitFor } from "@testing-library/react-native";
import AsyncStorage from "@react-native-async-storage/async-storage";
import { renderRouter } from "expo-router/testing-library";
import * as SecureStore from "expo-secure-store";
import { AppState, Text } from "react-native";

import RootLayout from "../app/_layout";
import Pair from "../app/pair";
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY, UNPAIRED_NOTICE } from "./hub";
import { saveSettings } from "./persist";
import { createOctetStore } from "./store";

jest.mock("expo-camera", () => ({
  CameraView: () => null,
  useCameraPermissions: () => [{ granted: false, canAskAgain: false, status: "denied" }, jest.fn()],
}));

const TOKEN = "cd".repeat(32);
const LINK = `https://mac.tail1234.ts.net/#pair=${"ab".repeat(32)}`;

let changeAppState: (state: string) => void = () => {};

beforeEach(async () => {
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
  await renderRouter({ _layout: Layout, index: () => <Text>Sessions list</Text>, pair: Pair, "session/[id]": () => null });
  return { hub, sockets, clock };
}

/** Walks the connection to ready: hello, welcome. */
function connected({ sockets }: { sockets: Sockets }): void {
  sockets.last.open();
  sockets.last.receive({ type: "welcome", device: "iPhone" });
}

describe("the app shell", () => {
  it("opens on Pair without a token", async () => {
    const { sockets } = await launch();
    expect(await screen.findByText("Pair with your Mac")).toBeTruthy();
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
      { _layout: Layout, index: () => <Text>Sessions list</Text>, pair: Pair, "session/[id]": () => <Text>Session x</Text> },
      { initialUrl: "/session/x" },
    );
    await view;
    expect(await screen.findByText("Pair with your Mac")).toBeTruthy();
    expect(screen.queryByText("Session x")).toBeNull();
    expect(view.getPathname()).toBe("/pair");
    expect(sockets.all).toHaveLength(0);
  });

  it("opens on Sessions with a token, and connects once", async () => {
    const { sockets } = await launch({ paired: true });
    expect(await screen.findByText("Sessions list")).toBeTruthy();
    expect(screen.queryByText("Pair with your Mac")).toBeNull();
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

  it("revoked_unpairs: the Mac revoking the phone deletes the token and shows Pair with the reason", async () => {
    const { sockets } = await launch({ paired: true });
    await screen.findByText("Sessions list");
    connected({ sockets });
    await act(async () => {
      sockets.last.receive({ type: "refused", req: 0, reason: "This device was revoked", code: "revoked" });
    });
    expect(await screen.findByText("Pair with your Mac")).toBeTruthy();
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
});
