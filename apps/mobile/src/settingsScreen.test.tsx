import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, screen } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { router } from "expo-router";
import * as SecureStore from "expo-secure-store";
import { Alert, type AlertButton } from "react-native";

import Settings from "../app/settings";
import { TOKEN_KEY } from "./hub";
import { loadSettings, saveSettings } from "./persist";
import { info, launch, pairedStorage } from "./screenRig";

let mockConfig: { version?: string } | null = { version: "4.5.6" };
jest.mock("expo-constants", () => ({
  __esModule: true,
  default: {
    get expoConfig() {
      return mockConfig;
    },
  },
}));

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

beforeEach(async () => {
  await pairedStorage();
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
});
afterEach(() => jest.restoreAllMocks());

const flush = () => act(async () => void jest.advanceTimersByTime(60));

async function open() {
  const rig = await launch("/", { settings: Settings }, [info("live")]);
  await act(async () => router.push("/settings"));
  await flush();
  return rig;
}
const toggle = () => screen.getByRole("switch", { name: "Face ID to allow" });
const turn = async (on: boolean) => {
  await fireEvent(toggle(), "valueChange", on);
  await flush();
};

describe("Settings", () => {
  it("shows the hub's host only and the device name", async () => {
    await open();
    expect(screen.getByText("mac.tail1234.ts.net")).toBeTruthy();
    expect(screen.queryByText(/wss:/)).toBeNull();
    expect(screen.getByText("Shubh's iPhone")).toBeTruthy();
  });

  it("shows the app version from the app's config", async () => {
    await open();
    expect(screen.getByText("Version 4.5.6")).toBeTruthy();
  });

  it("shows no version line when the config has none", async () => {
    mockConfig = null;
    await open();
    expect(screen.queryByText(/^Version/)).toBeNull();
    mockConfig = { version: "4.5.6" };
  });

  it("has Face ID to allow on by default", async () => {
    await open();
    expect(toggle().props.value).toBe(true);
  });

  it("shows what was saved", async () => {
    await saveSettings({ faceId: false });
    await open();
    expect(toggle().props.value).toBe(false);
  });

  describe("turning Face ID off", () => {
    it("asks for biometrics first and then saves it off", async () => {
      await open();
      await turn(false);
      expect(auth.authenticateAsync).toHaveBeenCalledWith({ promptMessage: "Turn off Face ID for allowing" });
      expect(toggle().props.value).toBe(false);
      expect((await loadSettings()).faceId).toBe(false);
    });

    it("stays on, and saved on, when biometrics fail", async () => {
      auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" } as never);
      await open();
      await turn(false);
      expect(toggle().props.value).toBe(true);
      expect((await loadSettings()).faceId).toBe(true);
    });

    it("saves it off when the phone has no biometrics: the user cannot use Face ID anyway", async () => {
      auth.isEnrolledAsync.mockResolvedValueOnce(false);
      await open();
      await turn(false);
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(toggle().props.value).toBe(false);
      expect((await loadSettings()).faceId).toBe(false);
    });

    it("gives one prompt for a double flip", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      await open();
      await fireEvent(toggle(), "valueChange", false);
      await fireEvent(toggle(), "valueChange", false);
      await act(async () => finish({ success: true }));
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    });
  });

  it("turning it on never asks", async () => {
    await saveSettings({ faceId: false });
    await open();
    await turn(true);
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(toggle().props.value).toBe(true);
    expect((await loadSettings()).faceId).toBe(true);
  });

  it("keeps the old setting and says so when it cannot be saved", async () => {
    await open();
    jest.spyOn(AsyncStorage, "setItem").mockRejectedValueOnce(new Error("disk full"));
    await turn(false);
    expect(toggle().props.value).toBe(true);
    expect(screen.getByText("Couldn't save that setting")).toBeTruthy();
  });

  describe("Unpair", () => {
    const confirm = () => {
      const alert = jest.spyOn(Alert, "alert").mockImplementation(() => {});
      return alert;
    };
    const buttons = (alert: jest.SpyInstance): AlertButton[] => (alert.mock.calls.at(-1)?.[2] ?? []) as AlertButton[];

    it("asks first, in these words, with a destructive Unpair", async () => {
      const alert = confirm();
      await open();
      await fireEvent.press(screen.getByRole("button", { name: "Unpair" }));
      expect(alert).toHaveBeenCalledTimes(1);
      expect(alert.mock.calls[0]?.[0]).toBe("Unpair this phone?");
      expect(alert.mock.calls[0]?.[1]).toBe(
        "You'll need a new pairing code from the Mac. The Mac keeps listing this phone until you run `octet hub revoke <name>` there.",
      );
      expect(buttons(alert).map((b) => [b.text, b.style])).toEqual([
        ["Cancel", "cancel"],
        ["Unpair", "destructive"],
      ]);
      expect(await SecureStore.getItemAsync(TOKEN_KEY)).not.toBeNull(); // nothing yet
    });

    it("Cancel changes nothing", async () => {
      const alert = confirm();
      const rig = await open();
      await fireEvent.press(screen.getByRole("button", { name: "Unpair" }));
      await act(async () => buttons(alert)[0]?.onPress?.());
      expect(await SecureStore.getItemAsync(TOKEN_KEY)).not.toBeNull();
      expect(rig.hub.snapshot().status).toBe("paired");
      expect(rig.view.getPathname()).toBe("/settings");
    });

    it("Unpair clears the token, the hub address and the cursors, and shows Pair", async () => {
      const alert = confirm();
      const rig = await open();
      await fireEvent.press(screen.getByRole("button", { name: "Unpair" }));
      await act(async () => {
        buttons(alert)[1]?.onPress?.();
      });
      await flush();
      expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
      expect((await loadSettings()).hubUrl).toBeNull();
      expect(rig.hub.snapshot()).toEqual({ status: "unpaired", notice: null, client: null });
      expect(await screen.findByText("Pair with your Mac")).toBeTruthy();
      expect(rig.view.getPathname()).toBe("/pair");
      expect(screen.queryByText("This phone was unpaired on the Mac")).toBeNull();
    });
  });

  it("sizes its controls for a thumb", async () => {
    await open();
    const style = (el: { props: { style?: unknown } }) => (Array.isArray(el.props.style) ? el.props.style : [el.props.style]).flat();
    const unpair = screen.getByRole("button", { name: "Unpair" });
    expect(Math.max(...style(unpair).map((s: { minHeight?: number } | undefined) => s?.minHeight ?? 0))).toBeGreaterThanOrEqual(44);
  });
});
