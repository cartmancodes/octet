import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";

import { FakeClock, Sockets } from "./client/fakes";
import { HubController, TOKEN_KEY, UNPAIRED_NOTICE } from "./hub";
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

  it("is unpaired with a token but no hub address", async () => {
    await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
    const r = rig();
    await r.hub.init();
    expect(r.hub.snapshot().status).toBe("unpaired");
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
    expect(result).toEqual({ ok: false, message: "That isn't an Octet pairing link" });
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
    expect(await pending).toEqual({ ok: false, message: "That pairing code has expired" });
    expect(await SecureStore.getItemAsync(TOKEN_KEY)).toBeNull();
    expect(r.hub.snapshot().status).toBe("unpaired");
  });

  it("says so when the Mac cannot be reached", async () => {
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    r.sockets.last.drop();
    expect(await pending).toEqual({ ok: false, message: "Can't reach your Mac" });
  });

  it("falls back to the platform's name for a blank device name", async () => {
    const r = rig();
    await r.hub.init();
    void r.hub.pair(LINK, "   ");
    r.sockets.last.open();
    expect(r.sockets.last.commandsOf("pair")).toEqual([expect.objectContaining({ name: "iPhone" })]);
  });

  it("does not go on when the keychain refuses the token", async () => {
    jest.mocked(SecureStore.setItemAsync).mockRejectedValueOnce(new Error("locked"));
    const r = rig();
    await r.hub.init();
    const pending = r.hub.pair(LINK, "iPhone");
    accept(r);
    const result = await pending;
    expect(result.ok).toBe(false);
    expect(r.hub.snapshot().status).toBe("unpaired");
  });

  it("pairs one link at a time", async () => {
    const r = rig();
    await r.hub.init();
    const first = r.hub.pair(LINK, "iPhone");
    expect(await r.hub.pair(LINK, "iPhone")).toEqual({ ok: false, message: "Already pairing" });
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
