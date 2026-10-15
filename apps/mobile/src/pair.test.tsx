import { act, fireEvent, render, screen, waitFor } from "@testing-library/react-native";
import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";
import * as Notifications from "expo-notifications";
import { Platform, StyleSheet } from "react-native";
import { SafeAreaProvider } from "react-native-safe-area-context";

import Pair from "../app/pair";
import { FakeClock, Sockets } from "./client/fakes";
import { DEV_BUILD_WARNING } from "./components/DevBuildBanner";
import { HubContext, HubController, TOKEN_KEY } from "./hub";
import { loadSettings, saveSettings } from "./persist";
import { createOctetStore } from "./store";
import { dark, light } from "./theme";
import { hideToast, toastStore } from "./toast";

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

let mockProject: string | undefined;
jest.mock("expo-constants", () => ({
  __esModule: true,
  default: {
    get expoConfig() {
      return { extra: { eas: { projectId: mockProject } } };
    },
  },
}));

const METRICS = { frame: { x: 0, y: 0, width: 390, height: 844 }, insets: { top: 47, left: 0, right: 0, bottom: 34 } };
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
    <SafeAreaProvider initialMetrics={METRICS}>
      <HubContext.Provider value={hub}>
        <Pair />
      </HubContext.Provider>
    </SafeAreaProvider>,
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

/** "Enter code instead": the paste and name fields (a scan or a missing camera shows them too). */
const enter = async () => {
  const button = screen.queryByRole("button", { name: "Enter code instead" });
  if (button) await fireEvent.press(button);
};
const paste = async (text: string) => {
  await enter();
  await fireEvent.changeText(screen.getByLabelText("Pairing link"), text);
};

const scan = async (data: string) => {
  const onScan = mockCamera.props?.onBarcodeScanned as ((result: { data: string }) => void) | undefined;
  await act(async () => onScan?.({ data }));
};

beforeEach(async () => {
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  jest.clearAllMocks();
  hideToast();
  mockCamera.props = null;
  mockProject = undefined;
});

describe("the Pair screen", () => {
  it("is a panel with 'Scan the code from octet pair', the camera in four accent corner brackets, and a sheet", async () => {
    await open();
    expect(screen.getByText("Scan the code from octet pair")).toBeTruthy();
    const style = (el: { props: { style?: unknown } }) => StyleSheet.flatten(el.props.style as never) as Record<string, unknown>;
    const frame = style(screen.getByTestId("scan-frame"));
    expect(frame.width).toBe(240);
    expect(frame.height).toBe(240);
    expect(screen.getByTestId("camera")).toBeTruthy();
    const corners = screen.getAllByTestId(/^scan-corner-/);
    expect(corners).toHaveLength(4);
    for (const corner of corners) expect([dark.accent, light.accent]).toContain(style(corner).borderColor);
    expect(screen.getByText("PAIRED")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Enter code instead" })).toBeTruthy();
  });

  it("keeps the paste and name fields away until 'Enter code instead'", async () => {
    await open();
    expect(screen.queryByLabelText("Pairing link")).toBeNull();
    expect(screen.queryByLabelText("Device name")).toBeNull();
    expect(screen.queryByRole("button", { name: "Pair" })).toBeNull();
    await fireEvent.press(screen.getByRole("button", { name: "Enter code instead" }));
    expect(screen.getByLabelText("Pairing link")).toBeTruthy();
    expect(screen.getByLabelText("Device name")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Pair" })).toBeTruthy();
  });

  it("shows the fields at once with the camera off, and says how to go on", async () => {
    await open(DENIED);
    expect(screen.getByLabelText("Pairing link")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Enter code instead" })).toBeNull();
  });

  it("an invalid scan is said in the sheet even with the fields away", async () => {
    await open();
    await scan("WIFI:S:cafe;;");
    expect(screen.getByText("That isn't an Octet pairing link")).toBeTruthy();
  });

  it("lists no Mac under PAIRED before a first pairing", async () => {
    await open();
    expect(screen.getByText("No Mac yet")).toBeTruthy();
  });

  it("lists the Mac this phone was paired with when it pairs again", async () => {
    await saveSettings({ hubUrl: "wss://studio-mac.tail1234.ts.net/" });
    await open();
    expect(await screen.findByText("○ studio-mac")).toBeTruthy();
    expect(screen.queryByText("No Mac yet")).toBeNull();
  });

  it("toasts 'Paired with <mac>' after a pairing, and not before", async () => {
    const rig = await open();
    await paste(LINK);
    expect(toastStore.getState().toast).toBeNull();
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(toastStore.getState().toast?.text).toBe("Paired with mac");
  });

  it("toasts nothing when the pairing fails", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.drop();
    expect(await screen.findByText("Can't reach your Mac")).toBeTruthy();
    expect(toastStore.getState().toast).toBeNull();
  });

  it("warns in a development build, and says nothing in a release build", async () => {
    const g = globalThis as { __DEV__?: boolean };
    const dev = g.__DEV__;
    try {
      g.__DEV__ = true;
      await open();
      expect(screen.getByText(DEV_BUILD_WARNING)).toBeTruthy();
      g.__DEV__ = false;
      await open();
      expect(screen.queryByText(DEV_BUILD_WARNING)).toBeNull();
    } finally {
      g.__DEV__ = dev;
    }
  });

  it("scans only QR codes", async () => {
    await open();
    expect(screen.getByTestId("camera")).toBeTruthy();
    expect(mockCamera.props?.barcodeScannerSettings).toEqual({ barcodeTypes: ["qr"] });
  });

  it("a scanned link asks which Mac before it pairs, and pairs only after the tap", async () => {
    const rig = await open();
    await scan(LINK);
    expect(screen.getByText("Pair with mac.tail1234.ts.net?")).toBeTruthy();
    expect(screen.getByLabelText("Pairing link").props.value).toBe(LINK);
    expect(rig.sockets.all).toHaveLength(0);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    expect((await loadSettings()).hubUrl).toBe("wss://mac.tail1234.ts.net/");
  });

  it("Cancel drops a scanned link without pairing", async () => {
    const rig = await open();
    await scan(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("Pair with mac.tail1234.ts.net?")).toBeNull();
    expect(screen.getByLabelText("Pairing link").props.value).toBe("");
    expect(rig.sockets.all).toHaveLength(0);
    // The camera still sees the same code: it does not ask again until it sees another.
    await scan(LINK);
    expect(screen.queryByText("Pair with mac.tail1234.ts.net?")).toBeNull();
  });

  it("a pasted link shows its host, and Pair is off until there is a valid link", async () => {
    const rig = await open();
    await enter();
    expect(screen.getByRole("button", { name: "Pair" }).props.accessibilityState.disabled).toBe(true);
    await paste(LINK);
    expect(screen.getByText("Pair with mac.tail1234.ts.net?")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Pair" }).props.accessibilityState.disabled).toBe(false);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
  });

  it("an invalid pasted link says so, keeps Pair off and opens nothing", async () => {
    const { sockets } = await open();
    await paste("https://example.com/");
    expect(screen.getByText("That isn't an Octet pairing link")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Pair" }).props.accessibilityState.disabled).toBe(true);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    expect(sockets.all).toHaveLength(0);
    expect(mockRouter.replace).not.toHaveBeenCalled();
  });

  it("a link to a host that is not a .ts.net name says where pair links come from, and opens nothing", async () => {
    const { sockets } = await open();
    await scan(`https://mac.tail1234.ts.net.evil.example/#pair=${SECRET}`);
    expect(screen.getByText("Pair links come from tailscale serve (a .ts.net name)")).toBeTruthy();
    await paste(`https://100.64.0.7:8443/#pair=${SECRET}`);
    expect(screen.getByText("Pair links come from tailscale serve (a .ts.net name)")).toBeTruthy();
    expect(screen.queryByText(/^Pair with .*\?$/)).toBeNull();
    expect(screen.getByRole("button", { name: "Pair" }).props.accessibilityState.disabled).toBe(true);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    expect(sockets.all).toHaveLength(0);
  });

  it("an invalid scan says so, once however often the camera sees it", async () => {
    const { sockets } = await open();
    await scan("WIFI:S:cafe;;");
    await scan("WIFI:S:cafe;;");
    expect(screen.getAllByText("That isn't an Octet pairing link")).toHaveLength(1);
    expect(screen.queryByText(/^Pair with .*\?$/)).toBeNull();
    expect(sockets.all).toHaveLength(0);
  });

  it("after a spent code's refusal the link and the scan are forgotten, and the same code asks again", async () => {
    const rig = await open();
    await scan(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "That pairing code has expired", code: "unpaired" });
    expect(await screen.findByText("That pairing code has expired")).toBeTruthy();
    expect(screen.getByLabelText("Pairing link").props.value).toBe("");
    expect(screen.queryByText("Pair with mac.tail1234.ts.net?")).toBeNull();
    await scan(LINK);
    expect(screen.getByText("Pair with mac.tail1234.ts.net?")).toBeTruthy();
    expect(rig.sockets.all).toHaveLength(1); // asked, not tried
  });

  it("tells how to recover when the phone cannot keep the token, and forgets the link", async () => {
    jest.mocked(SecureStore.setItemAsync).mockRejectedValueOnce(new Error("locked")).mockRejectedValueOnce(new Error("locked"));
    const rig = await open();
    await paste(LINK);
    await enter();
    await fireEvent.changeText(screen.getByLabelText("Device name"), "Work phone");
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    expect(
      await screen.findByText(
        "This phone paired but couldn't save it. On the Mac run `octet hub revoke Work phone`, then pair again with a new code.",
      ),
    ).toBeTruthy();
    expect(screen.getByLabelText("Pairing link").props.value).toBe("");
    expect(mockRouter.replace).not.toHaveBeenCalled();
  });

  it("keeps the link when the Mac could not be reached", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.drop();
    expect(await screen.findByText("Can't reach your Mac")).toBeTruthy();
    expect(screen.getByLabelText("Pairing link").props.value).toBe(LINK);
  });

  it("shows the hub's words when it refuses", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "A device with that name is already paired", code: "invalid" });
    expect(await screen.findByText("A device with that name is already paired")).toBeTruthy();
    expect(mockRouter.replace).not.toHaveBeenCalled();
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
    // The Mac kept the code: the link stays, to pair again under another name.
    expect(screen.getByLabelText("Pairing link").props.value).toBe(LINK);
  });

  it("forgets the link when the Mac says its code is spent or expired", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "Pairing code already used or expired", code: "unpaired" });
    expect(await screen.findByText("Pairing code already used or expired")).toBeTruthy();
    expect(screen.getByLabelText("Pairing link").props.value).toBe("");
  });

  it("works with the camera off: the paste field stays usable", async () => {
    const rig = await open(DENIED);
    expect(screen.queryByTestId("camera")).toBeNull();
    expect(screen.getByText("Camera access is off — paste the link instead")).toBeTruthy();
    await paste(LINK);
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
    await enter();
    expect(screen.getByLabelText("Device name").props.value).toBe("iPhone");
    await fireEvent.changeText(screen.getByLabelText("Device name"), "Work phone");
    await paste(LINK);
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
      await enter();
      expect(screen.getByLabelText("Device name").props.value).toBe("Android phone");
    } finally {
      replaced.restore();
    }
  });

  it("does not pair twice while it waits for the Mac", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    expect(rig.sockets.all).toHaveLength(1);
  });

  it("shows hub text as plain text, never as a link", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    rig.sockets.last.open();
    const [command] = rig.sockets.last.commandsOf("pair");
    rig.sockets.last.receive({ type: "refused", req: command?.req, reason: "<a href=x>[go](https://evil.test)</a>", code: "invalid" });
    const shown = await screen.findByText("<a href=x>[go](https://evil.test)</a>");
    expect(shown.props.onPress).toBeUndefined();
  });
});

describe("the notifications step after pairing", () => {
  const PROJECT = "11111111-2222-3333-4444-555555555555";
  const notifications = Notifications as jest.Mocked<typeof Notifications>;

  /** Pairs on a Mac that then welcomes the phone, and lands on the step. */
  async function pairedToStep() {
    mockProject = PROJECT;
    const rig = await open();
    rig.hub.setForeground(true);
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await screen.findByRole("button", { name: "Turn on notifications" });
    rig.sockets.last.open();
    rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
    return rig;
  }
  const ack = async (rig: Awaited<ReturnType<typeof open>>, message: string) => {
    await waitFor(() => expect(rig.sockets.last.commandsOf("register_push")).toHaveLength(1));
    const [command] = rig.sockets.last.commandsOf("register_push");
    await act(async () => rig.sockets.last.receive({ type: "ack", req: command?.req, message }));
  };

  it("follows a pairing when the app has a project id, instead of going straight on", async () => {
    await pairedToStep();
    expect(screen.getByRole("button", { name: "Not now" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Pair" })).toBeNull();
    expect(mockRouter.replace).not.toHaveBeenCalled();
  });

  it("Not now goes on to Sessions without asking for anything", async () => {
    const rig = await pairedToStep();
    await fireEvent.press(screen.getByRole("button", { name: "Not now" }));
    expect(mockRouter.replace).toHaveBeenCalledWith("/");
    expect(notifications.requestPermissionsAsync).not.toHaveBeenCalled();
    expect(rig.sockets.last.commandsOf("register_push")).toHaveLength(0);
    expect((await loadSettings()).notifications).toBe(false);
    expect(rig.hub.snapshot().offer).toBe(false);
  });

  it("Turn on notifications asks, registers, saves, and goes on to Sessions", async () => {
    const rig = await pairedToStep();
    await fireEvent.press(screen.getByRole("button", { name: "Turn on notifications" }));
    await ack(rig, "Notifications on");
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(notifications.requestPermissionsAsync).toHaveBeenCalledTimes(1);
    expect((await loadSettings()).notifications).toBe(true);
  });

  it("a denial is said here and the step stays, so that Not now is still there", async () => {
    notifications.requestPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    await pairedToStep();
    await fireEvent.press(screen.getByRole("button", { name: "Turn on notifications" }));
    expect(await screen.findByText("Notifications are off for Octet in the phone's settings.")).toBeTruthy();
    expect(mockRouter.replace).not.toHaveBeenCalled();
    await fireEvent.press(screen.getByRole("button", { name: "Not now" }));
    expect(mockRouter.replace).toHaveBeenCalledWith("/");
  });

  it("is skipped with no project id", async () => {
    const rig = await open();
    await paste(LINK);
    await fireEvent.press(screen.getByRole("button", { name: "Pair" }));
    accept(rig);
    await waitFor(() => expect(mockRouter.replace).toHaveBeenCalledWith("/"));
    expect(screen.queryByRole("button", { name: "Turn on notifications" })).toBeNull();
  });
});
