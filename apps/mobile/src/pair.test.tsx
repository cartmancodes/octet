import { act, fireEvent, render, screen, waitFor } from "@testing-library/react-native";
import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";
import { Platform } from "react-native";

import Pair from "../app/pair";
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY } from "./hub";
import { loadSettings } from "./persist";
import { createOctetStore } from "./store";

const mockRouter = { replace: jest.fn(), push: jest.fn() };
jest.mock("expo-router", () => ({ useRouter: () => mockRouter }));

type Permission = { granted: boolean; canAskAgain: boolean; status: string } | null;
const mockCamera: { permission: Permission; request: jest.Mock; props: Record<string, unknown> | null } = {
  permission: null,
  request: jest.fn(),
  props: null,
};
jest.mock("expo-camera", () => {
  const { View } = jest.requireActual("react-native");
  return {
    CameraView: (props: Record<string, unknown>) => {
      mockCamera.props = props;
      return <View testID="camera" />;
    },
    useCameraPermissions: () => [mockCamera.permission, mockCamera.request],
  };
});

const SECRET = "ab".repeat(32);
const LINK = `https://mac.tail1234.ts.net/#pair=${SECRET}`;
const TOKEN = "cd".repeat(32);
const GRANTED = { granted: true, canAskAgain: true, status: "granted" };
const DENIED = { granted: false, canAskAgain: false, status: "denied" };

async function open(permission: Permission = GRANTED) {
  mockCamera.permission = permission;
  const sockets = new Sockets();
  const clock = new FakeClock();
  const hub = new HubController({
    factory: sockets.factory,
    store: createOctetStore(),
    clientOptions: { now: clock.now, schedule: clock.schedule },
  });
  await hub.init();
  await render(
    <HubContext.Provider value={hub}>
      <Pair />
    </HubContext.Provider>,
  );
  return { sockets, hub };
}

/** The Mac accepts the pairing the app just sent. */
function accept({ sockets }: { sockets: Sockets }): void {
  const socket = sockets.last;
  socket.open();
  const [command] = socket.commandsOf("pair");
  socket.receive({ type: "paired", req: command?.req, token: TOKEN, device: "iPhone" });
}

const scan = async (data: string) => {
  const onScan = mockCamera.props?.onBarcodeScanned as ((result: { data: string }) => void) | undefined;
  await act(async () => onScan?.({ data }));
};

beforeEach(async () => {
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  jest.clearAllMocks();
  mockCamera.props = null;
});

describe("the Pair screen", () => {
  it("scans only QR codes", async () => {
    await open();
    expect(screen.getByTestId("camera")).toBeTruthy();
    expect(mockCamera.props?.barcodeScannerSettings).toEqual({ barcodeTypes: ["qr"] });
  });

  it("a scanned link pairs, keeps the token in the keychain and goes to Sessions", async () => {
    const rig = await open();
    await scan(LINK);
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    expect((await loadSettings()).hubUrl).toBe("wss://mac.tail1234.ts.net/");
  });

  it("a pasted link pairs with the Pair button", async () => {
    const rig = await open();
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
  });

  it("an invalid pasted link says so and opens nothing", async () => {
    const { sockets } = await open();
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), "https://example.com/");
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    expect(await screen.findByText("That isn't an Octet pairing link")).toBeTruthy();
    expect(sockets.all).toHaveLength(0);
    expect(mockRouter.replace).not.toHaveBeenCalled();
  });

  it("an invalid scan says so, once however often the camera sees it", async () => {
    const { sockets } = await open();
    await scan("WIFI:S:cafe;;");
    await scan("WIFI:S:cafe;;");
    expect(await screen.findByText("That isn't an Octet pairing link")).toBeTruthy();
    expect(sockets.all).toHaveLength(0);
  });

  it("a refused scan is not tried again while the camera still sees it", async () => {
    const rig = await open();
    await scan(LINK);
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "That pairing code has expired", code: "invalid" });
    expect(await screen.findByText("That pairing code has expired")).toBeTruthy();
    await scan(LINK);
    await scan(LINK);
    expect(rig.sockets.all).toHaveLength(1);
  });

  it("shows the hub's words when it refuses", async () => {
    const rig = await open();
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "A device with that name is already paired", code: "invalid" });
    expect(await screen.findByText("A device with that name is already paired")).toBeTruthy();
    expect(mockRouter.replace).not.toHaveBeenCalled();
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
  });

  it("works with the camera off: the paste field stays usable", async () => {
    const rig = await open(DENIED);
    expect(screen.queryByTestId("camera")).toBeNull();
    expect(screen.getByText("Camera access is off — paste the link instead")).toBeTruthy();
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
  });

  it("asks for the camera when it has not been asked", async () => {
    await open({ granted: false, canAskAgain: true, status: "undetermined" });
    expect(mockCamera.request).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId("camera")).toBeNull();
  });

  it("names the device by platform, and sends the name it was given", async () => {
    const rig = await open();
    expect(screen.getByLabelText("Device name").props.value).toBe("iPhone");
    await fireEvent.changeText(screen.getByLabelText("Device name"), "Work phone");
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    expect(rig.sockets.last.commandsOf("pair")).toEqual([
      expect.objectContaining({ name: "Work phone", platform: "ios" }),
    ]);
  });

  it("names an Android phone 'Android phone'", async () => {
    const replaced = jest.replaceProperty(Platform, "OS", "android");
    try {
      await open();
      expect(screen.getByLabelText("Device name").props.value).toBe("Android phone");
    } finally {
      replaced.restore();
    }
  });

  it("does not pair twice while it waits for the Mac", async () => {
    const rig = await open();
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    expect(rig.sockets.all).toHaveLength(1);
  });

  it("shows hub text as plain text, never as a link", async () => {
    const rig = await open();
    await fireEvent.changeText(screen.getByLabelText("Pairing link"), LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "<a href=x>[go](https://evil.test)</a>", code: "invalid" });
    const shown = await screen.findByText("<a href=x>[go](https://evil.test)</a>");
    expect(shown.props.onPress).toBeUndefined();
  });
});
