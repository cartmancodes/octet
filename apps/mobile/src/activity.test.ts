// The Live Activity while the app runs: what it shows (the hub's own rules, on the same scenarios as
// crates/octet-hub/src/activity.rs), how often it changes, the tokens each Mac is given, what the
// widget's App Group holds, Deny, and the switch. The native module is a fake here: the wrapper's
// own fallback is tested in modules/octet-live.
import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";

import type { ActivityState, WidgetEntry } from "../modules/octet-live";
import {
  COMMAND_CHARS,
  DRIVE_GAP_MS,
  ENTRIES_BYTES,
  ENTRIES_LIMIT,
  IDLE_END_MS,
  LiveActivity,
  NAME_CHARS,
  ActivityModel,
  widgetEntries,
  type ActivityRow,
} from "./activity";
import { FakeClock, Sockets, type FakeSocket } from "./client/fakes";
import { FakeLive } from "./liveRig";
import { Fleet, tokenKeyOf } from "./hub";
import { keyOf } from "./macs";
import { loadSettings, saveSettings } from "./persist";
import type { SessionInfo } from "./protocol";
import { createOctetStore, type ListedSession } from "./store";

jest.mock("expo-constants", () => ({ __esModule: true, default: { expoConfig: { extra: {} } } }));

const TOKEN = "cd".repeat(32);
const STUDIO = "m00000001";
const MINI = "m00000002";
const STUDIO_HOST = "studio.tail1234.ts.net";
const MINI_HOST = "mini.tail1234.ts.net";
const urlOf = (host: string) => `wss://${host}/`;
const A = "101-1790000000";
const B = "202-1790000000";

const settle = async () => {
  for (let i = 0; i < 60; i++) await Promise.resolve();
};

const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/Users/me/Projects/${id}`,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

const approval = (id: number, command: string | null = "cargo test", action = "Run a command") => ({
  id,
  summary: { action, command, files: [], scope: "once" },
});

/** Two Macs paired (studio, then mini), both links ready, and the controller over them. */
async function rig({ macs = [STUDIO_HOST, MINI_HOST], supported = true }: { macs?: string[]; supported?: boolean } = {}) {
  const ids = [STUDIO, MINI];
  for (const i of macs.keys()) await SecureStore.setItemAsync(tokenKeyOf(ids[i] ?? ""), TOKEN);
  await saveSettings({
    deviceName: "iPhone",
    pairings: macs.map((host, i) => ({ id: ids[i] ?? "", hubUrl: urlOf(host), deviceName: "iPhone", macName: host.split(".")[0] ?? "" })),
  });
  const sockets = new Sockets();
  const clock = new FakeClock();
  const store = createOctetStore();
  const fleet = new Fleet({
    factory: sockets.factory,
    store,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
  });
  const api = new FakeLive(clock.now);
  api.supported = supported;
  const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
  fleet.setForeground(true);
  await fleet.init();
  live.start();
  live.setForeground(true);
  await settle();
  const socketOf = (host: string): FakeSocket => {
    const all = sockets.all.filter((s) => s.url === urlOf(host));
    const socket = all[all.length - 1];
    if (!socket) throw new Error(`no socket to ${host}`);
    return socket;
  };
  const ready = async (host: string) => {
    const socket = socketOf(host);
    socket.open();
    socket.receive({ type: "welcome", device: "iPhone" });
    await settle();
  };
  for (const host of macs) await ready(host);
  /** Mac `host` lists `list`; the batch is delivered. */
  const list = async (host: string, sessions: SessionInfo[]) => {
    socketOf(host).receive({ type: "sessions", sessions });
    clock.advance(50);
    await settle();
  };
  const advance = async (ms: number) => {
    clock.advance(ms);
    await settle();
  };
  /** The Mac acks every command of `type` it has not acked yet. */
  const ack = async (host: string, type: string) => {
    for (const c of socketOf(host).commandsOf(type)) socketOf(host).receive({ type: "ack", req: c.req, message: "ok" });
    await settle();
  };
  return { sockets, clock, store, fleet, api, live, socketOf, ready, list, advance, ack };
}

beforeEach(async () => {
  await AsyncStorage.clear();
  for (const id of [STUDIO, MINI]) await SecureStore.deleteItemAsync(tokenKeyOf(id));
});

describe("what the activity shows: the hub's rules (activity.rs the_lead_and_since_follow_the_rulings)", () => {
  const BASE = 1_790_000_000_000;
  const at = (seconds: number) => BASE + seconds * 1000;
  /** A row as the hub's `snap(id, phase, approvals)`: the front is the first approval. */
  const snap = (key: string, phase: string, approvals: number[] = []): ActivityRow => ({
    key,
    phase,
    approvals: approvals.length,
    front: approvals[0] ?? null,
  });
  const idle = (key: string) => snap(key, "idle");

  it("leads with the oldest approval, then the newest running turn, then the last to finish", () => {
    const model = new ActivityModel();
    model.observe([idle("s1"), idle("s2"), idle("s3")], at(0));
    // The one with the oldest waiting approval leads; the count is every session's.
    model.observe([snap("s1", "idle", [1]), idle("s2"), idle("s3")], at(1));
    model.observe([snap("s1", "idle", [1]), snap("s2", "running", [5, 6]), idle("s3")], at(2));
    expect(model.content(at(2))).toEqual({ kind: "approval", key: "s1", since: at(1), approvals: 3 });
    // s1's approval is answered and another comes: s2's are older now.
    model.observe([snap("s1", "idle", [2]), snap("s2", "running", [5, 6]), idle("s3")], at(3));
    expect(model.content(at(3))).toMatchObject({ key: "s2", since: at(2) });
    // No approvals: the most recently started running session leads, since its turn began.
    model.observe([idle("s1"), snap("s2", "running"), snap("s3", "running")], at(4));
    expect(model.content(at(4))).toEqual({ kind: "running", key: "s3", since: at(4), approvals: 0 });
    model.observe([idle("s1"), snap("s2", "running"), idle("s3")], at(5));
    // s2 has been running since its approvals were answered.
    expect(model.content(at(5))).toMatchObject({ key: "s2", since: at(4) });
    // All idle: the last that finished leads, since it finished.
    model.observe([idle("s1"), idle("s2"), idle("s3")], at(7));
    expect(model.content(at(7))).toMatchObject({ kind: "done", key: "s2", since: at(7) });
    // A session that loses its state runs no more and has nothing open.
    model.observe([snap("s1", "running", [9]), idle("s2"), idle("s3")], at(8));
    model.observe([idle("s1"), idle("s2"), idle("s3")], at(9));
    expect(model.content(at(9))).toMatchObject({ kind: "done", key: "s1", approvals: 0 });
  });

  it("sessions that never ran lead nothing: no session, since every session went idle", () => {
    const model = new ActivityModel();
    model.observe([idle("s1")], at(0));
    expect(model.content(at(5))).toEqual({ kind: "done", key: "", since: at(0), approvals: 0 });
  });

  it("a session gone from the list is forgotten", () => {
    const model = new ActivityModel();
    model.observe([snap("s1", "running")], at(0));
    model.observe([], at(1));
    expect(model.content(at(2))).toEqual({ kind: "done", key: "", since: at(1), approvals: 0 });
  });
});

describe("the controller", () => {
  it("shows the lead with the hub's raw id and its Mac's host, as the hub's content state does", async () => {
    const r = await rig();
    await r.list(MINI_HOST, [info(B, { phase: "running" })]);
    await r.list(STUDIO_HOST, [info(A, { approvals: 2, approval: approval(4) })]);
    await r.advance(DRIVE_GAP_MS);
    const last = r.api.shown().at(-1);
    expect(last?.arg).toEqual({ kind: "approval", session: A, mac: STUDIO_HOST, since: expect.any(Number), approvals: 2 });
    expect(Number.isInteger((last?.arg as ActivityState).since)).toBe(true);
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("updates at most once a second, and the last one shows what is latest", async () => {
    const r = await rig();
    await r.list(STUDIO_HOST, [info(A, { phase: "running" })]);
    expect(r.api.shown()).toHaveLength(1);
    for (let n = 1; n <= 5; n++) {
      await r.list(STUDIO_HOST, [info(A, { phase: "running", approvals: n, approval: approval(1) })]);
      await r.advance(100);
    }
    await r.advance(DRIVE_GAP_MS);
    const shown = r.api.shown();
    for (let i = 1; i < shown.length; i++) expect((shown[i]?.at ?? 0) - (shown[i - 1]?.at ?? 0)).toBeGreaterThanOrEqual(DRIVE_GAP_MS);
    expect(shown.length).toBeLessThanOrEqual(3);
    expect(shown.at(-1)?.arg).toMatchObject({ kind: "approval", approvals: 5 });
  });

  it("ends the activity five minutes after every session went idle", async () => {
    const r = await rig();
    await r.list(STUDIO_HOST, [info(A, { phase: "running" })]);
    await r.list(STUDIO_HOST, [info(A, { phase: "idle" })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.shown().at(-1)?.arg).toMatchObject({ kind: "done", session: A });
    expect(r.api.of("end")).toHaveLength(0);
    await r.advance(IDLE_END_MS);
    expect(r.api.of("end")).toHaveLength(1);
  });

  it("a start that fails (Live Activities off) is caught, and not tried again until something changes", async () => {
    const r = await rig();
    r.api.failStart = true;
    await r.list(STUDIO_HOST, [info(A, { phase: "running" })]);
    await r.advance(10 * DRIVE_GAP_MS);
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("does nothing in the background, and shows the state again in the foreground", async () => {
    const r = await rig();
    r.live.setForeground(false);
    await r.list(STUDIO_HOST, [info(A, { phase: "running" })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.shown()).toHaveLength(0);
    r.live.setForeground(true);
    await settle();
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("does nothing at all where Live Activities are not supported", async () => {
    const r = await rig({ supported: false });
    await r.list(STUDIO_HOST, [info(A, { phase: "running", approvals: 1, approval: approval(1) })]);
    r.api.pushToStartToken("ab12");
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.calls).toEqual([]);
    expect(r.api.listening()).toBe(0);
    expect(r.socketOf(STUDIO_HOST).commandsOf("register_activity")).toEqual([]);
  });
});

describe("registration", () => {
  it("registers the tokens with every Mac, once each however often the same token comes", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    r.api.pushToStartToken("ab12"); // ActivityKit may say the first one twice
    await settle();
    for (const host of [STUDIO_HOST, MINI_HOST]) {
      expect(r.socketOf(host).commandsOf("register_activity")).toEqual([
        expect.not.objectContaining({ token: expect.anything() }),
      ]);
      expect(r.socketOf(host).commandsOf("register_activity")[0]).toMatchObject({ start_token: "ab12" });
      await r.ack(host, "register_activity");
    }
    r.api.pushToStartToken("ab12");
    await settle();
    // An activity's token joins it; each registration states both.
    r.api.pushToken("cd34");
    await settle();
    for (const host of [STUDIO_HOST, MINI_HOST]) {
      expect(r.socketOf(host).commandsOf("register_activity").map(({ token, start_token }) => ({ token, start_token }))).toEqual([
        { token: undefined, start_token: "ab12" },
        { token: "cd34", start_token: "ab12" },
      ]);
    }
  });

  it("a token that changes while a Mac is out of reach goes to it on its next connection", async () => {
    const r = await rig();
    r.socketOf(MINI_HOST).drop();
    await settle();
    r.api.pushToStartToken("ab12");
    await settle();
    expect(r.socketOf(STUDIO_HOST).commandsOf("register_activity")).toHaveLength(1);
    expect(r.socketOf(MINI_HOST).commandsOf("register_activity")).toHaveLength(0);
    await r.advance(60_000); // the link tries again
    await r.ready(MINI_HOST);
    expect(r.socketOf(MINI_HOST).commandsOf("register_activity")).toEqual([expect.objectContaining({ start_token: "ab12" })]);
  });

  it("an ended activity's token is taken back: only the push-to-start token is left", async () => {
    const r = await rig({ macs: [STUDIO_HOST] });
    r.api.pushToStartToken("ab12");
    r.api.pushToken("cd34");
    await settle();
    await r.ack(STUDIO_HOST, "register_activity");
    r.api.pushToken(null);
    await settle();
    expect(r.socketOf(STUDIO_HOST).commandsOf("register_activity").at(-1)).toMatchObject({ start_token: "ab12" });
    expect(r.socketOf(STUDIO_HOST).commandsOf("register_activity").at(-1)).not.toHaveProperty("token");
  });
});

describe("the widget's App Group entries", () => {
  const entries = (r: Awaited<ReturnType<typeof rig>>) => r.api.of("setEntries").at(-1)?.arg as Record<string, WidgetEntry> | undefined;

  it("names each waiting approval's session by host and raw id, cleaned, and the activity is shown again after each write", async () => {
    const r = await rig();
    const command = `echo start\n${"\n".repeat(12)}rm -rf / \u001b[31m${"x".repeat(400)}`;
    await r.list(STUDIO_HOST, [
      info(A, { workspace: `/w/${"n".repeat(120)}`, approvals: 1, approval: approval(3, command), last_activity: 10 }),
      info("303-1790000000", { phase: "running" }),
    ]);
    await r.list(MINI_HOST, [info(B, { approvals: 1, approval: approval(7, null, "Edit files"), last_activity: 20 })]);
    const got = entries(r);
    expect(Object.keys(got ?? {})).toEqual([`${MINI_HOST}/${B}`, `${STUDIO_HOST}/${A}`]);
    const studio = got?.[`${STUDIO_HOST}/${A}`];
    expect(studio?.approval).toBe("3");
    expect(studio?.name.length).toBeLessThanOrEqual(NAME_CHARS);
    expect(studio?.name).toBe("n".repeat(NAME_CHARS));
    expect(studio?.command.length).toBeLessThanOrEqual(COMMAND_CHARS);
    expect(studio?.command).toContain("⋯ 12 blank lines ⋯");
    expect(studio?.command).not.toContain("\u001b");
    expect(got?.[`${MINI_HOST}/${B}`]).toEqual({ name: B, command: "Edit files", approval: "7" });
    // Every write is followed by the activity being shown again, so the widget reads it anew.
    const writes = r.api.calls.map((c) => c.what);
    const lastWrite = writes.lastIndexOf("setEntries");
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.calls.slice(lastWrite + 1).some((c) => c.what === "update" || c.what === "start")).toBe(true);
  });

  it("keeps at most 64 entries, newest first, within 64 KiB", async () => {
    const r = await rig({ macs: [STUDIO_HOST] });
    const many = Array.from({ length: 70 }, (_, i) =>
      info(`${1000 + i}-1790000000`, { approvals: 1, approval: approval(1, "😀".repeat(300)), workspace: `/w/${"é".repeat(90)}`, last_activity: i }),
    );
    await r.list(STUDIO_HOST, many);
    const got = entries(r) ?? {};
    const keys = Object.keys(got);
    expect(keys.length).toBeLessThanOrEqual(ENTRIES_LIMIT);
    expect(keys[0]).toBe(`${STUDIO_HOST}/1069-1790000000`);
    expect(Buffer.byteLength(JSON.stringify(got), "utf8")).toBeLessThanOrEqual(ENTRIES_BYTES);
    for (const entry of Object.values(got)) for (const value of Object.values(entry)) expect(typeof value).toBe("string");
  });

  it("is rewritten when the approvals change, and an unpaired Mac's entries leave it", async () => {
    const r = await rig();
    await r.list(STUDIO_HOST, [info(A, { approvals: 1, approval: approval(1) })]);
    await r.list(MINI_HOST, [info(B, { approvals: 1, approval: approval(2) })]);
    expect(Object.keys(entries(r) ?? {}).sort()).toEqual([`${MINI_HOST}/${B}`, `${STUDIO_HOST}/${A}`]);
    await r.list(STUDIO_HOST, [info(A, { approvals: 1, approval: approval(5) })]);
    expect(entries(r)?.[`${STUDIO_HOST}/${A}`]?.approval).toBe("5");
    await r.fleet.unpair(STUDIO);
    await settle();
    expect(Object.keys(entries(r) ?? {})).toEqual([`${MINI_HOST}/${B}`]);
    await r.fleet.unpair(MINI);
    await settle();
    expect(entries(r)).toEqual({});
  });

  it("widgetEntries skips a session whose Mac has no host", () => {
    const row = { ...info(A, { approvals: 1, approval: approval(1) }), id: keyOf(STUDIO, A), mac: STUDIO, raw: A } as ListedSession;
    expect(widgetEntries([row], () => null)).toEqual({});
    expect(Object.keys(widgetEntries([row], () => STUDIO_HOST))).toEqual([`${STUDIO_HOST}/${A}`]);
  });
});

describe("Deny from the Island", () => {
  it("sends allow:false to the Mac the host names, and only once for the same approval", async () => {
    const r = await rig();
    await r.list(STUDIO_HOST, [info(A, { approvals: 1, approval: approval(3) })]);
    await r.list(MINI_HOST, [info(A, { approvals: 1, approval: approval(3) })]);
    r.api.deny({ mac: MINI_HOST, session: A, approval: "3" });
    r.api.deny({ mac: MINI_HOST, session: A, approval: "3" });
    await settle();
    expect(r.socketOf(MINI_HOST).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 3, allow: false })]);
    expect(r.socketOf(STUDIO_HOST).commandsOf("answer")).toEqual([]);
  });

  it("sends nothing for an unknown host, no host with several Macs, a bad session or a bad approval id", async () => {
    const r = await rig();
    for (const event of [
      { mac: "evil.example.com", session: A, approval: "3" },
      { mac: "", session: A, approval: "3" },
      { mac: STUDIO_HOST, session: "../settings", approval: "3" },
      { mac: STUDIO_HOST, session: A, approval: "3x" },
      { mac: STUDIO_HOST, session: A, approval: "-1" },
      { mac: STUDIO_HOST, session: A, approval: "99999999999999999999" },
    ]) {
      r.api.deny(event);
    }
    await settle();
    for (const host of [STUDIO_HOST, MINI_HOST]) expect(r.socketOf(host).commandsOf("answer")).toEqual([]);
  });

  it("with one Mac, no host is that Mac's", async () => {
    const r = await rig({ macs: [STUDIO_HOST] });
    r.api.deny({ mac: "", session: A, approval: "4" });
    await settle();
    expect(r.socketOf(STUDIO_HOST).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 4, allow: false })]);
  });

  it("a Deny handed over before the pairings are read is answered once they are", async () => {
    await SecureStore.setItemAsync(tokenKeyOf(STUDIO), TOKEN);
    await saveSettings({ pairings: [{ id: STUDIO, hubUrl: urlOf(STUDIO_HOST), deviceName: "iPhone", macName: "studio" }] });
    const sockets = new Sockets();
    const clock = new FakeClock();
    const store = createOctetStore();
    const fleet = new Fleet({ factory: sockets.factory, store, platform: "ios", clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 } });
    const api = new FakeLive(clock.now);
    const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
    live.start();
    api.deny({ mac: STUDIO_HOST, session: A, approval: "6" }); // a tap made while the app was closed
    fleet.setForeground(true);
    await fleet.init();
    await settle();
    const socket = sockets.last;
    socket.open();
    socket.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(socket.commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 6, allow: false })]);
    live.stop();
  });

  it("a Deny while the Mac is out of reach goes on its next connection", async () => {
    const r = await rig({ macs: [STUDIO_HOST] });
    r.socketOf(STUDIO_HOST).drop();
    await settle();
    r.api.deny({ mac: STUDIO_HOST, session: A, approval: "4" });
    await settle();
    await r.advance(60_000);
    await r.ready(STUDIO_HOST);
    expect(r.socketOf(STUDIO_HOST).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 4, allow: false })]);
  });
});

describe("the switch", () => {
  it("is on by default", async () => {
    expect((await loadSettings()).liveActivity).toBe(true);
    const r = await rig();
    expect(r.live.enabled).toBe(true);
    expect(r.live.supported).toBe(true);
  });

  it("off: ends the activity, clears every Mac's tokens and the App Group, and registers nothing; on registers again", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    await r.list(STUDIO_HOST, [info(A, { phase: "running", approvals: 1, approval: approval(1) })]);
    for (const host of [STUDIO_HOST, MINI_HOST]) await r.ack(host, "register_activity");
    expect(await r.live.setEnabled(false)).toBe(true);
    await settle();
    expect((await loadSettings()).liveActivity).toBe(false);
    expect(r.api.of("end")).toHaveLength(1);
    expect(r.api.of("setEntries").at(-1)?.arg).toEqual({});
    for (const host of [STUDIO_HOST, MINI_HOST]) expect(r.socketOf(host).commandsOf("clear_activity")).toHaveLength(1);
    const before = r.api.calls.length;
    r.api.pushToken("ef56");
    await r.list(STUDIO_HOST, [info(A, { phase: "running", approvals: 2, approval: approval(1) })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.calls.length).toBe(before);
    for (const host of [STUDIO_HOST, MINI_HOST]) expect(r.socketOf(host).commandsOf("register_activity")).toHaveLength(1);

    expect(await r.live.setEnabled(true)).toBe(true);
    await settle();
    for (const host of [STUDIO_HOST, MINI_HOST]) {
      expect(r.socketOf(host).commandsOf("register_activity").at(-1)).toMatchObject({ token: "ef56", start_token: "ab12" });
    }
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start").length + r.api.of("update").length).toBeGreaterThan(0);
  });

  it("off while a start is on its way: that activity is ended too", async () => {
    const r = await rig({ macs: [STUDIO_HOST] });
    let finish = () => {};
    r.api.start = async (state) => {
      r.api.calls.push({ what: "start", at: r.clock.now(), arg: state });
      await new Promise<void>((resolve) => (finish = resolve));
    };
    await r.list(STUDIO_HOST, [info(A, { phase: "running" })]);
    expect(r.api.of("start")).toHaveLength(1);
    await r.live.setEnabled(false);
    expect(r.api.of("end")).toHaveLength(1);
    finish();
    await settle();
    expect(r.api.of("end")).toHaveLength(2);
  });

  it("off from the start: nothing is shown or registered", async () => {
    await saveSettings({ liveActivity: false });
    const r = await rig();
    expect(r.live.enabled).toBe(false);
    r.api.pushToStartToken("ab12");
    await r.list(STUDIO_HOST, [info(A, { phase: "running" })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.shown()).toEqual([]);
    expect(r.socketOf(STUDIO_HOST).commandsOf("register_activity")).toEqual([]);
  });
});
