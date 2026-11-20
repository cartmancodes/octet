import AsyncStorage from "@react-native-async-storage/async-storage";
import { DEFAULT_PUSH_KINDS, DEFAULT_SETTINGS, SETTINGS_KEY, loadSettings, readSettings, saveSettings } from "./persist";

beforeEach(async () => {
  await AsyncStorage.clear();
});

describe("settings", () => {
  it("default when nothing is saved", async () => {
    expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    expect(DEFAULT_SETTINGS).toEqual({
      deviceName: null,
      faceId: true,
      beforeKeys: false,
      pairings: [],
      forget: [],
      notifications: false,
      pushKinds: { approvals: true, errors: true, turns: false },
      textSize: 13,
      liveActivity: true,
      liveActivityMac: null,
    });
  });

  it("round-trip, and a save merges into what is there", async () => {
    await saveSettings({ textSize: 14 });
    await saveSettings({ deviceName: "Shubho's iPhone", faceId: false });
    expect(await loadSettings()).toEqual({
      deviceName: "Shubho's iPhone",
      faceId: false,
      beforeKeys: false,
      pairings: [],
      forget: [],
      notifications: false,
      pushKinds: DEFAULT_PUSH_KINDS,
      textSize: 14,
      liveActivity: true,
      liveActivityMac: null,
    });
  });

  it("a save never writes the defaults over settings it could not read", async () => {
    await saveSettings({ deviceName: "Mine" });
    jest.mocked(AsyncStorage.getItem).mockRejectedValueOnce(new Error("busy"));
    expect(await saveSettings({ faceId: false })).toBe(false);
    expect(await loadSettings()).toMatchObject({ deviceName: "Mine", faceId: true });
  });

  it("a save replaces junk: it was read, as the defaults", async () => {
    for (const junk of ["not json", "[]", "null"]) {
      await AsyncStorage.setItem(SETTINGS_KEY, junk);
      expect(await saveSettings({ textSize: 12, deviceName: "p" })).toBe(true);
      expect(await loadSettings()).toMatchObject({ textSize: 12, deviceName: "p" });
    }
  });

  it("fall back to the defaults for junk", async () => {
    for (const junk of ["not json", "[]", "null", JSON.stringify({ deviceName: 5, faceId: "yes", pairings: { a: 1 } })]) {
      await AsyncStorage.setItem(SETTINGS_KEY, junk);
      expect(await loadSettings()).toEqual(DEFAULT_SETTINGS);
    }
  });

  it("the Live Activity is on by default, round-trips, and junk is the default", async () => {
    expect((await loadSettings()).liveActivity).toBe(true);
    await saveSettings({ liveActivity: false });
    expect((await loadSettings()).liveActivity).toBe(false);
    await saveSettings({ deviceName: "p" });
    expect((await loadSettings()).liveActivity).toBe(false);
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ liveActivity: "no" }));
    expect((await loadSettings()).liveActivity).toBe(true);
  });

  it("the Mac that drives the Live Activity is none by default, round-trips, and only a Mac id is kept", async () => {
    expect((await loadSettings()).liveActivityMac).toBeNull();
    await saveSettings({ liveActivityMac: "m0000000a" });
    expect((await loadSettings()).liveActivityMac).toBe("m0000000a");
    for (const junk of ["../x", 5, "M0000000A"]) {
      await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ liveActivityMac: junk }));
      expect((await loadSettings()).liveActivityMac).toBeNull();
    }
  });

  it("notifications are off by default, round-trip, and an old record without the field still loads", async () => {
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({ deviceName: "Old", faceId: false }),
    );
    expect(await loadSettings()).toEqual({
      deviceName: "Old",
      faceId: false,
      beforeKeys: false,
      pairings: [],
      forget: [],
      notifications: false,
      pushKinds: DEFAULT_PUSH_KINDS,
      textSize: 13,
      liveActivity: true,
      liveActivityMac: null,
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

  it("an app upgraded from before push kinds keeps what the Mac was pushing it: all three while notifications are on", async () => {
    const before = { deviceName: "Old", faceId: true, hubUrl: "wss://relay.example.test/", textSize: 13 };
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ ...before, notifications: true }));
    expect((await loadSettings()).pushKinds).toEqual({ approvals: true, errors: true, turns: true });
    // A save writes them down, so they stay what they were.
    await saveSettings({ textSize: 14 });
    expect(JSON.parse((await AsyncStorage.getItem(SETTINGS_KEY)) ?? "null").pushKinds).toEqual({
      approvals: true,
      errors: true,
      turns: true,
    });
    // Notifications off: nothing was being pushed, so the kinds are a new pairing's.
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ ...before, notifications: false }));
    expect((await loadSettings()).pushKinds).toEqual(DEFAULT_PUSH_KINDS);
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify(before));
    expect((await loadSettings()).pushKinds).toEqual(DEFAULT_PUSH_KINDS);
    // Kinds that were saved are kept, notifications on or not.
    const kinds = { approvals: true, errors: false, turns: false };
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ ...before, notifications: true, pushKinds: kinds }));
    expect((await loadSettings()).pushKinds).toEqual(kinds);
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
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ deviceName: "ok", faceId: 1, liveActivityMac: "u".repeat(100_000) }));
    const s = await loadSettings();
    expect(s.deviceName).toBe("ok");
    expect(s.faceId).toBe(true);
    expect(s.liveActivityMac).toBeNull();
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ deviceName: "u".repeat(100_000) }));
    expect((await loadSettings()).deviceName).toBeNull();
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
    await Promise.all([saveSettings({ textSize: 12 }), saveSettings({ faceId: false }), saveSettings({ deviceName: "P" })]);
    expect(await loadSettings()).toEqual({
      deviceName: "P",
      faceId: false,
      beforeKeys: false,
      pairings: [],
      forget: [],
      notifications: false,
      pushKinds: DEFAULT_PUSH_KINDS,
      textSize: 12,
      liveActivity: true,
      liveActivityMac: null,
    });
  });

  it("never hold a key, and no longer write a hub address", async () => {
    await saveSettings({ deviceName: "x" });
    const raw = (await AsyncStorage.getItem(SETTINGS_KEY)) ?? "";
    expect(Object.keys(JSON.parse(raw) as object).sort()).toEqual(["deviceName", "faceId", "forget", "liveActivity", "liveActivityMac", "notifications", "pairings", "pushKinds", "textSize"]);
  });
});

describe("pairings", () => {
  const hex = (byte: string) => byte.repeat(32);
  const p = (id: string, host = id, extra: object = {}) => ({
    id,
    relay: `wss://${host}.relay.test/`,
    channel: hex(id.slice(-2)),
    hubKey: hex("07"),
    deviceName: "iPhone",
    macName: host,
    ...extra,
  });

  it("round-trip, at most 4, duplicates by id or channel dropped, each cleaned", async () => {
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({
        pairings: [
          p("m00000001", "studio"),
          p("m00000001", "again"), // the same id: the first stays
          p("m000000a1", "twin", { channel: hex("01") }), // the same channel: the first stays
          "junk",
          { id: "m00000002" }, // no relay, channel or key
          p("not-an-id"),
          p("m00000003", "laptop", { macName: "  Lap\u0007top\n " }),
          p("m00000004", "mini", { macName: "" }), // no name: "Mac"
          p("m00000005", "five"),
          p("m00000006", "six"), // past four
        ],
      }),
    );
    const s = await loadSettings();
    expect(s.pairings.map((x) => [x.id, x.macName])).toEqual([
      ["m00000001", "studio"],
      ["m00000003", "Laptop"],
      ["m00000004", "Mac"],
      ["m00000005", "five"],
    ]);
    expect(s.pairings[0]).toEqual({
      id: "m00000001",
      relay: "wss://studio.relay.test/",
      channel: hex("01"),
      hubKey: hex("07"),
      deviceName: "iPhone",
      macName: "studio",
    });
    expect(await saveSettings({ faceId: false })).toBe(true);
    expect((await loadSettings()).pairings).toEqual(s.pairings);
  });

  it("take a channel and a hub key only as 64 lowercase hex digits, and a relay only as text", async () => {
    for (const bad of [
      { channel: hex("0A") },
      { channel: hex("0").slice(1) },
      { hubKey: "zz".repeat(32) },
      { hubKey: 5 },
      { relay: 7 },
      { relay: "" },
      { deviceName: null },
    ]) {
      await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ pairings: [p("m00000001", "studio", bad)] }));
      expect([bad, (await loadSettings()).pairings]).toEqual([bad, []]);
    }
  });

  it("are not a list: none", async () => {
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ pairings: { a: 1 } }));
    expect((await loadSettings()).pairings).toEqual([]);
  });

  it("keep a long name to 40 characters", async () => {
    await AsyncStorage.setItem(SETTINGS_KEY, JSON.stringify({ pairings: [p("m0000000a", "studio", { macName: "y".repeat(90) })] }));
    expect((await loadSettings()).pairings[0]?.macName).toBe("y".repeat(40));
  });

  it("pairings_from_before_keys_are_dropped_and_their_ids_are_to_be_forgotten", async () => {
    // What the app wrote before keys: a pairing by its hub's address, whose token is in the
    // keychain under its id, and the address of the one pairing before there could be several.
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({
        deviceName: "Old",
        hubUrl: null,
        pairings: [
          { id: "m0000000a", hubUrl: "wss://studio.tail1.ts.net/", deviceName: "Old", macName: "studio" },
          p("m00000001", "new"),
        ],
        forget: ["m0000000c"],
      }),
    );
    const read = await readSettings();
    if (!read.ok) throw new Error("not read");
    expect(read.settings.beforeKeys).toBe(true);
    expect(read.settings.pairings.map((x) => x.id)).toEqual(["m00000001"]);
    expect(read.settings.forget).toEqual(["m0000000c", "m0000000a"]);
    // A save writes the record of this version: the old pairing and the address are gone, and the
    // record no longer reads as one from before keys.
    expect(await saveSettings({})).toBe(true);
    const raw = JSON.parse((await AsyncStorage.getItem(SETTINGS_KEY)) ?? "{}") as Record<string, unknown>;
    expect(raw).not.toHaveProperty("hubUrl");
    expect((raw.pairings as { id: string }[]).map((x) => x.id)).toEqual(["m00000001"]);
    expect((await loadSettings()).beforeKeys).toBe(false);
  });
});
