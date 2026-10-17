// The Fleet: up to four pairings, each its own link (client, keychain entry, push sync), all open
// in the foreground. The single-Mac behaviour each link keeps is in src/hub.test.ts.
import AsyncStorage from "@react-native-async-storage/async-storage";
import * as Notifications from "expo-notifications";
import * as SecureStore from "expo-secure-store";

import { FakeClock, Sockets, type FakeSocket } from "./client/fakes";
import { Fleet, TOKEN_KEY, tokenKeyOf, unpairedNotice, type Keychain } from "./hub";
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

const linkTo = (host: string) => `https://${host}.tail1234.ts.net/#pair=${SECRET}`;
const urlOf = (host: string) => `wss://${host}.tail1234.ts.net/`;

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
  const fleet = new Fleet({
    factory: sockets.factory,
    store,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
    ...(newId ? { newId } : {}),
    ...(keychain ? { keychain } : {}),
  });
  return { sockets, clock, store, fleet };
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

/** Pairs with `host` (the Mac accepts with `token`) and walks its link to ready. */
async function pairWith(r: Rig, host: string, token = TOKEN): Promise<FakeSocket> {
  const pending = r.fleet.pair(linkTo(host), "iPhone");
  const pairing = r.sockets.last;
  pairing.open();
  const [command] = pairing.commandsOf("pair");
  pairing.receive({ type: "paired", req: command?.req, token, device: "iPhone" });
  expect(await pending).toEqual({ ok: true });
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

describe("migration_is_idempotent", () => {
  const OLD = TOKEN_KEY;
  const NEW = tokenKeyOf("m00000001");

  async function legacy(k: ReturnType<typeof stubKeychain>) {
    k.kept.set(OLD, TOKEN);
    await saveSettings({ hubUrl: urlOf("studio"), deviceName: "Shubh's iPhone" });
  }

  it("moves today's pairing to pairing 1: copied, read back, then the old entry deleted", async () => {
    const k = stubKeychain();
    await legacy(k);
    const r = rig(k.keychain);
    r.fleet.setForeground(true);
    await r.fleet.init();
    // The old token is read, copied, read back, and only then deleted.
    const at = (call: string, from = 0) => k.calls.indexOf(call, from);
    const copied = at(`set ${NEW}`);
    const readBack = at(`get ${NEW}`, copied);
    expect(at(`get ${OLD}`)).toBeGreaterThanOrEqual(0);
    expect(at(`get ${OLD}`)).toBeLessThan(copied);
    expect(copied).toBeLessThan(readBack);
    expect(readBack).toBeLessThan(at(`delete ${OLD}`));
    expect(k.kept.get(NEW)).toBe(TOKEN);
    expect(k.kept.has(OLD)).toBe(false);
    const s = await loadSettings();
    expect(s.hubUrl).toBeNull();
    expect(s.pairings).toEqual([{ id: "m00000001", hubUrl: urlOf("studio"), deviceName: "Shubh's iPhone", macName: "studio" }]);
    expect(r.fleet.snapshot()).toMatchObject({ status: "paired", macs: [{ id: "m00000001", macName: "studio", host: "studio.tail1234.ts.net" }] });
    socketOf(r, "studio").open();
    expect(socketOf(r, "studio").commandsOf("hello")).toEqual([expect.objectContaining({ token: TOKEN })]);
  });

  it("writes the pairings before the old entry is deleted", async () => {
    const k = stubKeychain();
    await legacy(k);
    let pairingsWhenDeleted: unknown = "never";
    const del = k.keychain.delete;
    k.keychain.delete = async (key) => {
      if (key === OLD) pairingsWhenDeleted = JSON.parse((await AsyncStorage.getItem(SETTINGS_KEY)) ?? "{}").pairings;
      return del(key);
    };
    const r = rig(k.keychain);
    await r.fleet.init();
    expect(pairingsWhenDeleted).toEqual([expect.objectContaining({ id: "m00000001" })]);
  });

  it("a failure at each step in turn: the next launch finishes the move, with exactly one pairing holding the token", async () => {
    // Count the keychain calls of a clean migration, then fail each one in turn.
    const probe = stubKeychain();
    await legacy(probe);
    await rig(probe.keychain).fleet.init();
    const steps = probe.calls.length;
    expect(steps).toBeGreaterThanOrEqual(4);
    for (let at = 0; at < steps; at++) {
      await AsyncStorage.clear();
      const k = stubKeychain();
      await legacy(k);
      k.failOnce(at);
      const first = rig(k.keychain);
      await first.fleet.init();
      // Whatever broke, the old token is never gone unless pairing 1's entry holds it.
      if (!k.kept.has(OLD)) expect([at, k.kept.get(NEW)]).toEqual([at, TOKEN]);
      first.fleet.setForeground(false);
      k.healthy();
      const next = rig(k.keychain);
      await next.fleet.init();
      const s = await loadSettings();
      expect([at, s.pairings.length, s.hubUrl]).toEqual([at, 1, null]);
      const id = s.pairings[0]!.id;
      expect([at, k.kept.get(tokenKeyOf(id)), k.kept.has(OLD)]).toEqual([at, TOKEN, false]);
      expect([at, next.fleet.snapshot().status, next.fleet.snapshot().macs.length]).toEqual([at, "paired", 1]);
      // No other entry holds a copy of the token.
      expect([at, [...k.kept.keys()]]).toEqual([at, [tokenKeyOf(id)]]);
    }
  });

  it("a settings write that fails leaves the move for the next launch", async () => {
    for (let fail = 0; fail < 2; fail++) {
      await AsyncStorage.clear();
      const k = stubKeychain();
      await legacy(k);
      // The `fail`th settings write of this launch fails (0: the pairings, 1: the old address).
      const setItem = jest.mocked(AsyncStorage.setItem);
      const real = setItem.getMockImplementation();
      if (!real) throw new Error("async-storage's mock has no setItem");
      if (fail === 1) setItem.mockImplementationOnce(real);
      setItem.mockRejectedValueOnce(new Error("disk full"));
      await rig(k.keychain).fleet.init();
      await rig(k.keychain).fleet.init();
      const s = await loadSettings();
      expect([fail, s.pairings.length, s.hubUrl, k.kept.has(OLD)]).toEqual([fail, 1, null, false]);
      expect([fail, k.kept.get(tokenKeyOf(s.pairings[0]!.id))]).toEqual([fail, TOKEN]);
    }
  });

  it("with the old token unreadable shows Pair, and the next launch moves it", async () => {
    const k = stubKeychain();
    await legacy(k);
    k.failOnce(0);
    const first = rig(k.keychain);
    await first.fleet.init();
    expect(first.fleet.snapshot().status).toBe("unpaired");
    expect(k.kept.get(OLD)).toBe(TOKEN);
    k.healthy();
    const next = rig(k.keychain);
    await next.fleet.init();
    expect(next.fleet.snapshot().status).toBe("paired");
    expect(k.kept.has(OLD)).toBe(false);
  });

  it("is nothing to do for a phone that never paired, and leaves a migrated phone alone", async () => {
    const k = stubKeychain();
    const r = rig(k.keychain);
    await r.fleet.init();
    expect(r.fleet.snapshot().status).toBe("unpaired");
    expect(k.calls.filter((c) => c.startsWith("set") || c.startsWith("delete"))).toEqual([]);
    expect((await loadSettings()).pairings).toEqual([]);
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
    expect((await loadSettings()).pairings.map((p) => [p.id, p.hubUrl, p.macName])).toEqual([
      ["m00000001", urlOf("studio"), "studio"],
      ["m00000002", urlOf("laptop"), "laptop"],
    ]);
    expect(r.fleet.links.macs()).toEqual(["m00000001", "m00000002"]);
    expect(r.fleet.links.client("m00000002")?.url).toBe(urlOf("laptop"));
  });

  it("pairing a host that is already paired replaces it and keeps its id (and its name)", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const old = await pairWith(r, "studio", "aa".repeat(32));
    await pairWith(r, "laptop", "bb".repeat(32));
    expect(await r.fleet.rename("m00000001", "Studio")).toBe(true);
    await pairWith(r, "studio", "ee".repeat(32));
    expect(old.closed).toBe(true);
    expect(r.fleet.snapshot().macs.map((m) => [m.id, m.macName])).toEqual([
      ["m00000001", "Studio"],
      ["m00000002", "laptop"],
    ]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("ee".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001", "m00000002"]);
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
    const pairing = r.sockets.last;
    pairing.open();
    pairing.receive({ type: "paired", req: pairing.commandsOf("pair")[0]?.req, token: TOKEN, device: "iPhone" });
    await pending;
    expect(linkOf(r, "m00000001")).toEqual({ state: "connecting", tried: false });
    socketOf(r, "studio").drop();
    expect(linkOf(r, "m00000001")).toEqual({ state: "offline", tried: true });
  });
});

describe("keyFor", () => {
  it("resolves a hub's published host and raw id to the phone's key, through the pairings", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio");
    expect(r.fleet.keyFor("studio.tail1234.ts.net", "1-2")).toBe(keyOf("m00000001", "1-2"));
    expect(r.fleet.keyFor(null, "1-2")).toBe(keyOf("m00000001", "1-2"));
    await pairWith(r, "laptop");
    expect(r.fleet.keyFor("Laptop.tail1234.ts.net", "1-2")).toBe(keyOf("m00000002", "1-2"));
    expect(r.fleet.keyFor(null, "1-2")).toBeNull();
    expect(r.fleet.keyFor("air.tail1234.ts.net", "1-2")).toBeNull();
    expect(r.fleet.keyFor("studio.tail1234.ts.net", "m00000002/1-2")).toBeNull();
    // A tap resolves the same way, and opens the Inbox for a session its Mac has ended.
    expect(r.fleet.tapKey({ host: "laptop.tail1234.ts.net", session: "1-2" })).toBe(keyOf("m00000002", "1-2"));
    socketOf(r, "laptop").receive({ type: "sessions", sessions: [] });
    r.clock.advance(50);
    expect(r.fleet.tapKey({ host: "laptop.tail1234.ts.net", session: "1-2" })).toBeNull();
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
  it("with no random source at all, the move and a pairing both succeed", async () => {
    const g = globalThis as { crypto?: unknown; expo?: { uuidv4?: () => string } };
    const crypto = g.crypto;
    const expo = g.expo;
    try {
      Object.defineProperty(globalThis, "crypto", { value: {}, configurable: true });
      g.expo = {};
      const k = stubKeychain();
      k.kept.set(TOKEN_KEY, TOKEN);
      await saveSettings({ hubUrl: urlOf("studio") });
      const r = rig(k.keychain, null);
      r.fleet.setForeground(true);
      await r.fleet.init();
      expect(r.fleet.snapshot().status).toBe("paired");
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
    const pairing = r.sockets.last;
    pairing.open();
    pairing.receive({ type: "paired", req: pairing.commandsOf("pair")[0]?.req, token: TOKEN, device: "iPhone" });
    expect(await pending).toEqual({ ok: true });
    expect(made).toBe(1);
  });

  it("a background stop does not count as the first attempt ending", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    const pairing = r.sockets.last;
    pairing.open();
    pairing.receive({ type: "paired", req: pairing.commandsOf("pair")[0]?.req, token: TOKEN, device: "iPhone" });
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

  it("an unpair while that host pairs again never deletes the token the pairing just saved", async () => {
    const r = rig();
    r.fleet.setForeground(true);
    await r.fleet.init();
    await pairWith(r, "studio", "aa".repeat(32));
    const pending = r.fleet.pair(linkTo("studio"), "iPhone");
    const pairing = r.sockets.last;
    const unpaired = r.fleet.unpair("m00000001");
    pairing.open();
    pairing.receive({ type: "paired", req: pairing.commandsOf("pair")[0]?.req, token: "ee".repeat(32), device: "iPhone" });
    expect(await pending).toEqual({ ok: true });
    await unpaired;
    await settle();
    expect(r.fleet.snapshot().macs.map((m) => m.id)).toEqual(["m00000001"]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000001"))).toBe("ee".repeat(32));
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
    // The next launch is paired with the new token.
    const again = rig();
    again.fleet.setForeground(true);
    await again.fleet.init();
    expect(again.fleet.snapshot().status).toBe("paired");
    socketOf(again, "studio").open();
    expect(socketOf(again, "studio").commandsOf("hello")).toEqual([expect.objectContaining({ token: "ee".repeat(32) })]);
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

  it("a pairing whose token cannot be read is listed, offline and not tried, and can be unpaired", async () => {
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
      { id: "m00000002", macName: "laptop", host: "laptop.tail1234.ts.net" },
    ]);
    expect(again.fleet.links.macs()).toEqual(["m00000001"]);
    // The store holds no link of it: offline, and not tried.
    expect(linkOf(again, "m00000002")).toEqual({ state: "offline", tried: false });
    await again.fleet.unpair("m00000002");
    expect(again.fleet.snapshot().macs.map((m) => m.id)).toEqual(["m00000001"]);
    expect((await loadSettings()).pairings.map((p) => p.id)).toEqual(["m00000001"]);
    expect(await SecureStore.getItemAsync(tokenKeyOf("m00000002"))).toBeNull();
  });

  it("a pairing still waiting for its moved token is listed, and unpairing it ends the move", async () => {
    const k = stubKeychain();
    k.kept.set(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: urlOf("studio") });
    // The copy into pairing 1's entry fails this launch (get old, get new, then set new).
    const set = k.keychain.set;
    k.keychain.set = async () => {
      throw new Error("locked");
    };
    const r = rig(k.keychain);
    await r.fleet.init();
    expect(r.fleet.snapshot()).toMatchObject({
      status: "unpaired",
      macs: [{ id: "m00000001", macName: "studio" }],
    });
    k.keychain.set = set;
    await r.fleet.unpair("m00000001");
    expect(r.fleet.snapshot().macs).toEqual([]);
    expect(k.kept.has(TOKEN_KEY)).toBe(false);
    const s = await loadSettings();
    expect([s.pairings, s.hubUrl]).toEqual([[], null]);
    // Nothing comes back at the next launch.
    const next = rig(k.keychain);
    await next.fleet.init();
    expect(next.fleet.snapshot()).toMatchObject({ status: "unpaired", macs: [] });
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
