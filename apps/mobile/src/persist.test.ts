import AsyncStorage from "@react-native-async-storage/async-storage";
import { DEFAULT_PUSH_KINDS, DEFAULT_SETTINGS, SETTINGS_KEY, loadSettings, saveSettings } from "./persist";

beforeEach(async () => {
  await AsyncStorage.clear();
});

describe("settings", () => {
  it("default when nothing is saved", async () => {
    expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    expect(DEFAULT_SETTINGS).toEqual({
      deviceName: null,
      faceId: true,
      hubUrl: null,
      notifications: false,
      pushKinds: { approvals: true, errors: true, turns: false },
      textSize: 13,
    });
  });

  it("round-trip, and a save merges into what is there", async () => {
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    await saveSettings({ deviceName: "Shubho's iPhone", faceId: false });
    expect(await loadSettings()).toEqual({
      deviceName: "Shubho's iPhone",
      faceId: false,
      hubUrl: "wss://mac.tail1234.ts.net/",
      notifications: false,
      pushKinds: DEFAULT_PUSH_KINDS,
      textSize: 13,
    });
  });

  it("a save never writes the defaults over settings it could not read", async () => {
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    jest.mocked(AsyncStorage.getItem).mockRejectedValueOnce(new Error("busy"));
    expect(await saveSettings({ faceId: false })).toBe(false);
    expect(await loadSettings()).toMatchObject({ hubUrl: "wss://mac.tail1234.ts.net/", faceId: true });
  });

  it("a save replaces junk: it was read, as the defaults", async () => {
    for (const junk of ["not json", "[]", "null"]) {
      await AsyncStorage.setItem(SETTINGS_KEY, junk);
      expect(await saveSettings({ hubUrl: "wss://mac.tail1.ts.net/", deviceName: "p" })).toBe(true);
      expect(await loadSettings()).toMatchObject({ hubUrl: "wss://mac.tail1.ts.net/", deviceName: "p" });
    }
  });

  it("fall back to the defaults for junk", async () => {
    for (const junk of ["not json", "[]", "null", JSON.stringify({ deviceName: 5, faceId: "yes", hubUrl: { a: 1 } })]) {
      await AsyncStorage.setItem(SETTINGS_KEY, junk);
      expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    }
  });

  it("notifications are off by default, round-trip, and an old record without the field still loads", async () => {
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({ deviceName: "Old", faceId: false, hubUrl: "wss://mac.tail1234.ts.net/" }),
    );
    expect(await loadSettings()).toEqual({
      deviceName: "Old",
      faceId: false,
      hubUrl: "wss://mac.tail1234.ts.net/",
      notifications: false,
      pushKinds: DEFAULT_PUSH_KINDS,
      textSize: 13,
    });
    await saveSettings({ notifications: true });
    expect((await loadSettings()).notifications).toBe(true);
    expect((await loadSettings()).deviceName).toBe("Old");
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ notifications: "yes" }));
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("push kinds default to approvals and errors on, turns off, round-trip, and each junk field is its default", async () => {
    expect((await loadSettings()).pushKinds).toEqual({ approvals: true, errors: true, turns: false });
    await saveSettings({ pushKinds: { approvals: false, errors: true, turns: true } });
    expect((await loadSettings()).pushKinds).toEqual({ approvals: false, errors: true, turns: true });
    // Each field alone: one that is missing or not a boolean is the default's.
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ pushKinds: { approvals: false, errors: "yes" } }));
    expect((await loadSettings()).pushKinds).toEqual({ approvals: false, errors: true, turns: false });
    for (const junk of [null, 5, "all", [], [true, true, true]]) {
      await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ pushKinds: junk }));
      expect((await loadSettings()).pushKinds).toEqual(DEFAULT_PUSH_KINDS);
    }
  });

  it("the transcript text size is 13 by default, 12, 13 or 14 when saved, and 13 for anything else", async () => {
    expect((await loadSettings()).textSize).toBe(13);
    for (const size of [12, 14, 13] as const) {
      await saveSettings({ textSize: size });
      expect((await loadSettings()).textSize).toBe(size);
    }
    for (const junk of [15, 11, 13.5, "14", null]) {
      await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ textSize: junk }));
      expect((await loadSettings()).textSize).toBe(13);
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
    await expect(saveSettings({ faceId: false })).resolves.toBe(false);
    await expect(saveSettings({ faceId: false })).resolves.toBe(true);
  });

  it("two saves at once both land", async () => {
    await Promise.all([saveSettings({ hubUrl: "wss://a/" }), saveSettings({ faceId: false }), saveSettings({ deviceName: "P" })]);
    expect(await loadSettings()).toEqual({
      deviceName: "P",
      faceId: false,
      hubUrl: "wss://a/",
      notifications: false,
      pushKinds: DEFAULT_PUSH_KINDS,
      textSize: 13,
    });
  });

  it("never hold a token", async () => {
    await saveSettings({ hubUrl: "wss://x/" });
    const raw = (await AsyncStorage.getItem(SETTINGS_KEY)) ?? "";
    expect(Object.keys(JSON.parse(raw) as object).sort()).toEqual(["deviceName", "faceId", "hubUrl", "notifications", "pushKinds", "textSize"]);
  });
});
