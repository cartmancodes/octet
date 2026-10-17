import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";

import * as Notifications from "expo-notifications";
import { Alert } from "react-native";

import { FakeClock, Sockets } from "./client/fakes";
import { Fleet, TOKEN_KEY, UNPAIR_FAILED_NOTICE, tokenKeyOf, unpairedNotice } from "./hub";
import { loadSettings, saveSettings } from "./persist";
import { createOctetStore } from "./store";
import { M, newM } from "./testKeys";

let mockProject: string | undefined = "11111111-2222-3333-4444-555555555555";
jest.mock("expo-constants", () => ({
  __esModule: true,
  default: {
    get expoConfig() {
      return { extra: { eas: { projectId: mockProject } } };
    },
  },
}));

const SECRET = "ab".repeat(32);
const LINK = `https://mac.tail1234.ts.net/#pair=${SECRET}`;
const TOKEN = "cd".repeat(32);
/** The one pairing's keychain entry; `TOKEN_KEY` is where an older version kept its token. */
const KEY = tokenKeyOf(M);
const UNPAIRED_NOTICE = unpairedNotice("mac");

function rig() {
  const sockets = new Sockets();
  const clock = new FakeClock();
  const store = createOctetStore();
  const hub = new Fleet({
    factory: sockets.factory,
    store,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
    newId: newM,
  });
  return { sockets, clock, store, hub };
}
type Rig = ReturnType<typeof rig>;

/** The Mac accepts the pairing the controller just sent. */
function accept(r: Rig, token = TOKEN): void {
  const socket = r.sockets.last;
  socket.open();
  const [command] = socket.commandsOf("pair");
  socket.receive({ type: "paired", req: command?.req, token, device: "iPhone" });
}

/** Pairs, and walks the new connection to ready. */
async function paired(r: Rig): Promise<void> {
  await r.hub.init();
  const pending = r.hub.pair(LINK, "iPhone");
  accept(r);
  expect(await pending).toEqual({ ok: true });
  r.sockets.last.open();
  r.sockets.last.receive({ type: "welcome", device: "iPhone" });
}

beforeEach(async () => {
  mockProject = "11111111-2222-3333-4444-555555555555";
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  await SecureStore.deleteItemAsync(KEY);
  jest.clearAllMocks();
});

describe("starting", () => {
  it("is unpaired without a saved token", async () => {
    const r = rig();
    expect(r.hub.snapshot().status).toBe("loading");
    await r.hub.init();
    expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: null });
    expect(r.hub.links.macs()).toEqual([]);
  });

  it("is unpaired, and drops the token, with a token but no hub address", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    const r = rig();
    await r.hub.init();
    expect(r.hub.snapshot().status).toBe("unpaired");
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
    expect(r.sockets.all).toHaveLength(0);
  });

  it("does not trust a saved hub address that is not a pairing host: unpaired, token dropped", async () => {
    for (const hubUrl of ["ws://mac.ts.net/", "wss://evil.example/path", "https://mac.ts.net/", "wss://mac.ts.net:99999/"]) {
      await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
      await saveSettings({ hubUrl });
      const r = rig();
      r.hub.setForeground(true);
      await r.hub.init();
      expect(r.hub.snapshot().status).toBe("unpaired");
      expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
      expect(r.sockets.all).toHaveLength(0);
    }
  });

  it("connects with the saved token and hub address, when the app is in the foreground", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    expect(r.hub.snapshot().status).toBe("paired");
    expect(r.sockets.last.url).toBe("wss://mac.tail1234.ts.net/");
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("hello")).toEqual([expect.objectContaining({ token: TOKEN })]);
  });

  it("does not connect while the app is in the background", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    const r = rig();
    r.hub.setForeground(false);
    await r.hub.init();
    expect(r.hub.snapshot().status).toBe("paired");
    expect(r.sockets.all).toHaveLength(0);
    r.hub.setForeground(true);
    expect(r.sockets.all).toHaveLength(1);
  });

  it("keeps a good token when the settings cannot be read once at launch", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    jest.mocked(AsyncStorage.getItem).mockRejectedValueOnce(new Error("busy"));
    const first = rig();
    await first.hub.init();
    expect(first.hub.snapshot().status).toBe("unpaired");
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    // The next launch reads them and is paired again, with no new pairing.
    const second = rig();
    await second.hub.init();
    expect(second.hub.snapshot().status).toBe("paired");
  });

  it("takes garbled settings for the defaults: no hub address, so the token goes and Pair shows", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await AsyncStorage.setItem("octet.settings.v1", "{not json");
    const r = rig();
    await r.hub.init();
    expect(r.hub.snapshot().status).toBe("unpaired");
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
  });

  it("pairs over garbled settings: the save replaces them, and the pairing is kept", async () => {
    for (const junk of ["not json", "[]", "null"]) {
      await AsyncStorage.setItem("octet.settings.v1", junk);
      await SecureStore.deleteItemAsync(TOKEN_KEY);
      const r = rig();
      await r.hub.init();
      const pending = r.hub.pair(LINK, "iPhone");
      accept(r);
      expect([junk, await pending]).toEqual([junk, { ok: true }]);
      expect((await loadSettings()).pairings.map((p) => p.hubUrl)).toEqual(["wss://mac.tail1234.ts.net/"]);
      expect(await SecureStore.getItemAsync(KEY)).toBe(TOKEN);
      r.hub.setForeground(false);
    }
  });

  it("is unpaired when the keychain cannot be read", async () => {
    jest.mocked(SecureStore.getItemAsync).mockRejectedValueOnce(new Error("locked"));
    const r = rig();
    await r.hub.init();
    expect(r.hub.snapshot().status).toBe("unpaired");
  });
});

describe("pairing", () => {
  it("sends the secret, name and platform, then keeps the token in the keychain and the hub address in settings", async () => {
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(` ${LINK}\n`, "Shubho's iPhone");
    expect(r.sockets.last.url).toBe("wss://mac.tail1234.ts.net/");
    accept(r);
    expect(await pending).toEqual({ ok: true });
    expect(r.sockets.last.closed).toBe(true); // the pairing socket is done
    expect(await SecureStore.getItemAsync(KEY)).toBe(TOKEN);
    const settings = await loadSettings();
    expect(settings.pairings).toEqual([{ id: M, hubUrl: "wss://mac.tail1234.ts.net/", deviceName: "Shubho's iPhone", macName: "mac" }]);
    expect(settings.deviceName).toBe("Shubho's iPhone");
    // The token is nowhere else on disk.
    expect(JSON.stringify(await AsyncStorage.getAllKeys())).not.toContain(TOKEN);
    expect(JSON.stringify(await AsyncStorage.multiGet(await AsyncStorage.getAllKeys()))).not.toContain(TOKEN);
    expect(r.hub.snapshot().status).toBe("paired");
  });

  it("sent the pair command with the secret, name and platform", async () => {
    const r = rig();
    await r.hub.init();
    void r.hub.pair(LINK, "Pixel");
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("pair")).toEqual([
      expect.objectContaining({ secret: SECRET, name: "Pixel", platform: "ios" }),
    ]);
  });

  it("connects to the hub with the new token once paired", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    await pending;
    const socket = r.sockets.last;
    socket.open();
    expect(socket.commandsOf("hello")).toEqual([expect.objectContaining({ token: TOKEN })]);
  });

  it("refuses a link that is not a pairing link without opening a socket", async () => {
    const r = rig();
    await r.hub.init();
    const result = await r.hub.pair("https://example.com/", "iPhone");
    expect(result).toEqual({ ok: false, message: "That isn't an Octet pairing link", clear: false });
    expect(r.sockets.all).toHaveLength(0);
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
  });

  it("refuses a pair link to a host that is not a .ts.net name, without opening a socket", async () => {
    const r = rig();
    await r.hub.init();
    const result = await r.hub.pair(`https://mac.example.com/#pair=${SECRET}`, "iPhone");
    expect(result).toEqual({ ok: false, message: "Pair links come from tailscale serve (a .ts.net name)", clear: false });
    expect(r.sockets.all).toHaveLength(0);
  });

  it("shows the hub's words when it refuses", async () => {
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    const socket = r.sockets.last;
    socket.open();
    const [command] = socket.commandsOf("pair");
    socket.receive({ type: "refused", req: command?.req, reason: "A device with that name is already paired", code: "invalid" });
    // The Mac kept its code: only a spent or expired one (`unpaired`) is no good any more.
    expect(await pending).toEqual({ ok: false, message: "A device with that name is already paired", clear: false });
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
    expect(r.hub.snapshot().status).toBe("unpaired");
  });

  it("forgets the link only when the Mac says its code is spent or expired", async () => {
    for (const [code, clear] of [["unpaired", true], ["invalid", false], ["rate_limited", false], ["busy", false]] as const) {
      const r = rig();
      await r.hub.init();
      const pending = r.hub.pair(LINK, "iPhone");
      const socket = r.sockets.last;
      socket.open();
      const [command] = socket.commandsOf("pair");
      socket.receive({ type: "refused", req: command?.req, reason: "No", code });
      expect([code, await pending]).toEqual([code, { ok: false, message: "No", clear }]);
    }
  });

  it("says so when the Mac cannot be reached", async () => {
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    r.sockets.last.drop();
    expect(await pending).toEqual({ ok: false, message: "Can't reach your Mac", clear: false });
  });

  it("falls back to the platform's name for a blank device name", async () => {
    const r = rig();
    await r.hub.init();
    void r.hub.pair(LINK, "   ");
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("pair")).toEqual([expect.objectContaining({ name: "iPhone" })]);
  });

  it("keeps the token on this phone only: it never rides a backup to another handset", async () => {
    const only = { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY };
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    await pending;
    expect(SecureStore.setItemAsync).toHaveBeenCalledWith(KEY, TOKEN, only);
    // Read and delete name the same options, so they reach the same keychain item.
    await rig().hub.init(); // the next launch reads it
    expect(SecureStore.getItemAsync).toHaveBeenCalledWith(KEY, only);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "refused", req: 0, reason: "revoked", code: "revoked" });
    await new Promise((done) => setImmediate(done));
    expect(SecureStore.deleteItemAsync).toHaveBeenCalledWith(KEY, only);
  });

  it("clears and retries the keychain write once", async () => {
    jest.mocked(SecureStore.setItemAsync).mockRejectedValueOnce(new Error("locked"));
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    expect(await pending).toEqual({ ok: true });
    expect(SecureStore.setItemAsync).toHaveBeenCalledTimes(2);
    expect(SecureStore.deleteItemAsync).toHaveBeenCalledWith(KEY, expect.anything());
    expect(await SecureStore.getItemAsync(KEY)).toBe(TOKEN);
    expect(r.hub.snapshot().status).toBe("paired");
  });

  it("tells the user how to recover when the keychain refuses the token twice", async () => {
    jest.mocked(SecureStore.setItemAsync).mockRejectedValueOnce(new Error("locked")).mockRejectedValueOnce(new Error("locked"));
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "Work phone");
    accept(r);
    expect(await pending).toEqual({
      ok: false,
      message:
        "This phone paired but couldn't save it. On the Mac run `octet hub revoke Work phone`, then pair again with a new code.",
      clear: true,
    });
    expect(SecureStore.setItemAsync).toHaveBeenCalledTimes(2);
    expect(r.hub.snapshot().status).toBe("unpaired");
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
  });

  it("treats a hub address that cannot be saved like a keychain failure", async () => {
    jest.mocked(AsyncStorage.setItem).mockRejectedValueOnce(new Error("full"));
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    const result = await pending;
    expect(result).toEqual({
      ok: false,
      message:
        "This phone paired but couldn't save it. On the Mac run `octet hub revoke iPhone`, then pair again with a new code.",
      clear: true,
    });
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
    expect(r.hub.snapshot().status).toBe("unpaired");
  });

  it("pairs one link at a time", async () => {
    const r = rig();
    await r.hub.init();
    const first = r.hub.pair(LINK, "iPhone");
    expect(await r.hub.pair(LINK, "iPhone")).toEqual({ ok: false, message: "Already pairing", clear: false });
    expect(r.sockets.all).toHaveLength(1);
    accept(r);
    await first;
  });

  it("names the default device by platform", () => {
    const r = rig();
    expect(r.hub.defaultName()).toBe("iPhone");
    const android = new Fleet({
      factory: r.sockets.factory,
      store: createOctetStore(),
      platform: "android",
    });
    expect(android.defaultName()).toBe("Android phone");
  });
});

describe("unpaired by the Mac", () => {
  it("revoked_unpairs: a revoked or unpaired refusal deletes the token and goes to Pair with a notice", async () => {
    for (const code of ["revoked", "unpaired"] as const) {
      const r = rig();
      r.hub.setForeground(true);
      await paired(r);
      expect(await SecureStore.getItemAsync(KEY)).toBe(TOKEN);
      r.sockets.last.receive({ type: "refused", req: 0, reason: "This device was revoked", code });
      await new Promise((done) => setImmediate(done));
      expect(await SecureStore.getItemAsync(KEY)).toBeNull();
      expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: UNPAIRED_NOTICE });
      expect(r.hub.links.macs()).toEqual([]);
      expect(UNPAIRED_NOTICE).toBe("This phone was unpaired on mac");
    }
  });

  it("revoked_unpairs: a refused hello does the same", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/" });
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    r.sockets.last.open();
    const [hello] = r.sockets.last.commandsOf("hello");
    r.sockets.last.receive({ type: "refused", req: hello?.req, reason: "Unknown device", code: "unpaired" });
    await new Promise((done) => setImmediate(done));
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
    expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: UNPAIRED_NOTICE });
  });

  it("forgets the old Mac's sessions", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    r.sockets.last.receive({
      type: "sessions",
      sessions: [
        { id: "s1", engine: "claude", workspace: "/w", mode: "ask", phase: "idle", approvals: 0, from_phone: false },
      ],
    });
    r.clock.advance(50);
    expect(r.store.getState().sessions).toHaveLength(1);
    r.sockets.last.receive({ type: "refused", req: 0, reason: "revoked", code: "revoked" });
    await new Promise((done) => setImmediate(done));
    expect(r.store.getState().sessions).toHaveLength(0);
  });

  it("a network drop never unpairs", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    r.sockets.last.drop();
    r.clock.advance(60_000);
    r.sockets.last.drop();
    await new Promise((done) => setImmediate(done));
    expect(await SecureStore.getItemAsync(KEY)).toBe(TOKEN);
    expect(r.hub.snapshot()).toMatchObject({ status: "paired", notice: null });
    expect(SecureStore.deleteItemAsync).not.toHaveBeenCalledWith(KEY);
  });

  it("clears the notice when pairing starts again", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    r.sockets.last.receive({ type: "refused", req: 0, reason: "revoked", code: "revoked" });
    await new Promise((done) => setImmediate(done));
    const pending = r.hub.pair(LINK, "iPhone");
    expect(r.hub.snapshot().notice).toBeNull();
    accept(r, "ef".repeat(32));
    await pending;
    expect(await SecureStore.getItemAsync(KEY)).toBe("ef".repeat(32));
  });
});

describe("the app's lifecycle", () => {
  it("stops the client in the background, leaving no timer, and starts it on return", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    const socket = r.sockets.last;
    r.hub.setForeground(false);
    expect(socket.closed).toBe(true);
    expect(r.clock.pending).toBe(0);
    r.hub.setForeground(true);
    expect(r.sockets.last).not.toBe(socket);
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("hello")).toHaveLength(1);
  });

  it("foreground_reconnects_now: returning while a reconnect waits skips the backoff", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    const dropped = r.sockets.last;
    dropped.drop();
    expect(r.clock.delays).toEqual([500]); // the client waits out its backoff
    expect(r.sockets.all).toHaveLength(2);
    r.hub.setForeground(true); // the app is active again
    expect(r.sockets.all).toHaveLength(3);
    expect(r.clock.delays).not.toContain(500); // the backoff timer was cancelled, not left to connect again
    r.clock.advance(500);
    expect(r.sockets.all).toHaveLength(3);
    expect(r.sockets.last).not.toBe(dropped);
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("hello")).toHaveLength(1);
    // A link that is up is left alone.
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    r.hub.setForeground(true);
    expect(r.sockets.all).toHaveLength(3);
  });

  it("sends no ping in the background", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    const socket = r.sockets.last;
    r.hub.setForeground(false);
    r.clock.advance(120_000);
    expect(socket.commandsOf("ping")).toHaveLength(0);
    expect(r.clock.pending).toBe(0);
  });

  it("does nothing without a client", () => {
    const r = rig();
    r.hub.setForeground(false);
    r.hub.setForeground(true);
    expect(r.sockets.all).toHaveLength(0);
  });

  it("gives up an unfinished pairing in the background", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    r.hub.setForeground(false);
    expect((await pending).ok).toBe(false);
  });
});

describe("unpair", () => {
  it("clears the token and the hub address, forgets the Mac's sessions, and shows Pair with no notice", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    const client = r.hub.links.client(r.hub.links.macs()[0] ?? "");
    r.sockets.last.receive({
      type: "sessions",
      sessions: [
        { id: "s1", engine: "claude", workspace: "/w", mode: "ask", phase: "idle", approvals: 0, from_phone: false },
      ],
    });
    r.clock.advance(50);
    expect(r.store.getState().sessions).toHaveLength(1);
    const socket = r.sockets.last;
    await r.hub.unpair();
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
    expect((await loadSettings()).pairings).toEqual([]);
    expect((await loadSettings()).deviceName).toBe("iPhone"); // kept to prefill the next pairing
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: null, offer: false, macs: [], notices: [] });
    expect(r.store.getState().sessions).toHaveLength(0);
    // The connection stays open just long enough for the Mac to hear "notifications off".
    expect(socket.commandsOf("register_push")).toEqual([expect.objectContaining({ token: null })]);
    socket.receive({ type: "ack", req: socket.commandsOf("register_push")[0]?.req, message: "Notifications off" });
    await settle();
    expect(client?.state).toBe("offline");
    expect(socket.closed).toBe(true);
  });

  it("stays quiet when the connection drops afterwards, and a restart is unpaired", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    await r.hub.unpair();
    r.clock.advance(120_000);
    expect(r.sockets.all.filter((s) => !s.closed)).toHaveLength(0);
    const again = rig();
    await again.hub.init();
    expect(again.hub.snapshot().status).toBe("unpaired");
  });

  it("tries the keychain twice; a second try that works leaves no notice", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    (SecureStore.deleteItemAsync as jest.Mock).mockRejectedValueOnce(new Error("locked"));
    await r.hub.unpair();
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: null, offer: false, macs: [], notices: [] });
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
  });

  it("says so when the keychain will not clear: Pair shows a notice, and the next start drops the token", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    (SecureStore.deleteItemAsync as jest.Mock)
      .mockRejectedValueOnce(new Error("locked"))
      .mockRejectedValueOnce(new Error("locked"));
    await r.hub.unpair();
    expect(UNPAIR_FAILED_NOTICE).toBe("This phone couldn't delete its pairing. Pair again to replace it.");
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: UNPAIR_FAILED_NOTICE, offer: false, macs: [], notices: [] });
    expect(await SecureStore.getItemAsync(KEY)).toBe(TOKEN); // still there...
    const again = rig();
    await again.hub.init(); // ...but with no hub address it is dropped
    expect(again.hub.snapshot().status).toBe("unpaired");
  });

  it("says so when the hub address cannot be cleared either", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    jest.spyOn(AsyncStorage, "setItem").mockRejectedValueOnce(new Error("disk full"));
    await r.hub.unpair();
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: UNPAIR_FAILED_NOTICE, offer: false, macs: [], notices: [] });
  });

  it("a foreground event while unpairing does not start the old client again", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    const sockets = r.sockets.all.length;
    const old = r.sockets.last;
    const pending = r.hub.unpair();
    r.hub.setForeground(false);
    r.hub.setForeground(true);
    await pending;
    r.clock.advance(120_000);
    expect(r.sockets.all).toHaveLength(sockets);
    expect(old.closed).toBe(true);
  });

  it("shows Pair at once, and refuses a pairing until the storage writes are done", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await paired(r);
    const pending = r.hub.unpair();
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: null, offer: false, macs: [], notices: [] });
    expect(await r.hub.pair(LINK, "iPhone")).toMatchObject({ ok: false, clear: false });
    await pending;
    r.clock.advance(2_000); // the Mac never answered the goodbye
    expect(r.sockets.all.filter((s) => !s.closed)).toHaveLength(0);
  });

  it("does nothing when not paired, and a second call while one runs is ignored", async () => {
    const r = rig();
    await r.hub.init();
    await r.hub.unpair();
    expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: null });
    const p = rig();
    p.hub.setForeground(true);
    await paired(p);
    await Promise.all([p.hub.unpair(), p.hub.unpair()]);
    expect(p.hub.snapshot().status).toBe("unpaired");
  });
});

const n = Notifications as jest.Mocked<typeof Notifications>;
const PUSH = "ExponentPushToken[abc-123_XYZ]";

/** Lets the promise chains of storage reads and permission checks run. */
const settle = async () => {
  for (let i = 0; i < 30; i++) await Promise.resolve();
};
const registrations = (r: Rig) => r.sockets.all.flatMap((s) => s.commandsOf("register_push"));
const answer = (r: Rig, message = "Notifications on") => {
  const last = r.sockets.last.commandsOf("register_push").at(-1);
  r.sockets.last.receive({ type: "ack", req: last?.req, message });
};

/** A paired app with notifications saved on, its connection ready. */
async function readyWithPush(on = true): Promise<Rig> {
  await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
  await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/", notifications: on });
  const r = rig();
  r.hub.setForeground(true);
  await r.hub.init();
  r.sockets.last.open();
  r.sockets.last.receive({ type: "welcome", device: "iPhone" });
  await settle();
  return r;
}

describe("registering for push", () => {
  it("registers the current token once after ready, when notifications are on", async () => {
    const r = await readyWithPush();
    expect(registrations(r)).toEqual([expect.objectContaining({ type: "register_push", v: 1, token: PUSH })]);
    expect(n.requestPermissionsAsync).not.toHaveBeenCalled(); // registering never prompts
    answer(r);
    await settle();
    expect(registrations(r)).toHaveLength(1);
    expect(r.hub.snapshot().notices).toEqual([]);
  });

  it("registers nothing when notifications are off", async () => {
    const r = await readyWithPush(false);
    expect(registrations(r)).toHaveLength(0);
    expect(n.getExpoPushTokenAsync).not.toHaveBeenCalled();
  });

  it("registers again after each reconnect", async () => {
    const r = await readyWithPush();
    answer(r);
    r.sockets.last.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(registrations(r)).toHaveLength(2);
    expect(r.sockets.all).toHaveLength(2);
  });

  it("registers the new token when the push token rotates, and not while the link is down", async () => {
    const r = await readyWithPush();
    answer(r);
    const rotated = n.addPushTokenListener.mock.calls[0]?.[0];
    expect(rotated).toBeDefined();
    n.getExpoPushTokenAsync.mockResolvedValueOnce({ type: "expo", data: "ExponentPushToken[rotated-1]" } as never);
    rotated?.({} as never);
    await settle();
    expect(registrations(r).map((c) => c.token)).toEqual([PUSH, "ExponentPushToken[rotated-1]"]);
    r.sockets.last.drop();
    rotated?.({} as never);
    await settle();
    expect(registrations(r)).toHaveLength(2); // the next ready sends it
  });

  it("stops listening for rotation when the connection is released", async () => {
    const r = await readyWithPush();
    const subscription = n.addPushTokenListener.mock.results[0]?.value as { remove: jest.Mock };
    await r.hub.unpair();
    expect(subscription.remove).toHaveBeenCalled();
  });

  it("with the permission revoked in the phone's settings tells the Mac to stop, stays on, and says so on Settings", async () => {
    n.getPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    const r = await readyWithPush();
    // The Mac would go on pushing to a phone that shows nothing: it is told "no token".
    expect(registrations(r)).toEqual([expect.objectContaining({ type: "register_push", token: null })]);
    expect(n.getExpoPushTokenAsync).not.toHaveBeenCalled();
    answer(r, "Notifications off");
    await settle();
    expect(r.hub.snapshot().notices).toEqual(["Notifications are off for Octet in the phone's settings."]);
    // The setting stays on, so that a permission given back registers again on the next ready.
    expect((await loadSettings()).notifications).toBe(true);
    r.sockets.last.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(registrations(r).map((c) => c.token)).toEqual([null, PUSH]);
    answer(r);
    await settle();
    expect(r.hub.snapshot().notices).toEqual([]);
  });

  it("with the permission revoked and the Mac refusing the stop, still says why on Settings", async () => {
    n.getPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    const r = await readyWithPush();
    r.sockets.last.receive({ type: "refused", req: registrations(r)[0]?.req, code: "invalid", reason: "No" });
    await settle();
    expect(r.hub.snapshot().notices).toEqual(["Notifications are off for Octet in the phone's settings."]);
  });

  it("shows the hub's refusal on Settings only: never a modal", async () => {
    const alert = jest.spyOn(Alert, "alert").mockImplementation(() => {});
    const r = await readyWithPush();
    const last = registrations(r).at(-1);
    r.sockets.last.receive({ type: "refused", req: last?.req, code: "invalid", reason: "That is not a push token" });
    await settle();
    expect(r.hub.snapshot().notices).toEqual(["That is not a push token"]);
    expect(alert).not.toHaveBeenCalled();
    expect(r.hub.snapshot().status).toBe("paired");
    alert.mockRestore();
  });

  it("clears an old notice when a later registration works", async () => {
    const r = await readyWithPush();
    r.sockets.last.receive({ type: "refused", req: registrations(r)[0]?.req, code: "invalid", reason: "No" });
    await settle();
    expect(r.hub.snapshot().notices).toEqual(["No"]);
    r.sockets.last.drop();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    await settle();
    answer(r);
    await settle();
    expect(r.hub.snapshot().notices).toEqual([]);
  });
});

describe("turning notifications on and off", () => {
  const ready = async () => {
    const r = await readyWithPush(false);
    return r;
  };

  it("on: asks permission, sends the token, and saves it on after the ack", async () => {
    const r = await ready();
    const done = r.hub.turnOnNotifications();
    await settle();
    expect(n.requestPermissionsAsync).toHaveBeenCalledTimes(1);
    expect(registrations(r)).toEqual([expect.objectContaining({ token: PUSH })]);
    expect((await loadSettings()).notifications).toBe(false); // not before the Mac says so
    answer(r);
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(true);
  });

  it("on, denied: says so, sends nothing and stays off", async () => {
    n.requestPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    const r = await ready();
    await expect(r.hub.turnOnNotifications()).resolves.toEqual({
      ok: false,
      message: "Notifications are off for Octet in the phone's settings.",
    });
    expect(registrations(r)).toHaveLength(0);
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("on, without a project id: says so and asks nothing", async () => {
    mockProject = undefined;
    const r = await ready();
    await expect(r.hub.turnOnNotifications()).resolves.toEqual({
      ok: false,
      message: "Notifications need this app's EAS project id. See the README.",
    });
    expect(n.requestPermissionsAsync).not.toHaveBeenCalled();
    expect(registrations(r)).toHaveLength(0);
  });

  it("on, refused by the Mac: shows its words and stays off", async () => {
    const r = await ready();
    const done = r.hub.turnOnNotifications();
    await settle();
    r.sockets.last.receive({ type: "refused", req: registrations(r)[0]?.req, code: "invalid", reason: "Bad token" });
    await expect(done).resolves.toEqual({ ok: false, message: "Bad token" });
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("on, waits a few seconds for the link, then gives up with the usual words", async () => {
    const r = await ready();
    r.sockets.last.drop();
    const done = r.hub.turnOnNotifications();
    await settle();
    r.clock.advance(8_000);
    await expect(done).resolves.toEqual({ ok: false, message: "Can't reach your Mac" });
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("on, sends once the link comes back within the wait", async () => {
    const r = await ready();
    r.sockets.last.drop();
    const done = r.hub.turnOnNotifications();
    await settle();
    r.clock.advance(500);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    await settle();
    answer(r);
    await expect(done).resolves.toEqual({ ok: true });
  });

  it("off: sends an explicit null token, then saves it off", async () => {
    const r = await readyWithPush();
    answer(r);
    const done = r.hub.turnOffNotifications();
    await settle();
    const off = registrations(r).at(-1);
    expect(off).toMatchObject({ type: "register_push", token: null });
    expect(JSON.parse(r.sockets.last.sent.at(-1) ?? "")).toHaveProperty("token", null);
    answer(r, "Notifications off");
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(false);
    expect(r.hub.snapshot().notices).toEqual([]);
  });

  it("off never asks for biometrics", async () => {
    const LocalAuthentication = jest.requireMock("expo-local-authentication") as { authenticateAsync: jest.Mock };
    LocalAuthentication.authenticateAsync.mockClear();
    const r = await readyWithPush();
    answer(r);
    const done = r.hub.turnOffNotifications();
    await settle();
    answer(r, "Notifications off");
    await done;
    expect(LocalAuthentication.authenticateAsync).not.toHaveBeenCalled();
  });

  it("off, with the Mac out of reach: stays on, so the Mac is not left sending", async () => {
    const r = await readyWithPush();
    answer(r);
    r.sockets.last.drop();
    const done = r.hub.turnOffNotifications();
    await settle();
    r.clock.advance(8_000);
    await expect(done).resolves.toEqual({ ok: false, message: "Can't reach your Mac" });
    expect((await loadSettings()).notifications).toBe(true);
  });

  it("with no connection both fail the same way", async () => {
    const r = rig();
    await r.hub.init();
    await expect(r.hub.turnOnNotifications()).resolves.toEqual({ ok: false, message: "Can't reach your Mac" });
    await expect(r.hub.turnOffNotifications()).resolves.toEqual({ ok: false, message: "Can't reach your Mac" });
  });
});

describe("per-kind notifications", () => {
  const KINDS = { approvals: true, errors: true, turns: false };
  /** Ready, with the master switch on and `kinds` saved; the registration made at ready is answered. */
  const on = async (kinds = KINDS) => {
    await saveSettings({ pushKinds: kinds });
    const r = await readyWithPush(true);
    answer(r);
    await settle();
    return r;
  };
  const last = (r: Rig) => registrations(r).at(-1);

  it("turning a kind on sends the token with all three kinds, and saves them after the ack", async () => {
    const r = await on();
    const done = r.hub.setPushKind("turns", true);
    await settle();
    expect(last(r)).toMatchObject({ type: "register_push", token: PUSH, kinds: { approvals: true, errors: true, turns: true } });
    expect((await loadSettings()).pushKinds).toEqual(KINDS); // not before the Mac says so
    answer(r);
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).pushKinds).toEqual({ approvals: true, errors: true, turns: true });
    expect((await loadSettings()).notifications).toBe(true);
  });

  it("turning a kind off sends the rest as they are and keeps notifications on", async () => {
    const r = await on();
    const done = r.hub.setPushKind("errors", false);
    await settle();
    expect(last(r)).toMatchObject({ token: PUSH, kinds: { approvals: true, errors: false, turns: false } });
    answer(r);
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).pushKinds).toEqual({ approvals: true, errors: false, turns: false });
    expect((await loadSettings()).notifications).toBe(true);
  });

  it("turning the last kind off sends a null token: notifications are off", async () => {
    const r = await on({ approvals: false, errors: true, turns: false });
    const done = r.hub.setPushKind("errors", false);
    await settle();
    expect(last(r)).toMatchObject({ type: "register_push", token: null });
    answer(r, "Notifications off");
    await expect(done).resolves.toEqual({ ok: true });
    const saved = await loadSettings();
    expect(saved.notifications).toBe(false);
    expect(saved.pushKinds).toEqual({ approvals: false, errors: false, turns: false });
  });

  it("turning a kind on while notifications are off asks permission, turns them on, and sends the defaults plus that kind", async () => {
    const r = await readyWithPush(false);
    const done = r.hub.setPushKind("turns", true);
    await settle();
    expect(n.requestPermissionsAsync).toHaveBeenCalledTimes(1);
    expect(registrations(r)).toEqual([expect.objectContaining({ token: PUSH, kinds: { approvals: true, errors: true, turns: true } })]);
    expect((await loadSettings()).notifications).toBe(false);
    answer(r);
    await expect(done).resolves.toEqual({ ok: true });
    const saved = await loadSettings();
    expect(saved.notifications).toBe(true);
    expect(saved.pushKinds).toEqual({ approvals: true, errors: true, turns: true });
  });

  it("the defaults, not the kinds kept from before, are what a first kind brings when notifications are off", async () => {
    await saveSettings({ pushKinds: { approvals: false, errors: false, turns: false } });
    const r = await readyWithPush(false);
    const done = r.hub.setPushKind("approvals", true);
    await settle();
    expect(registrations(r)).toEqual([expect.objectContaining({ kinds: { approvals: true, errors: true, turns: false } })]);
    answer(r);
    await done;
  });

  it("with notifications off and the permission denied, sends nothing and changes nothing", async () => {
    n.requestPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    const r = await readyWithPush(false);
    await expect(r.hub.setPushKind("turns", true)).resolves.toEqual({
      ok: false,
      message: "Notifications are off for Octet in the phone's settings.",
    });
    expect(registrations(r)).toHaveLength(0);
    expect((await loadSettings()).notifications).toBe(false);
    expect((await loadSettings()).pushKinds).toEqual(KINDS);
  });

  it("with notifications on and the permission taken away, never prompts, sends nothing, and changes nothing", async () => {
    const r = await on();
    n.getPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    await expect(r.hub.setPushKind("turns", true)).resolves.toMatchObject({ ok: false });
    expect(n.requestPermissionsAsync).not.toHaveBeenCalled();
    expect(registrations(r)).toHaveLength(1);
    expect((await loadSettings()).pushKinds).toEqual(KINDS);
  });

  it("the Mac refusing it leaves the kinds as they were and says the Mac's words", async () => {
    const r = await on();
    const done = r.hub.setPushKind("turns", true);
    await settle();
    r.sockets.last.receive({ type: "refused", req: last(r)?.req, code: "invalid", reason: "Bad kinds" });
    await expect(done).resolves.toEqual({ ok: false, message: "Bad kinds" });
    expect((await loadSettings()).pushKinds).toEqual(KINDS);
  });

  it("with the Mac out of reach waits a few seconds, then leaves the kinds as they were", async () => {
    const r = await on();
    r.sockets.last.drop();
    const done = r.hub.setPushKind("turns", true);
    await settle();
    r.clock.advance(8_000);
    await expect(done).resolves.toEqual({ ok: false, message: "Can't reach your Mac" });
    expect((await loadSettings()).pushKinds).toEqual(KINDS);
    expect((await loadSettings()).notifications).toBe(true);
  });

  it("with no connection fails the same way", async () => {
    const r = rig();
    await r.hub.init();
    await expect(r.hub.setPushKind("turns", true)).resolves.toEqual({ ok: false, message: "Can't reach your Mac" });
  });

  it("two at once go one after the other, each from what the one before saved", async () => {
    const r = await on();
    const a = r.hub.setPushKind("turns", true);
    const b = r.hub.setPushKind("errors", false);
    await settle();
    expect(registrations(r)).toHaveLength(2); // the one at ready, and the first
    answer(r);
    await expect(a).resolves.toEqual({ ok: true });
    await settle();
    expect(last(r)).toMatchObject({ kinds: { approvals: true, errors: false, turns: true } });
    answer(r);
    await expect(b).resolves.toEqual({ ok: true });
    expect((await loadSettings()).pushKinds).toEqual({ approvals: true, errors: false, turns: true });
  });

  it("the registration made at ready carries the saved kinds", async () => {
    await saveSettings({ pushKinds: { approvals: false, errors: true, turns: true } });
    const r = await readyWithPush(true);
    expect(registrations(r)).toEqual([expect.objectContaining({ token: PUSH, kinds: { approvals: false, errors: true, turns: true } })]);
  });

  it("turning notifications on (the step after pairing) sends the saved kinds, or the defaults when none is on", async () => {
    const r = await readyWithPush(false);
    const done = r.hub.turnOnNotifications();
    await settle();
    expect(last(r)).toMatchObject({ token: PUSH, kinds: KINDS });
    answer(r);
    await done;
    await saveSettings({ notifications: false, pushKinds: { approvals: false, errors: false, turns: false } });
    const again = r.hub.turnOnNotifications();
    await settle();
    expect(last(r)).toMatchObject({ token: PUSH, kinds: KINDS });
    answer(r);
    await again;
    expect((await loadSettings()).pushKinds).toEqual(KINDS);
  });

  it("unpairing puts the kinds back to the defaults: a new pairing starts there on the Mac", async () => {
    const r = await on({ approvals: false, errors: false, turns: true });
    await r.hub.unpair();
    expect((await loadSettings()).pushKinds).toEqual(KINDS);
  });
});

describe("the notifications step after pairing", () => {
  it("offers itself after a pairing when the app has a project id, and holds Pair until it is done", async () => {
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    await pending;
    expect(r.hub.snapshot()).toMatchObject({ status: "paired", offer: true });
    r.hub.finishOffer();
    expect(r.hub.snapshot().offer).toBe(false);
  });

  it("is skipped with no project id", async () => {
    mockProject = undefined;
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    await pending;
    expect(r.hub.snapshot()).toMatchObject({ status: "paired", offer: false });
  });

  it("turning on from the step works on the connection pairing made", async () => {
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    await pending;
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    const done = r.hub.turnOnNotifications();
    await settle();
    answer(r);
    await expect(done).resolves.toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(true);
  });
});

describe("unpair and push", () => {
  it("tells the Mac to stop before the connection goes, and does not wait on the answer", async () => {
    const r = await readyWithPush();
    answer(r);
    const socket = r.sockets.last;
    const done = r.hub.unpair();
    expect(r.hub.snapshot().status).toBe("unpaired"); // synchronously
    await done; // the Mac has not answered, and unpair finished anyway
    expect(socket.commandsOf("register_push").at(-1)).toMatchObject({ token: null });
    expect(socket.closed).toBe(false); // the goodbye is still on its way
    answer(r, "Notifications off");
    await settle();
    expect(socket.closed).toBe(true);
    expect((await loadSettings()).notifications).toBe(false);
    // The Mac heard it, so the phone keeps its registration with the push service.
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled();
  });

  it("tells the Mac to forget the Live Activity's tokens too, first, so the hub has it before the goodbye's answer", async () => {
    const r = await readyWithPush();
    answer(r);
    const socket = r.sockets.last;
    await r.hub.unpair();
    const types = socket.commands().map((c) => c.type);
    const clear = types.lastIndexOf("clear_activity");
    expect(clear).toBeGreaterThanOrEqual(0);
    expect(clear).toBeLessThan(types.lastIndexOf("register_push"));
    // Its answer, or a refusal, changes nothing: the goodbye ends on register_push's.
    const sent = socket.commandsOf("clear_activity")[0];
    socket.receive({ type: "refused", req: sent?.req, code: "invalid", reason: "no" });
    answer(r, "Notifications off");
    await settle();
    expect(socket.closed).toBe(true);
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled();
  });

  it("closes the connection after 2 s if the Mac never answers, and drops the push registration", async () => {
    const r = await readyWithPush();
    answer(r);
    const socket = r.sockets.last;
    await r.hub.unpair();
    r.clock.advance(1_999);
    expect(socket.closed).toBe(false);
    expect(n.unregisterForNotificationsAsync).not.toHaveBeenCalled();
    r.clock.advance(1);
    await settle();
    expect(socket.closed).toBe(true);
    expect(r.clock.pending).toBe(0);
    // Expo then reports the token as gone, and the hub drops it.
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
  });

  it("a refused goodbye changes nothing: it still unpairs, and drops the push registration", async () => {
    const r = await readyWithPush();
    answer(r);
    await r.hub.unpair();
    const last = registrations(r).at(-1);
    r.sockets.last.receive({ type: "refused", req: last?.req, code: "invalid", reason: "no" });
    await settle();
    expect(r.sockets.last.closed).toBe(true);
    expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: null });
    expect(await SecureStore.getItemAsync(KEY)).toBeNull();
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
  });

  it("with the link down there is nothing to send: it closes at once and drops the push registration", async () => {
    const r = await readyWithPush();
    answer(r);
    r.sockets.last.drop();
    const socket = r.sockets.last;
    await r.hub.unpair();
    await settle();
    expect(socket.commandsOf("register_push").filter((c) => c.token === null)).toHaveLength(0);
    expect(socket.commandsOf("clear_activity")).toHaveLength(0);
    expect(r.hub.snapshot().status).toBe("unpaired");
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
  });

  it("a push registration that will not drop is no failure: it still unpairs, quietly", async () => {
    n.unregisterForNotificationsAsync.mockRejectedValueOnce(new Error("no native module"));
    const r = await readyWithPush();
    answer(r);
    r.sockets.last.drop();
    await r.hub.unpair();
    await settle();
    expect(n.unregisterForNotificationsAsync).toHaveBeenCalledTimes(1);
    expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: null });
  });

  it("the Mac's goodbye does not feed the store, and a new pairing is not held up", async () => {
    const r = await readyWithPush();
    answer(r);
    await r.hub.unpair();
    r.sockets.last.receive({
      type: "sessions",
      sessions: [{ id: "1-2", engine: "claude", workspace: "/w", mode: "ask", phase: "idle", approvals: 0, from_phone: false }],
    });
    r.clock.advance(50);
    expect(r.store.getState().sessions).toHaveLength(0);
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    await expect(pending).resolves.toEqual({ ok: true });
  });
});

describe("push operations run one at a time", () => {
  it("a turn-off during the registration made at ready is sent after it, so the Mac ends off", async () => {
    let giveToken: (t: { type: string; data: string }) => void = () => {};
    n.getExpoPushTokenAsync.mockReturnValueOnce(new Promise((resolve) => (giveToken = resolve)) as never);
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/", notifications: true });
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    await settle(); // the registration waits for its token
    const off = r.hub.turnOffNotifications();
    await settle();
    expect(registrations(r)).toHaveLength(0); // nothing went out of turn
    giveToken({ type: "expo", data: PUSH });
    await settle();
    answer(r);
    await settle();
    answer(r, "Notifications off");
    await expect(off).resolves.toEqual({ ok: true });
    expect(registrations(r).map((c) => c.token)).toEqual([PUSH, null]);
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("a registration that finds notifications turned off while it waited for the token sends nothing", async () => {
    let giveToken: (t: { type: string; data: string }) => void = () => {};
    n.getExpoPushTokenAsync.mockReturnValueOnce(new Promise((resolve) => (giveToken = resolve)) as never);
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    await saveSettings({ hubUrl: "wss://mac.tail1234.ts.net/", notifications: true });
    const r = rig();
    r.hub.setForeground(true);
    await r.hub.init();
    r.sockets.last.open();
    r.sockets.last.receive({ type: "welcome", device: "iPhone" });
    await settle();
    await saveSettings({ notifications: false });
    giveToken({ type: "expo", data: PUSH });
    await settle();
    expect(registrations(r)).toHaveLength(0);
  });
});

describe("a push token that never comes", () => {
  afterEach(() => jest.useRealTimers());

  it("turning on gives up after 15 s with the usual words, so the switch is free again", async () => {
    const r = await readyWithPush(false);
    jest.useFakeTimers();
    n.getExpoPushTokenAsync.mockReturnValue(new Promise(() => {}) as never);
    let result: unknown;
    void r.hub.turnOnNotifications().then((value) => (result = value));
    await jest.advanceTimersByTimeAsync(14_999);
    expect(result).toBeUndefined();
    await jest.advanceTimersByTimeAsync(1);
    expect(result).toEqual({ ok: false, message: "Couldn't turn on notifications. Try again." });
    expect(registrations(r)).toHaveLength(0);
    expect((await loadSettings()).notifications).toBe(false);
  });

  it("a fetch that hangs at ready does not hold up a later turn-off", async () => {
    jest.useFakeTimers();
    n.getExpoPushTokenAsync.mockReturnValueOnce(new Promise(() => {}) as never);
    const r = await readyWithPush();
    let result: unknown;
    void r.hub.turnOffNotifications().then((value) => (result = value));
    await settle();
    expect(registrations(r)).toHaveLength(0); // waiting its turn behind the hung fetch
    await jest.advanceTimersByTimeAsync(15_000);
    expect(registrations(r).map((c) => c.token)).toEqual([null]);
    answer(r, "Notifications off");
    await jest.advanceTimersByTimeAsync(0);
    expect(result).toEqual({ ok: true });
    expect((await loadSettings()).notifications).toBe(false);
  });
});

describe("a stale answer", () => {
  it("an ack for a turn-on that the unpair overtook saves nothing", async () => {
    const r = await readyWithPush(false);
    const socket = r.sockets.last;
    const done = r.hub.turnOnNotifications();
    await settle();
    const turnOn = registrations(r)[0];
    await r.hub.unpair();
    socket.receive({ type: "ack", req: turnOn?.req, message: "Notifications on" });
    await expect(done).resolves.toMatchObject({ ok: false });
    await settle();
    expect((await loadSettings()).notifications).toBe(false);
  });
});

describe("a revoke", () => {
  it("also turns notifications off, so that the next pairing starts off", async () => {
    const r = await readyWithPush();
    answer(r);
    expect((await loadSettings()).notifications).toBe(true);
    r.sockets.last.receive({ type: "refused", req: 0, reason: "This device was revoked", code: "revoked" });
    await settle();
    expect(r.hub.snapshot().status).toBe("unpaired");
    expect((await loadSettings()).notifications).toBe(false);
  });
});

