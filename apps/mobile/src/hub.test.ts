import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";

import { FakeClock, Sockets } from "./client/fakes";
import { HubController, TOKEN_KEY, UNPAIRED_NOTICE, UNPAIR_FAILED_NOTICE } from "./hub";
import { loadSettings, saveSettings } from "./persist";
import { createOctetStore } from "./store";

const SECRET = "ab".repeat(32);
const LINK = `https://mac.tail1234.ts.net/#pair=${SECRET}`;
const TOKEN = "cd".repeat(32);

function rig() {
  const sockets = new Sockets();
  const clock = new FakeClock();
  const store = createOctetStore();
  const hub = new HubController({
    factory: sockets.factory,
    store,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
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
  await AsyncStorage.clear();
  await SecureStore.deleteItemAsync(TOKEN_KEY);
  jest.clearAllMocks();
});

describe("starting", () => {
  it("is unpaired without a saved token", async () => {
    const r = rig();
    expect(r.hub.snapshot().status).toBe("loading");
    await r.hub.init();
    expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: null, client: null });
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    const settings = await loadSettings();
    expect(settings.hubUrl).toBe("wss://mac.tail1234.ts.net/");
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
  });

  it("shows the hub's words when it refuses", async () => {
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    const socket = r.sockets.last;
    socket.open();
    const [command] = socket.commandsOf("pair");
    socket.receive({ type: "refused", req: command?.req, reason: "That pairing code has expired", code: "invalid" });
    expect(await pending).toEqual({ ok: false, message: "That pairing code has expired", clear: true });
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
    expect(r.hub.snapshot().status).toBe("unpaired");
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
    expect(SecureStore.setItemAsync).toHaveBeenCalledWith(TOKEN_KEY, TOKEN, only);
    // Read and delete name the same options, so they reach the same keychain item.
    expect(SecureStore.getItemAsync).toHaveBeenCalledWith(TOKEN_KEY, only);
    r.sockets.last.open();
    r.sockets.last.receive({ type: "refused", req: 0, reason: "revoked", code: "revoked" });
    await new Promise((done) => setImmediate(done));
    expect(SecureStore.deleteItemAsync).toHaveBeenCalledWith(TOKEN_KEY, only);
  });

  it("clears and retries the keychain write once", async () => {
    jest.mocked(SecureStore.setItemAsync).mockRejectedValueOnce(new Error("locked"));
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    expect(await pending).toEqual({ ok: true });
    expect(SecureStore.setItemAsync).toHaveBeenCalledTimes(2);
    expect(SecureStore.deleteItemAsync).toHaveBeenCalledWith(TOKEN_KEY, expect.anything());
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
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
    const android = new HubController({
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
      expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
      r.sockets.last.receive({ type: "refused", req: 0, reason: "This device was revoked", code });
      await new Promise((done) => setImmediate(done));
      expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
      expect(r.hub.snapshot()).toMatchObject({ status: "unpaired", notice: UNPAIRED_NOTICE, client: null });
      expect(UNPAIRED_NOTICE).toBe("This phone was unpaired on the Mac");
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN);
    expect(r.hub.snapshot()).toMatchObject({ status: "paired", notice: null });
    expect(SecureStore.deleteItemAsync).not.toHaveBeenCalledWith(TOKEN_KEY);
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe("ef".repeat(32));
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
    const client = r.hub.snapshot().client;
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
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
    expect((await loadSettings()).hubUrl).toBeNull();
    expect((await loadSettings()).deviceName).toBe("iPhone"); // kept to prefill the next pairing
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: null, client: null });
    expect(r.store.getState().sessions).toHaveLength(0);
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
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: null, client: null });
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
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
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: UNPAIR_FAILED_NOTICE, client: null });
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBe(TOKEN); // still there...
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
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: UNPAIR_FAILED_NOTICE, client: null });
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
    expect(r.hub.snapshot()).toEqual({ status: "unpaired", notice: null, client: null });
    expect(await r.hub.pair(LINK, "iPhone")).toMatchObject({ ok: false, clear: false });
    await pending;
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
