// The Fleet: up to four pairings, each its own link (client, keychain entry, push sync), all open
// in the foreground. The single-Mac behaviour each link keeps is in src/hub.test.ts.
import AsyncStorage from "@react-native-async-storage/async-storage";
import * as Notifications from "expo-notifications";
import * as SecureStore from "expo-secure-store";

import { FakeClock, Sockets, channelOf, hubKeyOf, pairLinkTo, phoneUrlOf, type FakeSocket } from "./client/fakes";
import { fromHex } from "./client/link";
import { newKeyPair } from "./client/noise";
import { Fleet, NOT_FROM_THAT_MAC, TOKEN_KEY, tokenKeyOf, unpairedNotice, type Keychain } from "./hub";
import { MAC_ID, TOO_MANY_MACS, keyOf, macStatus } from "./macs";
import { SETTINGS_KEY, loadSettings, saveSettings } from "./persist";
import { createOctetStore } from "./store";
import { showToast, toastStore } from "./toast";

jest.mock("expo-constants", () => ({
  __esModule: true,
  default: { expoConfig: { extra: { eas: { projectId: "11111111-2222-3333-4444-555555555555" } } } },
}));

const SECRET = "ab".repeat(32);
const TOKEN = "cd".repeat(32);
const PUSH = "ExponentPushToken[abc-123_XYZ]";
const n = Notifications as jest.Mocked<typeof Notifications>;

/** Every Mac of these tests is reached through the one relay, as on the hosted relay: only its
 * channel id tells one from another. */
const RELAY = "relay.example.test";
const relayOf = (_host: string) => RELAY;
const linkTo = (host: string) => pairLinkTo(host, SECRET, { relay: relayOf(host) });
/** The address of the phone sockets to the Mac on `host`. */
const urlOf = (host: string) => phoneUrlOf(host, relayOf(host));

const madeKeys = new Map<string, ReturnType<typeof newKeyPair>>();
/** The key pair whose private half is `hex`, made once (a test may take the random source away). */
function keyOfHex(hex: string) {
  let key = madeKeys.get(hex);
  if (!key) {
    key = newKeyPair(() => fromHex(hex) ?? new Uint8Array(32));
    madeKeys.set(hex, key);
  }
  return key;
}

/** Ids in order: m00000001, m00000002, ... */
function ids() {
  let next = 0;
  return () => `m${(++next).toString(16).padStart(8, "0")}`;
}

/** A keychain in memory whose `failAt`th call (0-based, counting every call) throws once. */
function stubKeychain() {
  const kept = new Map<string, string>();
  const calls: string[] = [];
  let failAt = -1;
  let count = 0;
  const step = (what: string) => {
    calls.push(what);
    if (count++ === failAt) throw new Error(`keychain: ${what}`);
  };
  const keychain: Keychain = {
    get: async (key) => {
      step(`get ${key}`);
      return kept.get(key) ?? null;
    },
    set: async (key, value) => {
      step(`set ${key}`);
      kept.set(key, value);
    },
    delete: async (key) => {
      step(`delete ${key}`);
      kept.delete(key);
    },
  };
  return {
    keychain,
    kept,
    calls,
    failOnce(at: number) {
      failAt = at;
      count = 0;
      calls.length = 0;
    },
    healthy() {
      failAt = -1;
      count = 0;
      calls.length = 0;
    },
  };
}

function rig(keychain?: Keychain, newId: (() => string) | null = ids()) {
  const sockets = new Sockets();
  const clock = new FakeClock();
  const store = createOctetStore();
  /** The private key, as hex, the next pairing makes; and the keys each link's sockets were made with. */
  const next = { key: TOKEN };
  const keys: { hubKey: Uint8Array; device: { publicKey: Uint8Array; privateKey: Uint8Array } }[] = [];
  const fleet = new Fleet({
    factory: (k) => {
      keys.push(k);
      return sockets.factory;
    },
    newKey: () => keyOfHex(next.key),
    store,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
    ...(newId ? { newId } : {}),
    ...(keychain ? { keychain } : {}),
  });
  return { sockets, clock, store, fleet, next, keys };
}
type Rig = ReturnType<typeof rig>;

/** The newest socket to `host`'s hub. */
function socketOf(r: Rig, host: string): FakeSocket {
  const all = r.sockets.all.filter((s) => s.url === urlOf(host));
  const socket = all[all.length - 1];
  if (!socket) throw new Error(`no socket to ${host}`);
  return socket;
}

/** What the store holds of Mac `id`'s link: the one place its state is kept. */
const linkOf = (r: Pick<Rig, "store">, id: string) => {
  const s = r.store.getState();
  return { state: s.hubs.get(id) ?? "offline", tried: s.tried.has(id) };
};

const settle = async () => {
  for (let i = 0; i < 30; i++) await Promise.resolve();
};

/** The Mac on `host` answers the pairing just sent: its code, then `paired` under its name. */
function accept(r: Rig, host: string): void {
  const pairing = r.sockets.last;
  pairing.open();
  const [command] = pairing.commandsOf("pair");
  pairing.receive({ type: "pair_code", req: command?.req, code: "482913" });
  pairing.receive({ type: "paired", req: command?.req, device: "iPhone", name: host });
  pairing.drop(1000); // the hub closes the paired connection: the phone says hello on its own
}

/** Pairs with `host` (the phone making the key whose private half is `key`, as hex) and walks its
 * link to ready. */
async function pairWith(r: Rig, host: string, key = TOKEN): Promise<FakeSocket> {
  r.next.key = key;
  const pending = r.fleet.pair(linkTo(host), "iPhone");
  accept(r, host);
  expect(await pending).toEqual({ ok: true, macName: expect.any(String) });
  r.fleet.finishOffer();
  const socket = socketOf(r, host);
  socket.open();
  socket.receive({ type: "welcome", device: "iPhone" });
  await settle();
  return socket;
}

const info = (id: string, extra: object = {}) => ({
  id,
  engine: "claude",
  workspace: `/w/${id}`,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

beforeEach(async () => {
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  for (const id of ["m00000001", "m00000002", "m00000003", "m00000004", "m00000005"]) await SecureStore.deleteItemAsync(tokenKeyOf(id));
  jest.clearAllMocks();
});

describe("the relay", () => {
  /** A saved pairing of the Mac on `host` under `id`, as this version keeps it. */
  const saved = (id: string, host: string) => ({
    id,
    relay: `wss://${relayOf(host)}/`,
    channel: channelOf(host),
    hubKey: hubKeyOf(host),
    deviceName: "iPhone",
    macName: host,
  });

  it("pairs with the link's hub key and a key of its own, kept in hex under the pairing's item", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    const [pairing, link] = r.keys;
    // The pairing's own connection and the link after it are made with the same keys.
    expect(r.keys).toHaveLength(2);
    expect(pairing).toEqual(link);
    expect(Buffer.from(link?.hubKey ?? []).toString("hex")).toBe(hubKeyOf("studio"));
    expect(Buffer.from(link?.device.privateKey ?? []).toString("hex")).toBe("aa".repeat(32));
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    expect((await loadSettings()).pairings).toEqual([saved("m00000001", "studio")]);
    // The next launch reaches the Mac with the key it kept.
    r.fleet.setForeground(false);
    const again = rig();
    again.fleet.setForeground(true);
    await again.fleet.init();
    expect(again.sockets.all.map((s) => s.url)).toEqual([urlOf("studio")]);
    expect(Buffer.from(again.keys[0]?.device.privateKey ?? []).toString("hex")).toBe("aa".repeat(32));
    expect(again.keys[0]?.device.publicKey).toEqual(link?.device.publicKey);
  });

  it("a_4404_close_is_offline_and_keeps_the_pairing", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio", "aa".repeat(32));
    // The relay has no hub for the channel (the Mac is asleep, or stopped dialling).
    studio.drop(4404);
    await settle();
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: true });
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", notice: null, macs: [{ id: "m00000001" }] });
    r.clock.advance(r.clock.delays[0] ?? 0);
    socketOf(r, "studio").drop(4404);
    await settle();
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
  });

  it("a_close_code_from_the_relay_never_forgets_a_pairing", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    for (const code of [4401, 4408, 4409, 4429, 4404]) {
      socketOf(r, "studio").drop(code);
      await settle();
      expect([code, r.fleet.snapshot().status, r.fleet.snapshot().notice]).toEqual([code, "paired", null]);
      r.clock.advance(r.clock.delays[0] ?? 0);
    }
    expect(r.fleet.links.macs()).toEqual(["m00000001"]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
  });

  it("a_refused_unpaired_forgets_it", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    r.fleet.setForeground(false);
    // A revoked phone: the hub stopped knowing its key, and says so inside the channel.
    const again = rig();
    again.fleet.setForeground(true);
    await again.fleet.init();
    const socket = socketOf(again, "studio");
    socket.open();
    const [hello] = socket.commandsOf("hello");
    socket.receive({ type: "refused", req: hello?.req, code: "unpaired", reason: "Unknown device; pair it again" });
    await settle();
    expect(again.fleet.snapshot()).toMatchObject({ status: "unpaired", notice: unpairedNotice("studio"), macs: [] });
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBeNull();
    expect((await loadSettings()).pairings).toEqual([]);
  });

  it("pairing_a_channel_already_held_replaces_it_and_keeps_its_id_and_cursors", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const old = await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    expect(await r.fleet.rename("m00000001", "Studio")).toBe(true);
    // A session streamed from the first pairing: the store holds it, and the client its cursor.
    old.receive({ type: "sessions", sessions: [info("1-2")] });
    r.fleet.links.client("m00000001")?.subscribe("1-2");
    old.receive({ type: "subscribed", session: "1-2", from: "start", journals: ["j"] });
    old.receive({ type: "event", session: "1-2", journal: "j", seq: 4, event: { type: "text", data: "kept" } });
    r.clock.advance(200);
    const key = keyOf("m00000001", "1-2");
    expect(JSON.stringify(r.store.getState().bySession.get(key))).toContain("kept");

    // The same channel again (a phone that paired again): the hub's new answer replaces the pairing.
    const again = await pairWith(r, "studio", "ee".repeat(32));
    expect(old.closed).toBe(true);
    expect(r.fleet.snapshot().macs.map((m) => [m.id, m.macName])).toEqual([
      ["m00000001", "Studio"],
      ["m00000002", "laptop"],
    ]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("ee".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001", "m00000002"]);
    // What the store held of it stays, and the session resumes after its cursor on the new link.
    expect(JSON.stringify(r.store.getState().bySession.get(key))).toContain("kept");
    r.fleet.links.client("m00000001")?.subscribe("1-2");
    expect(again.commandsOf("subscribe")).toEqual([expect.objectContaining({ session: "1-2", after: { journal: "j", seq: 4 } })]);
  });

  it("a_repair_sends_the_name_already_paired_not_the_one_typed", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    // The hub replaces the device of the name it is sent: another name would leave the old key
    // paired there, and the phone would forget it.
    r.next.key = "ee".repeat(32);
    const pending = r.fleet.pair(linkTo("studio"), "Work iPhone");
    const pairing = r.sockets.last;
    pairing.open();
    const [command] = pairing.commandsOf("pair");
    expect(command).toMatchObject({ name: "iPhone" });
    pairing.receive({ type: "pair_code", req: command?.req, code: "482913" });
    pairing.receive({ type: "paired", req: command?.req, device: "iPhone", name: "studio" });
    pairing.drop(1000);
    expect(await pending).toEqual({ ok: true, macName: "studio" });
    expect((await loadSettings()).pairings).toEqual([saved("m00000001", "studio")]);
  });

  it("a_re_pair_survives_its_old_link_being_refused_unpaired_while_it_saves", async () => {
    /** Runs once, as the new key is written: between the hub's `paired` and the settings write. */
    let midSave: (() => void) | null = null;
    const hooked: Keychain = {
      get: (key) => SecureStore.getItemAsync(key),
      set: async (key, value) => {
        const run = midSave;
        midSave = null;
        run?.();
        await SecureStore.setItemAsync(key, value);
      },
      delete: (key) => SecureStore.deleteItemAsync(key),
    };
    const r = rig(hooked);
    r.fleet.setForeground(true);
    await r.fleet.init();
    const old = await pairWith(r, "studio", "aa".repeat(32));
    old.receive({ type: "sessions", sessions: [info("1-2")] });
    r.fleet.links.client("m00000001")?.subscribe("1-2");
    old.receive({ type: "subscribed", session: "1-2", from: "start", journals: ["j"] });
    old.receive({ type: "event", session: "1-2", journal: "j", seq: 4, event: { type: "text", data: "kept" } });
    r.clock.advance(200);
    const key = keyOf("m00000001", "1-2");

    // The hub replaced the old key at `paired` and closed its connection; the old link comes back
    // during the saves and is refused `unpaired`, as the hub refuses any key it no longer knows.
    midSave = () => {
      const [hello] = old.commandsOf("hello");
      old.receive({ type: "refused", req: hello?.req, code: "unpaired", reason: "Unknown device; pair it again" });
      const before = r.sockets.all.length;
      old.drop(1000);
      r.clock.advance(r.clock.delays[0] ?? 0);
      for (const back of r.sockets.all.slice(before)) {
        back.open();
        const [again] = back.commandsOf("hello");
        back.receive({ type: "refused", req: again?.req, code: "unpaired", reason: "Unknown device; pair it again" });
      }
    };
    const notices: string[] = [];
    r.fleet.subscribe(() => {
      const { notice } = r.fleet.snapshot();
      if (notice !== null) notices.push(notice);
    });
    const again = await pairWith(r, "studio", "ee".repeat(32));
    await settle();
    expect(midSave).toBeNull();
    expect(notices).toEqual([]);
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", notice: null, macs: [{ id: "m00000001" }] });
    expect(JSON.stringify(r.store.getState().bySession.get(key))).toContain("kept");
    r.fleet.links.client("m00000001")?.subscribe("1-2");
    expect(again.commandsOf("subscribe")).toEqual([expect.objectContaining({ session: "1-2", after: { journal: "j", seq: 4 } })]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("ee".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
    expect((await loadSettings()).forget).toEqual([]);
    // The next launch is paired with the new key.
    r.fleet.setForeground(false);
    const next = rig();
    next.fleet.setForeground(true);
    await next.fleet.init();
    expect(next.fleet.snapshot().status).toBe("paired");
    expect(Buffer.from(next.keys[0]?.device.privateKey ?? []).toString("hex")).toBe("ee".repeat(32));
  });

  it("a_link_that_names_a_held_mac_with_another_hub_key_is_refused_before_any_network_call", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const old = await pairWith(r, "studio", "aa".repeat(32));
    const sockets = r.sockets.all.length;
    // The studio's channel id, but a key that is not the studio's: no Mac makes such a link.
    const forged = pairLinkTo("studio", SECRET, { relay: RELAY, hubKey: hubKeyOf("intruder") });
    expect(await r.fleet.pair(forged, "iPhone")).toEqual({ ok: false, message: NOT_FROM_THAT_MAC, clear: false });
    expect(r.sockets.all).toHaveLength(sockets);
    // The pairing held is as it was.
    expect(old.closed).toBe(false);
    expect(r.fleet.links.client("m00000001")?.state).toBe("ready");
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", notice: null, macs: [{ id: "m00000001", macName: "studio" }] });
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    expect((await loadSettings()).pairings).toEqual([saved("m00000001", "studio")]);
    // The same Mac's own link still pairs again.
    await pairWith(r, "studio", "ee".repeat(32));
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("ee".repeat(32));
  });

  it("a_pairing_that_fails_leaves_the_one_already_held", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const old = await pairWith(r, "studio", "aa".repeat(32));
    // Refused at the Mac, or cut off on the way: the hub replaced nothing.
    for (const fail of ["refused", "cut"] as const) {
      r.next.key = "ee".repeat(32);
      const pending = r.fleet.pair(linkTo("studio"), "iPhone");
      const pairing = r.sockets.last;
      pairing.open();
      const [command] = pairing.commandsOf("pair");
      pairing.receive({ type: "pair_code", req: command?.req, code: "482913" });
      if (fail === "refused") pairing.receive({ type: "refused", req: command?.req, code: "unpaired", reason: "Pairing was refused on the Mac" });
      if (fail === "cut") pairing.drop(4404);
      const result = await pending;
      expect([fail, result.ok]).toEqual([fail, false]);
      await settle();
      expect([fail, old.closed]).toEqual([fail, false]);
      expect([fail, r.fleet.links.client("m00000001")?.state]).toEqual([fail, "ready"]);
      expect([fail, await SecureStore.getItemAsync(tokenKeyOf("m00000001"))]).toEqual([fail, "aa".repeat(32)]);
      expect([fail, (await loadSettings()).pairings]).toEqual([fail, [saved("m00000001", "studio")]]);
    }
  });

  it("a_fifth_channel_is_refused", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    for (const host of ["a1", "a2", "a3", "a4"]) await pairWith(r, host);
    const sockets = r.sockets.all.length;
    // Four Macs on one relay, and a fifth channel there too: refused before any network call.
    const fifth = pairLinkTo("a5", SECRET, { relay: relayOf("a1") });
    expect(await r.fleet.pair(fifth, "iPhone")).toEqual({ ok: false, message: TOO_MANY_MACS, clear: false });
    expect(r.sockets.all).toHaveLength(sockets);
    // A channel held already pairs again: that replaces, it does not add.
    await pairWith(r, "a2", "ff".repeat(32));
    expect(r.fleet.snapshot().macs).toHaveLength(4);
  });

  it("pairings_and_tokens_from_before_are_dropped_at_first_launch", async () => {
    const k = stubKeychain();
    // What the app from before keys left: the one-pairing token, a pairing by its hub's address
    // with its token under its id, and settings that still carry the old address.
    k.kept.set(TOKEN_KEY, TOKEN);
    k.kept.set(tokenKeyOf("m0000000a"), TOKEN);
    k.kept.set(tokenKeyOf("m00000001"), "aa".repeat(32));
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({
        deviceName: "Shubh's iPhone",
        hubUrl: null,
        pairings: [{ id: "m0000000a", hubUrl: "wss://studio.tail1234.ts.net/", deviceName: "Shubh's iPhone", macName: "studio" }, saved("m00000001", "laptop")],
        forget: [],
      }),
    );
    const r = rig(k.keychain);
    r.fleet.setForeground(true);
    await r.fleet.init();
    // Only this version's pairing is left, connected; the old one and every token are gone.
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", macs: [{ id: "m00000001", macName: "laptop" }] });
    expect(r.sockets.all.map((s) => s.url)).toEqual([urlOf("laptop")]);
    expect([...k.kept.keys()]).toEqual([tokenKeyOf("m00000001")]);
    const raw = JSON.parse((await AsyncStorage.getItem(SETTINGS_KEY)) ?? "{}") as Record<string, unknown>;
    expect(raw).not.toHaveProperty("hubUrl");
    expect(raw.forget).toEqual([]);
    expect((raw.pairings as { id: string }[]).map((p) => p.id)).toEqual(["m00000001"]);
    expect(raw.deviceName).toBe("Shubh's iPhone");
    // Once: the next launch has nothing from before to drop.
    k.healthy();
    r.fleet.setForeground(false);
    await rig(k.keychain).fleet.init();
    expect(k.calls.filter((c) => c.startsWith("delete"))).toEqual([]);
  });

  it("a key in the keychain that is not 32 bytes of hex reaches nothing, and goes", async () => {
    const k = stubKeychain();
    k.kept.set(tokenKeyOf("m00000001"), "not a key");
    await saveSettings({ pairings: [saved("m00000001", "studio")] });
    const r = rig(k.keychain);
    r.fleet.setForeground(true);
    await r.fleet.init();
    expect(r.fleet.snapshot()).toMatchObject({ status: "unpaired", macs: [] });
    expect(r.sockets.all).toHaveLength(0);
    expect(k.kept.has(tokenKeyOf("m00000001"))).toBe(false);
  });

  it("a relay address saved as ws:// is taken only by a development build", async () => {
    const plain = { ...saved("m00000001", "studio"), relay: "ws://127.0.0.1:9000/" };
    const dev = globalThis as { __DEV__?: boolean };
    const before = dev.__DEV__;
    try {
      for (const development of [true, false]) {
        dev.__DEV__ = development;
        await AsyncStorage.clear();
        await SecureStore.setItemAsync(tokenKeyOf("m00000001"), "aa".repeat(32));
        await saveSettings({ pairings: [plain] });
        const r = rig();
        r.fleet.setForeground(true);
        await r.fleet.init();
        expect([development, r.sockets.all.map((s) => s.url)]).toEqual([
          development,
          development ? [`ws://127.0.0.1:9000/v1/phone/${channelOf("studio")}`] : [],
        ]);
        r.fleet.setForeground(false);
      }
    } finally {
      dev.__DEV__ = before;
    }
  });
});

describe("pairing more than one Mac", () => {
  it("adds a second Mac: both are paired, both connected, each with its own keychain entry", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    expect(r.fleet.snapshot().macs.map((m) => [m.id, m.macName, r.store.getState().hubs.get(m.id)])).toEqual([
      ["m00000001", "studio", "ready"],
      ["m00000002", "laptop", "ready"],
    ]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000002"))).toBe("bb".repeat(32));
    expect((await loadSettings()).pairings.map((p) => [p.id, p.channel, p.macName])).toEqual([
      ["m00000001", channelOf("studio"), "studio"],
      ["m00000002", channelOf("laptop"), "laptop"],
    ]);
    expect(r.fleet.links.macs()).toEqual(["m00000001", "m00000002"]);
    expect(r.fleet.links.client("m00000002")?.channel).toBe(channelOf("laptop"));
  });

  it("refuses a 5th Mac before any network call", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    for (const host of ["a1", "a2", "a3", "a4"]) await pairWith(r, host);
    const sockets = r.sockets.all.length;
    expect(await r.fleet.pair(linkTo("a5"), "iPhone")).toEqual({ ok: false, message: TOO_MANY_MACS, clear: false });
    expect(TOO_MANY_MACS).toBe("Unpair a Mac first (4 at most)");
    expect(r.sockets.all).toHaveLength(sockets);
    // A paired host still pairs again: that replaces, it does not add.
    await pairWith(r, "a2", "ff".repeat(32));
    expect(r.fleet.snapshot().macs).toHaveLength(4);
  });

  it("loads every saved pairing at the next launch", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    r.fleet.setForeground(false);
    const again = rig();
    again.fleet.setForeground(true);
    await again.fleet.init();
    expect(again.fleet.snapshot().macs.map((m) => m.id)).toEqual(["m00000001", "m00000002"]);
    expect(again.sockets.all.map((s) => s.url)).toEqual([urlOf("studio"), urlOf("laptop")]);
  });

  it("each Mac's sessions are keyed by its own id", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio");
    const laptop = await pairWith(r, "laptop");
    studio.receive({ type: "sessions", sessions: [info("1-2")] });
    laptop.receive({ type: "sessions", sessions: [info("1-2")] });
    r.clock.advance(200);
    expect(r.store.getState().sessions.map((s) => s.id)).toEqual([keyOf("m00000001", "1-2"), keyOf("m00000002", "1-2")]);
  });

  it("tried turns true once a link's first connection attempt ends", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    accept(r, "studio");
    await pending;
    expect(linkOf(r, "m00000001")).toEqual({ state: "connecting", tried: false });
    socketOf(r, "studio").drop();
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: true });
  });
});

describe("cancelling a pairing", () => {
  const CANCELLED = { ok: false, message: "", clear: false, cancelled: true };

  /** Starts pairing with `host`; the Mac holds the phone and sends its code. The pairing's socket,
   * its `pair` command and the pairing's answer to come. */
  function heldBy(r: Rig, host: string) {
    const pending = r.fleet.pair(linkTo(host), "iPhone");
    const pairing = r.sockets.last;
    pairing.open();
    const [command] = pairing.commandsOf("pair");
    pairing.receive({ type: "pair_code", req: command?.req, code: "482913" });
    return { pending, pairing, req: command?.req };
  }

  it("cancelPair_racing_a_paired_answer_keeps_nothing", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const { pending, pairing, req } = heldBy(r, "studio");
    // The Mac's yes and the cancel land together: the cancel wins.
    pairing.receive({ type: "paired", req, device: "iPhone", name: "studio" });
    r.fleet.cancelPair();
    expect(await pending).toEqual(CANCELLED);
    await settle();
    expect(pairing.closed).toBe(true);
    expect(r.fleet.snapshot().macs).toEqual([]);
    expect((await loadSettings()).pairings).toEqual([]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBeNull();
    // The next pairing is not refused as one already in flight.
    await pairWith(r, "studio");
    expect(r.fleet.snapshot().macs.map((m) => m.macName)).toEqual(["studio"]);
  });

  it("cancelPair_while_the_pairing_saves_keeps_it_and_offers_no_notifications", async () => {
    // A keychain that holds the key's save until the test lets it go.
    const kept = new Map<string, string>();
    let saving!: () => void;
    const started = new Promise<void>((resolve) => (saving = resolve));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    const keychain: Keychain = {
      get: async (key) => kept.get(key) ?? null,
      set: async (key, value) => {
        saving();
        await gate;
        kept.set(key, value);
      },
      delete: async (key) => {
        kept.delete(key);
      },
    };
    const r = rig(keychain);
    r.fleet.setForeground(true);
    await r.fleet.init();
    const { pending, pairing, req } = heldBy(r, "studio");
    pairing.receive({ type: "paired", req, device: "iPhone", name: "studio" });
    pairing.drop(1000);
    // The phone heard `paired` and is saving the key: the cancel comes too late to stop it.
    await started;
    r.fleet.cancelPair();
    release();
    expect(await pending).toEqual({ ok: true, macName: "studio" });
    // The Mac holds the device, so the phone keeps it: it works, and it is in the list.
    expect(kept.get(tokenKeyOf("m00000001"))).toBe(TOKEN);
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
    expect(r.fleet.snapshot().macs.map((m) => m.macName)).toEqual(["studio"]);
    // But nothing shows for a pairing the user cancelled: no notifications step.
    expect(r.fleet.snapshot().offer).toBe(false);
  });

  it("cancelPair_of_a_re_pair_keeps_the_old_pairing", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const old = await pairWith(r, "studio", "aa".repeat(32));
    const before = await loadSettings();
    r.next.key = "ee".repeat(32);
    const { pending, pairing, req } = heldBy(r, "studio");
    r.fleet.cancelPair();
    expect(await pending).toEqual(CANCELLED);
    expect(pairing.closed).toBe(true);
    // A yes after the cancel is not heard.
    pairing.receive({ type: "paired", req, device: "iPhone", name: "studio" });
    await settle();
    expect(await loadSettings()).toEqual(before);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    expect(r.fleet.snapshot().macs.map((m) => m.id)).toEqual(["m00000001"]);
    expect(old.closed).toBe(false);
    expect(linkOf(r, "m00000001").state).toBe("ready");
  });
});

describe("keyFor", () => {
  it("resolves the channel id a hub sends and a raw id to the phone's key, through the pairings", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio");
    expect(r.fleet.keyFor(channelOf("studio"), "1-2")).toBe(keyOf("m00000001", "1-2"));
    // No channel names no Mac, even the only one.
    expect(r.fleet.keyFor(null, "1-2")).toBeNull();
    await pairWith(r, "laptop");
    expect(r.fleet.keyFor(channelOf("laptop"), "1-2")).toBe(keyOf("m00000002", "1-2"));
    expect(r.fleet.keyFor(channelOf("air"), "1-2")).toBeNull();
    expect(r.fleet.keyFor(RELAY, "1-2")).toBeNull();
    expect(r.fleet.keyFor(channelOf("studio"), "m00000002/1-2")).toBeNull();
    // A tap resolves the same way, and opens the Inbox for a session its Mac has ended.
    expect(r.fleet.tapKey({ channel: channelOf("laptop"), session: "1-2" })).toBe(keyOf("m00000002", "1-2"));
    socketOf(r, "laptop").receive({ type: "sessions", sessions: [] });
    r.clock.advance(50);
    expect(r.fleet.tapKey({ channel: channelOf("laptop"), session: "1-2" })).toBeNull();
  });

  it("two_macs_on_one_relay_stay_two", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    // One relay, two channels: two pairings, each found by its own channel id.
    expect((await loadSettings()).pairings.map((p) => [p.id, p.relay, p.channel])).toEqual([
      ["m00000001", `wss://${RELAY}/`, channelOf("studio")],
      ["m00000002", `wss://${RELAY}/`, channelOf("laptop")],
    ]);
    expect(r.fleet.snapshot().macs).toEqual([
      { id: "m00000001", macName: "studio", channel: channelOf("studio"), relay: RELAY },
      { id: "m00000002", macName: "laptop", channel: channelOf("laptop"), relay: RELAY },
    ]);
    expect(r.fleet.keyFor(channelOf("studio"), "1-2")).toBe(keyOf("m00000001", "1-2"));
    expect(r.fleet.keyFor(channelOf("laptop"), "1-2")).toBe(keyOf("m00000002", "1-2"));
  });

  describe("two_macs_with_the_same_session_id_stay_apart", () => {
    /** Two Macs through the one relay, each listing session 1-2 with approval `studio` and
     * `laptop` waiting at its front. */
    async function both({ studio, laptop }: { studio: number; laptop: number }) {
      const r = rig();
      r.fleet.setForeground(true);
      await r.fleet.init();
      const a = await pairWith(r, "studio", "aa".repeat(32));
      const b = await pairWith(r, "laptop", "bb".repeat(32));
      const waiting = (id: number) => info("1-2", { approvals: 1, approval: { id, summary: { action: "Run", command: "make", files: [], scope: "once" } } });
      a.receive({ type: "sessions", sessions: [waiting(studio)] });
      b.receive({ type: "sessions", sessions: [waiting(laptop)] });
      r.clock.advance(50);
      return r;
    }

    it("a push tap", async () => {
      const r = await both({ studio: 3, laptop: 4 });
      expect(r.fleet.tapKey({ channel: channelOf("studio"), session: "1-2" })).toBe(keyOf("m00000001", "1-2"));
      expect(r.fleet.tapKey({ channel: channelOf("laptop"), session: "1-2" })).toBe(keyOf("m00000002", "1-2"));
    });

    it("Allow", async () => {
      const r = await both({ studio: 3, laptop: 4 });
      expect(r.fleet.approveNow({ channel: channelOf("laptop"), raw: "1-2", approval: 4 })).toBe(keyOf("m00000002", "1-2"));
      expect(r.fleet.approveNow({ channel: channelOf("studio"), raw: "1-2", approval: 3 })).toBe(keyOf("m00000001", "1-2"));
      // Each approval is its own Mac's: the other's channel with it opens the Inbox.
      expect(r.fleet.approveNow({ channel: channelOf("studio"), raw: "1-2", approval: 4 })).toBeNull();
      expect(r.fleet.approveNow({ channel: channelOf("laptop"), raw: "1-2", approval: 3 })).toBeNull();
    });
  });
});

describe("a Mac's name", () => {
  /** Pairs with the Mac on `host`, whose hub answers `paired` with `extra` (its name, or none). */
  async function pairNamed(extra: object) {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    const pairing = r.sockets.last;
    pairing.open();
    const [command] = pairing.commandsOf("pair");
    pairing.receive({ type: "paired", req: command?.req, device: "iPhone", ...extra });
    pairing.drop(1000);
    return { r, result: await pending };
  }

  it("the_macs_name_is_the_hubs_cleaned", async () => {
    const { r, result } = await pairNamed({ name: "  Studio\u0007 \n  Mac\u202e " });
    expect(result).toEqual({ ok: true, macName: "Studio Mac" });
    expect(r.fleet.snapshot().macs.map((m) => m.macName)).toEqual(["Studio Mac"]);
    expect((await loadSettings()).pairings.map((p) => p.macName)).toEqual(["Studio Mac"]);
  });

  it.each([
    ["no name", {}],
    ["an empty name", { name: "" }],
    ["a name of spaces and controls", { name: " \u0007\u202e " }],
  ])("a_missing_name_falls_back_to_Mac: %s", async (_what, extra) => {
    const { r, result } = await pairNamed(extra);
    expect(result).toEqual({ ok: true, macName: "Mac" });
    expect(r.fleet.snapshot().macs.map((m) => m.macName)).toEqual(["Mac"]);
    // Never the relay's host, which every Mac on it shares.
    expect((await loadSettings()).pairings.map((p) => p.macName)).toEqual(["Mac"]);
  });
});

describe("links", () => {
  it("are one object for the Fleet's life, resolving each Mac as it is at the call", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const links = r.fleet.links;
    await pairWith(r, "studio");
    const studio = links.client("m00000001");
    await pairWith(r, "laptop");
    await r.fleet.unpair("m00000002");
    expect(r.fleet.links).toBe(links);
    expect(links.client("m00000001")).toBe(studio);
    expect(links.client("m00000002")).toBeNull();
    // Paired again: the same links name the new client.
    await pairWith(r, "studio");
    expect(links.client("m00000001")).not.toBe(studio);
    expect(links.macs()).toEqual(["m00000001"]);
  });
});

describe("foreground and background", () => {
  it("open and close every link", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const a = await pairWith(r, "studio");
    const b = await pairWith(r, "laptop");
    r.fleet.setForeground(false);
    expect([a.closed, b.closed]).toEqual([true, true]);
    expect(r.clock.pending).toBe(0);
    const before = r.sockets.all.length;
    r.fleet.setForeground(true);
    expect(r.sockets.all.slice(before).map((s) => s.url)).toEqual([urlOf("studio"), urlOf("laptop")]);
  });
});

describe("unpair_one_mac", () => {
  async function two(notifications: boolean) {
    await saveSettings({ notifications });
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio", "aa".repeat(32));
    const laptop = await pairWith(r, "laptop", "bb".repeat(32));
    // Each answers the registration made at ready.
    for (const s of [studio, laptop]) {
      for (const c of s.commandsOf("register_push")) s.receive({ type: "ack", req: c.req, message: "ok" });
    }
    await settle();
    studio.receive({ type: "sessions", sessions: [info("1-2")] });
    laptop.receive({ type: "sessions", sessions: [info("3-4")] });
    r.clock.advance(200);
    r.store.getState().setDraft(keyOf("m00000001", "1-2"), "draft a");
    r.store.getState().setDraft(keyOf("m00000002", "3-4"), "draft b");
    r.store.getState().remember(keyOf("m00000001", "1-2"), "sent a");
    r.store.getState().remember(keyOf("m00000002", "3-4"), "sent b");
    return { r, studio, laptop };
  }

  it("drops that Mac's token, keys, views, drafts and toasts; the other Mac's link, keys and views stay", async () => {
    const { r, studio, laptop } = await two(false);
    const laptopClient = r.fleet.links.client("m00000002");
    showToast("1-2 needs approval", () => {}, "View", "m00000001");
    await r.fleet.unpair("m00000001");
    const s = r.store.getState();
    expect(s.sessions.map((x) => x.id)).toEqual([keyOf("m00000002", "3-4")]);
    expect([...s.drafts.keys()]).toEqual([keyOf("m00000002", "3-4")]);
    expect([...s.sent.keys()]).toEqual([keyOf("m00000002", "3-4")]);
    expect(toastStore.getState().toast).toBeNull();
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", macs: [{ id: "m00000002" }] });
    expect(r.fleet.links.client("m00000002")).toBe(laptopClient);
    expect(r.fleet.links.client("m00000001")).toBeNull();
    expect(laptop.closed).toBe(false);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBeNull();
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000002"))).toBe("bb".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000002"]);
    r.clock.advance(2_000);
    expect(studio.closed).toBe(true);
    // The other Mac's sessions still flow in.
    laptop.receive({ type: "sessions", sessions: [info("3-4"), info("5-6")] });
    r.clock.advance(200);
    expect(r.store.getState().sessions.map((x) => x.id)).toEqual([keyOf("m00000002", "3-4"), keyOf("m00000002", "5-6")]);
  });

  it("with notifications on, tells that Mac first, keeps notifications on for the others, and keeps the phone's push registration", async () => {
    const { r, studio, laptop } = await two(true);
    const before = laptop.commandsOf("register_push").length;
    await r.fleet.unpair("m00000001");
    expect(studio.commandsOf("register_push").at(-1)).toMatchObject({ token: null });
    expect(laptop.commandsOf("register_push")).toHaveLength(before);
    expect(studio.closed).toBe(false); // the goodbye is still on its way
    r.clock.advance(2_000);
    await settle();
    expect(studio.closed).toBe(true);
    expect((await loadSettings()).notifications).toBe(true);
    // Another Mac still pushes to this phone: its registration must stay.
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled();
  });

  it("unpairing the last Mac shows Pair", async () => {
    const { r } = await two(false);
    await r.fleet.unpair("m00000001");
    await r.fleet.unpair("m00000002");
    expect(r.fleet.snapshot()).toMatchObject({ status: "unpaired", macs: [] });
    expect((await loadSettings()).pairings).toEqual([]);
  });

  it("an unknown Mac is nothing", async () => {
    const { r } = await two(false);
    await r.fleet.unpair("m0000000f");
    expect(r.fleet.snapshot().macs).toHaveLength(2);
  });
});

describe("rename", () => {
  it("is cleaned and persisted", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio");
    expect(await r.fleet.rename("m00000001", "  Home\u0007  studio\n ")).toBe(true);
    expect(r.fleet.snapshot().macs[0]?.macName).toBe("Home studio");
    expect((await loadSettings()).pairings[0]?.macName).toBe("Home studio");
    expect(await r.fleet.rename("m00000001", "   ")).toBe(false);
    expect(await r.fleet.rename("m00000001", "x".repeat(60))).toBe(true);
    expect(r.fleet.snapshot().macs[0]?.macName).toBe("x".repeat(40));
    expect(await r.fleet.rename("m0000000f", "Other")).toBe(false);
    const again = rig();
    await again.fleet.init();
    expect(again.fleet.snapshot().macs[0]?.macName).toBe("x".repeat(40));
  });
});

describe("push across Macs", () => {
  const answerLast = (s: FakeSocket, message = "ok") => {
    const last = s.commandsOf("register_push").at(-1);
    s.receive({ type: "ack", req: last?.req, message });
  };
  /** Two Macs paired, each one's registration at ready (with notifications on) answered. */
  async function two() {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const on = (await loadSettings()).notifications;
    const studio = await pairWith(r, "studio");
    if (on) answerLast(studio);
    await settle();
    const laptop = await pairWith(r, "laptop");
    if (on) answerLast(laptop);
    await settle();
    return { r, studio, laptop };
  }

  it("on reaches every Mac, in turn, and is saved after their acks", async () => {
    const { r, studio, laptop } = await two();
    const done = r.fleet.turnOnNotifications();
    await settle();
    expect(studio.commandsOf("register_push")).toEqual([expect.objectContaining({ token: PUSH })]);
    expect(laptop.commandsOf("register_push")).toHaveLength(0); // its turn comes after the first's answer
    answerLast(studio);
    await settle();
    expect(laptop.commandsOf("register_push")).toEqual([expect.objectContaining({ token: PUSH })]);
    answerLast(laptop);
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(true);
    expect(r.fleet.snapshot().notices).toEqual([]);
  });

  it("one Mac failing gives a notice naming it; the switch follows the Macs that answered", async () => {
    const { r, studio, laptop } = await two();
    const done = r.fleet.turnOnNotifications();
    await settle();
    answerLast(studio);
    await settle();
    const last = laptop.commandsOf("register_push").at(-1);
    laptop.receive({ type: "refused", req: last?.req, code: "invalid", reason: "No push here" });
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(true);
    expect(r.fleet.snapshot().notices).toEqual(["laptop: No push here"]);
  });

  it("every Mac failing fails, naming each, and saves nothing", async () => {
    const { r, studio, laptop } = await two();
    const done = r.fleet.turnOnNotifications();
    await settle();
    studio.receive({ type: "refused", req: studio.commandsOf("register_push").at(-1)?.req, code: "invalid", reason: "No" });
    await settle();
    laptop.receive({ type: "refused", req: laptop.commandsOf("register_push").at(-1)?.req, code: "invalid", reason: "Nope" });
    await expect(done).resolves.toEqual({ ok: false, message: "studio: No\nlaptop: Nope" });
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("off reaches every Mac; one out of reach is named, and told when it is ready again", async () => {
    await saveSettings({ notifications: true });
    const { r, studio, laptop } = await two();
    laptop.drop();
    const done = r.fleet.turnOffNotifications();
    await settle();
    answerLast(studio, "Notifications off");
    await settle();
    r.clock.advance(8_000); // the laptop does not come back within the wait
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(false);
    expect(r.fleet.snapshot().notices).toEqual(["laptop: Can't reach your Mac"]);
    // Back (its reconnect is under way), it is told "off".
    const back = socketOf(r, "laptop");
    expect(back).not.toBe(laptop);
    back.open();
    back.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(back.commandsOf("register_push")).toEqual([expect.objectContaining({ token: null })]);
    answerLast(back, "Notifications off");
    await settle();
    expect(r.fleet.snapshot().notices).toEqual([]);
  });

  it("a kind reaches every Mac", async () => {
    await saveSettings({ notifications: true });
    const { r, studio, laptop } = await two();
    const done = r.fleet.setPushKind("turns", true);
    await settle();
    answerLast(studio);
    await settle();
    answerLast(laptop);
    await expect(done).resolves.toEqual({ ok: true });
    for (const s of [studio, laptop]) expect(s.commandsOf("register_push").at(-1)).toMatchObject({ kinds: { approvals: true, errors: true, turns: true } });
    expect((await loadSettings()).pushKinds.turns).toBe(true);
  });
});

describe("the Mac unpairs this phone", () => {
  it("removes only that pairing, and the notice names the Mac", async () => {
    await saveSettings({ notifications: true });
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio", "aa".repeat(32));
    const laptop = await pairWith(r, "laptop", "bb".repeat(32));
    studio.receive({ type: "sessions", sessions: [info("1-2")] });
    laptop.receive({ type: "sessions", sessions: [info("3-4")] });
    r.clock.advance(200);
    laptop.receive({ type: "refused", req: 0, reason: "This device was revoked", code: "revoked" });
    await settle();
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", macs: [{ id: "m00000001" }], notice: unpairedNotice("laptop") });
    expect(unpairedNotice("laptop")).toBe("This phone was unpaired on laptop");
    expect(r.store.getState().sessions.map((x) => x.id)).toEqual([keyOf("m00000001", "1-2")]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000002"))).toBeNull();
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("aa".repeat(32));
    const s = await loadSettings();
    expect(s.pairings.map((p) => p.id)).toEqual(["m00000001"]);
    expect(s.notifications).toBe(true); // the studio still pushes
    expect(studio.closed).toBe(false);
  });

  it("the last one shows Pair, naming it", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio");
    studio.receive({ type: "refused", req: 0, reason: "revoked", code: "unpaired" });
    await settle();
    expect(r.fleet.snapshot()).toMatchObject({ status: "unpaired", notice: unpairedNotice("studio"), macs: [] });
  });
});

describe("fix round 1", () => {
  it("with no random source for ids at all, two pairings both succeed", async () => {
    const g = globalThis as { crypto?: unknown; expo?: { uuidv4?: () => string } };
    const crypto = g.crypto;
    const expo = g.expo;
    // What the pairings need of the random source anyway (the keys, the links) is made first.
    for (const hex of ["aa".repeat(32), "bb".repeat(32)]) keyOfHex(hex);
    for (const host of ["studio", "laptop"]) linkTo(host);
    try {
      Object.defineProperty(globalThis, "crypto", { value: {}, configurable: true });
      g.expo = {};
      const k = stubKeychain();
      const r = rig(k.keychain, null);
      r.fleet.setForeground(true);
      await r.fleet.init();
      await pairWith(r, "studio", "aa".repeat(32));
      await pairWith(r, "laptop", "bb".repeat(32));
      const ids = r.fleet.snapshot().macs.map((m) => m.id);
      expect(ids).toHaveLength(2);
      for (const id of ids) expect(id).toMatch(MAC_ID);
      expect(new Set(ids).size).toBe(2);
      expect(k.kept.get(tokenKeyOf(ids[1]!))).toBe("bb".repeat(32));
    } finally {
      Object.defineProperty(globalThis, "crypto", { value: crypto, configurable: true });
      g.expo = expo;
    }
  });

  it("makes the new pairing's id before the Mac spends its code", async () => {
    let made = 0;
    const r = rig(undefined, () => `m0000000${++made}`);
    await r.fleet.init();
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    expect(made).toBe(1); // before any answer from the Mac
    accept(r, "studio");
    expect(await pending).toEqual({ ok: true, macName: expect.any(String) });
    expect(made).toBe(1);
  });

  it("a background stop does not count as the first attempt ending", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    accept(r, "studio");
    await pending;
    expect(linkOf(r, "m00000001")).toEqual({ state: "connecting", tried: false });
    r.fleet.setForeground(false);
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: false });
    r.fleet.setForeground(true);
    expect(linkOf(r, "m00000001")).toEqual({ state: "connecting", tried: false });
    socketOf(r, "studio").drop();
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: true });
  });

  it("a Mac back from the background is connecting, not asleep, until its first attempt ends", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio");
    expect(linkOf(r, "m00000001")).toEqual({ state: "ready", tried: true });
    r.fleet.setForeground(false);
    r.fleet.setForeground(true);
    expect(linkOf(r, "m00000001")).toEqual({ state: "connecting", tried: false });
    expect(macStatus("connecting", false)).toBe("connecting…");
    socketOf(r, "studio").drop();
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: true });
    // A drop from ready while in front is a Mac asleep, at once.
    r.clock.advance(60_000);
    const again = socketOf(r, "studio");
    again.open();
    again.receive({ type: "welcome", device: "iPhone" });
    expect(linkOf(r, "m00000001")).toEqual({ state: "ready", tried: true });
    again.drop();
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: true });
  });

  it("an unpair while that Mac pairs again never deletes the key the pairing just saved", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    r.next.key = "ee".repeat(32);
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    const unpaired = r.fleet.unpair("m00000001");
    accept(r, "studio");
    expect(await pending).toEqual({ ok: true, macName: expect.any(String) });
    await unpaired;
    await settle();
    expect(r.fleet.snapshot().macs.map((m) => m.id)).toEqual(["m00000001"]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("ee".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
    // The next launch is paired with the new key.
    const again = rig();
    again.fleet.setForeground(true);
    await again.fleet.init();
    expect(again.fleet.snapshot().status).toBe("paired");
    expect(Buffer.from(again.keys[0]?.device.privateKey ?? []).toString("hex")).toBe("ee".repeat(32));
  });

  it("unpairing while turning on is waiting for that Mac ends the wait at once", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio");
    studio.drop();
    let result: unknown;
    void r.fleet.turnOnNotifications().then((x) => (result = x));
    await settle();
    expect(result).toBeUndefined(); // waiting for the link to come back
    await r.fleet.unpair("m00000001");
    await settle();
    expect(result).toEqual({ ok: false, message: "Can't reach your Mac" });
    expect(r.clock.pending).toBe(0);
  });

  it("a pairing whose key cannot be read is listed, offline and not tried, and can be unpaired", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    r.fleet.setForeground(false);
    const locked: Keychain = {
      get: async (key) => {
        if (key === tokenKeyOf("m00000002")) throw new Error("locked");
        return SecureStore.getItemAsync(key);
      },
      set: (key, value) => SecureStore.setItemAsync(key, value),
      delete: (key) => SecureStore.deleteItemAsync(key),
    };
    const again = rig(locked);
    await again.fleet.init();
    expect(again.fleet.snapshot().macs).toEqual([
      expect.objectContaining({ id: "m00000001" }),
      { id: "m00000002", macName: "laptop", channel: channelOf("laptop"), relay: RELAY },
    ]);
    expect(again.fleet.links.macs()).toEqual(["m00000001"]);
    // The store holds no link of it: offline, and not tried.
    expect(linkOf(again, "m00000002")).toEqual({ state: "offline", tried: false });
    await again.fleet.unpair("m00000002");
    expect(again.fleet.snapshot().macs.map((m) => m.id)).toEqual(["m00000001"]);
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000002"))).toBeNull();
  });

  it("keeps the Macs list the same object when only a notice changed", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const studio = await pairWith(r, "studio");
    const laptop = await pairWith(r, "laptop");
    const macs = r.fleet.snapshot().macs;
    const done = r.fleet.turnOnNotifications();
    await settle();
    studio.receive({ type: "ack", req: studio.commandsOf("register_push").at(-1)?.req, message: "ok" });
    await settle();
    laptop.receive({ type: "refused", req: laptop.commandsOf("register_push").at(-1)?.req, code: "invalid", reason: "No" });
    await done;
    expect(r.fleet.snapshot().notices).toEqual(["laptop: No"]);
    expect(r.fleet.snapshot().macs).toBe(macs);
  });
});

describe("unpairing the last pairing, one with no link", () => {
  /** studio and laptop paired, then a launch whose keychain will not give the tokens of `locked`. */
  async function relaunch(...locked: string[]) {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    r.fleet.setForeground(false);
    const keychain: Keychain = {
      get: async (key) => {
        if (locked.some((id) => key === tokenKeyOf(id))) throw new Error("locked");
        return SecureStore.getItemAsync(key);
      },
      set: (key, value) => SecureStore.setItemAsync(key, value),
      delete: (key) => SecureStore.deleteItemAsync(key),
    };
    const again = rig(keychain);
    again.fleet.setForeground(true);
    await again.fleet.init();
    n.unregisterForNotificationsAsync.mockClear();
    return again;
  }

  it("drops the phone's push registration: no Mac is left to say notifications off to", async () => {
    const r = await relaunch("m00000001", "m00000002");
    expect(r.fleet.snapshot().status).toBe("unpaired");
    await r.fleet.unpair("m00000001");
    await settle();
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled(); // laptop's pairing is still kept
    await r.fleet.unpair("m00000002");
    await settle();
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
  });

  it("drops it once when all go at once and none has a link", async () => {
    const r = await relaunch("m00000001", "m00000002");
    await r.fleet.unpair();
    await settle();
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
  });

  it("leaves it to the linked Mac's goodbye when one goes with it: once if unheard, none if heard", async () => {
    const r = await relaunch("m00000002");
    const studio = socketOf(r, "studio");
    studio.open();
    studio.receive({ type: "welcome", device: "iPhone" });
    await settle();
    await r.fleet.unpair();
    await settle();
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled(); // the goodbye is out
    const [bye] = studio.commandsOf("register_push").filter((c) => c.token === null);
    studio.receive({ type: "ack", req: bye?.req, message: "Notifications off" });
    await settle();
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled();

    const other = await relaunch("m00000002");
    await other.fleet.unpair(); // studio is not ready: its goodbye drops the registration
    await settle();
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
  });
});
