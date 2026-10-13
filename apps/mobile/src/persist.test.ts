import AsyncStorage from "@react-native-async-storage/async-storage";
import { DEFAULT_SETTINGS, SETTINGS_KEY, loadSettings, saveSettings } from "./persist";

jest.mock("@react-native-async-storage/async-storage", () =>
  jest.requireActual("@react-native-async-storage/async-storage/jest/async-storage-mock"),
);

beforeEach(async () => {
  await AsyncStorage.clear();
});

describe("settings", () => {
  it("default when nothing is saved", async () => {
    expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    expect(DEFAULT_SETTINGS).toEqual({ deviceName: null, faceId: true, hubUrl: null });
  });

  it("round-trip, and a save merges into what is there", async () => {
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    await saveSettings({ deviceName: "Shubho's iPhone", faceId: false });
    expect(await loadSettings()).toEqual({ deviceName: "Shubho's iPhone", faceId: false, hubUrl: "wss://mac.tail1234.ts.net/" });
  });

  it("fall back to the defaults for junk", async () => {
    for (const junk of ["not json", "[]", "null", JSON.stringify({ deviceName: 5, faceId: "yes", hubUrl: { a: 1 } })]) {
      await AsyncStorage.setItem(SETTINGS_KEY, junk);
      expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    }
  });

  it("keep the good fields of a partly bad record, and bound strings", async () => {
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ deviceName: "ok", faceId: 1, hubUrl: "u".repeat(100_000) }));
    const s = await loadSettings();
    expect(s.deviceName).toBe("ok");
    expect(s.faceId).toBe(true);
    expect(s.hubUrl).toBeNull();
  });

  it("do not throw when storage does", async () => {
    // Each rejection is used once; the mock keeps working after.
    jest.mocked(AsyncStorage.getItem).mockRejectedValueOnce(new Error("disk"));
    expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    jest.mocked(AsyncStorage.setItem).mockRejectedValueOnce(new Error("full"));
    await expect(saveSettings({ faceId: false })).resolves.toBeUndefined();
  });

  it("two saves at once both land", async () => {
    await Promise.all([saveSettings({ hubUrl: "wss://a/" }), saveSettings({ faceId: false }), saveSettings({ deviceName: "P" })]);
    expect(await loadSettings()).toEqual({ deviceName: "P", faceId: false, hubUrl: "wss://a/" });
  });

  it("never hold a token", async () => {
    await saveSettings({ hubUrl: "wss://x/" });
    const raw = (await AsyncStorage.getItem(SETTINGS_KEY)) ?? "";
    expect(Object.keys(JSON.parse(raw) as object).sort()).toEqual(["deviceName", "faceId", "hubUrl"]);
  });
});
