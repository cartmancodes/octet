// The Live Activity while the app runs: what it shows (the hub's own rules, on the same scenarios as
// crates/octet-hub/src/activity.rs), how often it changes, the tokens each Mac is given, what the
// widget's App Group holds, Deny, and the switch. The native module is a fake here: the wrapper's
// own fallback is tested in modules/octet-live.
import AsyncStorage from "@react-native-async-storage/async-storage";
import * as SecureStore from "expo-secure-store";
import { readFileSync } from "node:fs";
import path from "node:path";

import type { ActivityState, WidgetEntry } from "../modules/octet-live";
import { DRIVE_GAP_MS, LiveActivity, REGISTER_RETRY_MS } from "./activity";
import {
  COMMAND_CHARS,
  ENTRIES_BYTES,
  ENTRIES_LIMIT,
  IDLE_END_MS,
  NAME_CHARS,
  ActivityModel,
  widgetEntries,
  type ActivityRow,
} from "./activityModel";
import { FakeClock, Sockets, channelOf, pairingOf, phoneUrlOf, type FakeSocket } from "./client/fakes";
import { FakeLive } from "./liveRig";
import { Fleet, HAND_OVER_MS, tokenKeyOf } from "./hub";
import { keyOf } from "./macs";
import { loadSettings, saveSettings } from "./persist";
import type { SessionInfo } from "./protocol";
import { createOctetStore, type ListedSession } from "./store";

jest.mock("expo-constants", () => ({ __esModule: true, default: { expoConfig: { extra: {} } } }));

const TOKEN = "cd".repeat(32);
const STUDIO = "m00000001";
const MINI = "m00000002";
/** The two Macs, by the names their hubs go by, both reached through the one relay; outside the
 * app (the content state, the App Group, a Deny) each is named by its channel id. */
const STUDIO_NAME = "studio";
const MINI_NAME = "mini";
const RELAY = "relay.example.test";
const STUDIO_CHANNEL = channelOf(STUDIO_NAME);
const MINI_CHANNEL = channelOf(MINI_NAME);
/** The address of the phone sockets to the Mac `name`. */
const urlOf = (name: string) => phoneUrlOf(name, RELAY);
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
async function rig({ macs = [STUDIO_NAME, MINI_NAME], supported = true }: { macs?: string[]; supported?: boolean } = {}) {
  const ids = [STUDIO, MINI];
  for (const i of macs.keys()) await SecureStore.setItemAsync(tokenKeyOf(ids[i] ?? ""), TOKEN);
  await saveSettings({
    deviceName: "iPhone",
    pairings: macs.map((name, i) => pairingOf(ids[i] ?? "", name, RELAY, { macName: name })),
  });
  const sockets = new Sockets();
  const clock = new FakeClock();
  const store = createOctetStore();
  const fleet = new Fleet({
    factory: () => sockets.factory,
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

  // The hub's own scenarios, from the fixture its test the_scenarios_fixture_is_the_rulings holds
  // to its rules: one file, so the two can't drift.
  type Step = {
    at: number;
    rows?: { id: string; phase: string | null; approvals: number[] }[];
    expect?: { kind?: string; session?: string; since?: number; approvals?: number };
  };
  const fixture = JSON.parse(
    readFileSync(path.resolve(__dirname, "..", "..", "..", "crates", "octet-hub", "tests", "data", "activity-scenarios.json"), "utf8"),
  ) as { scenarios: { name: string; sessions: string[]; steps: Step[] }[] };

  it("runs the hub's scenarios, from the shared fixture", () => {
    let checked = 0;
    for (const scenario of fixture.scenarios) {
      const model = new ActivityModel();
      model.observe(scenario.sessions.map(idle), at(0));
      for (const step of scenario.steps) {
        // A session without state is idle with nothing open on the phone's list.
        if (step.rows) model.observe(step.rows.map((r) => (r.phase === null ? idle(r.id) : snap(r.id, r.phase, r.approvals))), at(step.at));
        if (!step.expect) continue;
        const now = model.content(at(step.at));
        const { kind, session, since, approvals } = step.expect;
        const where = `${scenario.name} at ${step.at}`;
        if (kind !== undefined) expect([where, now.kind]).toEqual([where, kind]);
        if (session !== undefined) expect([where, now.key]).toEqual([where, session]);
        if (since !== undefined) expect([where, now.since]).toEqual([where, at(since)]);
        if (approvals !== undefined) expect([where, now.approvals]).toEqual([where, approvals]);
        checked += 1;
      }
    }
    expect(checked).toBe(7);
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
  it("shows the lead with the hub's raw id and its Mac's channel id, as the hub's content state does", async () => {
    const r = await rig();
    await r.list(MINI_NAME, [info(B, { phase: "running" })]);
    await r.list(STUDIO_NAME, [info(A, { approvals: 2, approval: approval(4) })]);
    await r.advance(DRIVE_GAP_MS);
    const last = r.api.shown().at(-1);
    expect(last?.arg).toEqual({ kind: "approval", session: A, mac: STUDIO_CHANNEL, since: expect.any(Number), approvals: 2 });
    expect(Number.isInteger((last?.arg as ActivityState).since)).toBe(true);
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("two_macs_with_the_same_session_id_stay_apart: an activity update", async () => {
    // Both Macs, on the one relay, list session A: the content state names whose it is.
    const r = await rig();
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    await r.list(MINI_NAME, [info(A, { approvals: 1, approval: approval(4) })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.shown().at(-1)?.arg).toEqual(expect.objectContaining({ kind: "approval", session: A, mac: MINI_CHANNEL }));
    await r.list(MINI_NAME, [info(A, { phase: "running" })]);
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(5) })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.shown().at(-1)?.arg).toEqual(expect.objectContaining({ kind: "approval", session: A, mac: STUDIO_CHANNEL }));
  });

  it("updates at most once a second, and the last one shows what is latest", async () => {
    const r = await rig();
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    expect(r.api.shown()).toHaveLength(1);
    for (let n = 1; n <= 5; n++) {
      await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: n, approval: approval(1) })]);
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
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    await r.list(STUDIO_NAME, [info(A, { phase: "idle" })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.shown().at(-1)?.arg).toMatchObject({ kind: "done", session: A });
    expect(r.api.of("end")).toHaveLength(0);
    await r.advance(IDLE_END_MS);
    expect(r.api.of("end")).toHaveLength(1);
  });

  it("a start that fails (Live Activities off) is caught, and not tried again until something changes", async () => {
    const r = await rig();
    r.api.failStart = true;
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    await r.advance(10 * DRIVE_GAP_MS);
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("does nothing in the background, and shows the state again in the foreground", async () => {
    const r = await rig();
    r.live.setForeground(false);
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.shown()).toHaveLength(0);
    r.live.setForeground(true);
    await settle();
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("does nothing at all where Live Activities are not supported", async () => {
    const r = await rig({ supported: false });
    await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: 1, approval: approval(1) })]);
    r.api.pushToStartToken("ab12");
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.calls).toEqual([]);
    expect(r.api.listening()).toBe(0);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([]);
  });
});

describe("registration: one Mac drives the Island", () => {
  it("only the owner (the first paired Mac, with no lead yet) is given the tokens, once each; every other Mac is told to forget them", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    r.api.pushToStartToken("ab12"); // ActivityKit may say the first one twice
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([expect.not.objectContaining({ token: expect.anything() })]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")[0]).toMatchObject({ start_token: "ab12" });
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toEqual([]);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toEqual([]);
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(1);
    await r.ack(STUDIO_NAME, "register_activity");
    await r.ack(MINI_NAME, "clear_activity");
    r.api.pushToStartToken("ab12");
    // An activity's token joins it; each registration states both.
    r.api.pushToken("cd34");
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").map(({ token, start_token }) => ({ token, start_token }))).toEqual([
      { token: undefined, start_token: "ab12" },
      { token: "cd34", start_token: "ab12" },
    ]);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toEqual([]);
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(1);
  });

  it("a lead on another Mac moves the tokens there, and the old owner forgets them", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    r.api.pushToken("cd34", "a");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await r.ack(MINI_NAME, "clear_activity");
    await r.list(MINI_NAME, [info(B, { phase: "running" })]);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toEqual([expect.objectContaining({ token: "cd34", start_token: "ab12" })]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(1);
    expect((await loadSettings()).liveActivityMac).toBe(MINI);
    await r.ack(MINI_NAME, "register_activity");
    // And back: an approval on studio leads.
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(2) })]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).toMatchObject({ token: "cd34", start_token: "ab12" });
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(2);
  });

  it("the owner doesn't change in the background", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    await settle();
    r.live.setForeground(false);
    await r.list(MINI_NAME, [info(B, { phase: "running" })]);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toEqual([]);
    expect((await loadSettings()).liveActivityMac).not.toBe(MINI);
    r.live.setForeground(true);
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(1);
  });

  it("the owner is kept across launches, until a lead says otherwise", async () => {
    await saveSettings({ liveActivityMac: MINI });
    const r = await rig();
    r.api.pushToStartToken("ab12");
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(1);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(1);
  });

  it("a token that changes while the owner is out of reach goes to it on its next connection", async () => {
    const r = await rig();
    r.socketOf(STUDIO_NAME).drop();
    await settle();
    r.api.pushToStartToken("ab12");
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(0);
    await r.advance(60_000); // the link tries again
    await r.ready(STUDIO_NAME);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([expect.objectContaining({ start_token: "ab12" })]);
  });

  it("an old owner out of reach when the tokens moved is told to forget them on the next foreground", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    r.api.pushToken("cd34", "a");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await r.ack(MINI_NAME, "clear_activity");
    // Studio drops; then a lead on mini moves the tokens there while studio can't be told.
    r.socketOf(STUDIO_NAME).drop();
    await settle();
    await r.list(MINI_NAME, [info(B, { phase: "running" })]);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(1);
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toEqual([]);
    // The app leaves before studio is back, and comes to the front again.
    r.live.setForeground(false);
    r.fleet.setForeground(false);
    await settle();
    r.fleet.setForeground(true);
    r.live.setForeground(true);
    await settle();
    await r.ready(STUDIO_NAME);
    await r.ready(MINI_NAME);
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(1);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([]);
    // Refused (the hub was busy): it is told again at the next foreground, the link up all along.
    await r.list(MINI_NAME, [info(B, { phase: "running" })]);
    for (const c of r.socketOf(STUDIO_NAME).commandsOf("clear_activity")) {
      r.socketOf(STUDIO_NAME).receive({ type: "refused", req: c.req, code: "not_allowed", reason: "no" });
    }
    await settle();
    r.live.setForeground(false);
    r.live.setForeground(true);
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(2);
  });

  it(`a register or clear the hub refuses for now is tried once more, ${REGISTER_RETRY_MS / 1000} s on`, async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    await settle();
    const refuse = async (host: string, type: string, code: string) => {
      const c = r.socketOf(host).commandsOf(type).at(-1);
      r.socketOf(host).receive({ type: "refused", req: c?.req, code, reason: "Too many commands" });
      await settle();
    };
    await refuse(STUDIO_NAME, "register_activity", "rate_limited");
    await refuse(MINI_NAME, "clear_activity", "unavailable");
    await r.advance(REGISTER_RETRY_MS - 1);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(1);
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(1);
    await r.advance(1);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(2);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")[1]).toMatchObject({ start_token: "ab12" });
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(2);
    // Refused again: once is all, until something changes or the link comes back.
    await refuse(STUDIO_NAME, "register_activity", "rate_limited");
    await r.advance(2 * REGISTER_RETRY_MS); // short of the 25 s ping: the link stays the same
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(2);
    // A definite no is not tried again by itself.
    r.api.pushToken("cd34");
    await settle();
    await refuse(STUDIO_NAME, "register_activity", "invalid");
    await r.advance(2 * REGISTER_RETRY_MS); // short of the 25 s ping: the link stays the same
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(3);
    // A retry that would say what is no longer wanted is not sent: the newest body goes instead.
    r.api.pushToken("ef56");
    await settle();
    await refuse(STUDIO_NAME, "register_activity", "busy");
    r.api.pushToken("0a0b");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await r.advance(REGISTER_RETRY_MS);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").map((c) => c.token)).toEqual([undefined, undefined, "cd34", "ef56", "0a0b"]);
  });

  it("an ended activity's token is taken back: only the push-to-start token is left", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.api.pushToStartToken("ab12");
    r.api.pushToken("cd34");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    r.api.pushToken(null);
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).toMatchObject({ start_token: "ab12" });
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).not.toHaveProperty("token");
  });
});

describe("the widget's App Group entries", () => {
  const entries = (r: Awaited<ReturnType<typeof rig>>) => r.api.of("setEntries").at(-1)?.arg as Record<string, WidgetEntry> | undefined;

  it("names each waiting approval's session by channel id and raw id, cleaned, and the activity is shown again after each write", async () => {
    const r = await rig();
    const command = `echo start\n${"\n".repeat(12)}rm -rf / \u001b[31m${"x".repeat(400)}`;
    await r.list(STUDIO_NAME, [
      info(A, { workspace: `/w/${"n".repeat(120)}`, approvals: 1, approval: approval(3, command), last_activity: 10 }),
      info("303-1790000000", { phase: "running" }),
    ]);
    await r.list(MINI_NAME, [info(B, { approvals: 1, approval: approval(7, null, "Edit files"), last_activity: 20 })]);
    const got = entries(r);
    expect(Object.keys(got ?? {})).toEqual([`${MINI_CHANNEL}/${B}`, `${STUDIO_CHANNEL}/${A}`]);
    const studio = got?.[`${STUDIO_CHANNEL}/${A}`];
    expect(studio?.approval).toBe("3");
    expect(studio?.name.length).toBeLessThanOrEqual(NAME_CHARS);
    expect(studio?.name).toBe("n".repeat(NAME_CHARS));
    expect(studio?.command.length).toBeLessThanOrEqual(COMMAND_CHARS);
    expect(studio?.command).toContain("⋯ 12 blank lines ⋯");
    expect(studio?.command).not.toContain("\u001b");
    expect(got?.[`${MINI_CHANNEL}/${B}`]).toEqual({ name: B, command: "Edit files", approval: "7" });
    // Every write is followed by the activity being shown again, so the widget reads it anew.
    const writes = r.api.calls.map((c) => c.what);
    const lastWrite = writes.lastIndexOf("setEntries");
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.calls.slice(lastWrite + 1).some((c) => c.what === "update" || c.what === "start")).toBe(true);
  });

  it("keeps at most 64 entries, newest first, within 64 KiB", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    const many = Array.from({ length: 70 }, (_, i) =>
      info(`${1000 + i}-1790000000`, { approvals: 1, approval: approval(1, "😀".repeat(300)), workspace: `/w/${"é".repeat(90)}`, last_activity: i }),
    );
    await r.list(STUDIO_NAME, many);
    const got = entries(r) ?? {};
    const keys = Object.keys(got);
    expect(keys.length).toBeLessThanOrEqual(ENTRIES_LIMIT);
    expect(keys[0]).toBe(`${STUDIO_CHANNEL}/1069-1790000000`);
    expect(Buffer.byteLength(JSON.stringify(got), "utf8")).toBeLessThanOrEqual(ENTRIES_BYTES);
    for (const entry of Object.values(got)) for (const value of Object.values(entry)) expect(typeof value).toBe("string");
  });

  it("is rewritten when the approvals change, and an unpaired Mac's entries leave it", async () => {
    const r = await rig();
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(1) })]);
    await r.list(MINI_NAME, [info(B, { approvals: 1, approval: approval(2) })]);
    expect(Object.keys(entries(r) ?? {}).sort()).toEqual([`${MINI_CHANNEL}/${B}`, `${STUDIO_CHANNEL}/${A}`]);
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(5) })]);
    expect(entries(r)?.[`${STUDIO_CHANNEL}/${A}`]?.approval).toBe("5");
    await r.fleet.unpair(STUDIO);
    await settle();
    expect(Object.keys(entries(r) ?? {})).toEqual([`${MINI_CHANNEL}/${B}`]);
    await r.fleet.unpair(MINI);
    await settle();
    expect(entries(r)).toEqual({});
  });

  it("widgetEntries skips a session whose Mac has no channel id", () => {
    const row = { ...info(A, { approvals: 1, approval: approval(1) }), id: keyOf(STUDIO, A), mac: STUDIO, raw: A } as ListedSession;
    expect(widgetEntries([row], () => null)).toEqual({});
    expect(Object.keys(widgetEntries([row], () => STUDIO_CHANNEL))).toEqual([`${STUDIO_CHANNEL}/${A}`]);
  });
});

describe("Deny from the Island", () => {
  it("deny_goes_to_the_mac_whose_channel_the_activity_named", async () => {
    // Studio is the first Mac paired, the one a Deny went to while both shared the relay's host.
    const r = await rig();
    await r.list(MINI_NAME, [info(B, { approvals: 1, approval: approval(3) })]);
    r.api.deny({ mac: MINI_CHANNEL, session: B, approval: "3" });
    r.api.deny({ mac: MINI_CHANNEL, session: B, approval: "3" });
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toEqual([expect.objectContaining({ session: B, approval: 3, allow: false })]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toEqual([]);
  });

  it("two_macs_with_the_same_session_id_stay_apart: Deny", async () => {
    const r = await rig();
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(3) })]);
    await r.list(MINI_NAME, [info(A, { approvals: 1, approval: approval(3) })]);
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "3" });
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 3, allow: false })]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toEqual([]);
    // The same session and approval on the other Mac is its own Deny, not a repeat.
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "3" });
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 3, allow: false })]);
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toHaveLength(1);
  });

  it("sends nothing for an unknown channel, none, a hostname, a bad session or a bad approval id", async () => {
    const r = await rig();
    for (const event of [
      { mac: channelOf("evil"), session: A, approval: "3" },
      { mac: "", session: A, approval: "3" },
      { mac: RELAY, session: A, approval: "3" },
      { mac: STUDIO_CHANNEL, session: "../settings", approval: "3" },
      { mac: STUDIO_CHANNEL, session: A, approval: "3x" },
      { mac: STUDIO_CHANNEL, session: A, approval: "-1" },
      { mac: STUDIO_CHANNEL, session: A, approval: "99999999999999999999" },
    ]) {
      r.api.deny(event);
    }
    await settle();
    for (const host of [STUDIO_NAME, MINI_NAME]) expect(r.socketOf(host).commandsOf("answer")).toEqual([]);
  });

  it("an_activity_left_from_a_hostname_pairing_names_no_mac", async () => {
    // An activity from before the relay named its Mac by the host it was published at, or by none:
    // its Deny reaches no Mac, not even the only one paired, whose session does wait.
    const r = await rig({ macs: [STUDIO_NAME] });
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(4) })]);
    r.api.deny({ mac: "studio.tail1234.ts.net", session: A, approval: "4" });
    r.api.deny({ mac: "", session: A, approval: "4" });
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toEqual([]);
  });

  it("a Deny handed over before the pairings are read is answered once they are", async () => {
    await SecureStore.setItemAsync(tokenKeyOf(STUDIO), TOKEN);
    await saveSettings({ pairings: [pairingOf(STUDIO, STUDIO_NAME, RELAY, { macName: "studio" })] });
    const sockets = new Sockets();
    const clock = new FakeClock();
    const store = createOctetStore();
    const fleet = new Fleet({ factory: () => sockets.factory, store, platform: "ios", clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 } });
    const api = new FakeLive(clock.now);
    const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
    live.start();
    live.setForeground(true); // the app is opened
    api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "6" }); // a tap made while the app was closed
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

  it("a Deny in the background goes at once, over a background connection to its Mac: hello background, the answer, closed", async () => {
    const r = await rig();
    r.fleet.setForeground(false);
    r.live.setForeground(false);
    await settle();
    const before = r.sockets.all.length;
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "5" });
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "5" }); // once
    await settle();
    const opened = r.sockets.all.slice(before);
    expect(opened.map((s) => s.url)).toEqual([urlOf(MINI_NAME)]);
    const [mini] = opened;
    mini?.open();
    mini?.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(mini?.commands().map((c) => c.type)).toEqual(["hello", "answer"]);
    expect(mini?.commandsOf("hello")[0]).toMatchObject({ background: true });
    expect(mini?.commandsOf("answer")[0]).toMatchObject({ session: A, approval: 5, allow: false });
    mini?.receive({ type: "ack", req: mini.commandsOf("answer")[0]?.req, message: "answered" });
    await settle();
    expect(mini?.closed).toBe(true);
    // Answered: nothing is held for the foreground.
    r.fleet.setForeground(true);
    r.live.setForeground(true);
    await settle();
    await r.ready(MINI_NAME);
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toEqual([]);
  });

  it("a background Deny the Mac doesn't answer within 10 s is held, and sent over the link back in the foreground", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.fleet.setForeground(false);
    r.live.setForeground(false);
    await settle();
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "5" });
    await settle();
    await r.advance(HAND_OVER_MS);
    expect(r.sockets.all.at(-1)?.closed).toBe(true);
    r.fleet.setForeground(true);
    r.live.setForeground(true);
    await settle();
    await r.ready(STUDIO_NAME);
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 5, allow: false })]);
  });

  it("a Deny while the Mac is out of reach goes on its next connection", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.socketOf(STUDIO_NAME).drop();
    await settle();
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "4" });
    await settle();
    await r.advance(60_000);
    await r.ready(STUDIO_NAME);
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toEqual([expect.objectContaining({ session: A, approval: 4, allow: false })]);
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
    await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: 1, approval: approval(1) })]);
    await r.ack(STUDIO_NAME, "register_activity");
    await r.ack(MINI_NAME, "clear_activity");
    expect(await r.live.setEnabled(false)).toBe(true);
    await settle();
    expect((await loadSettings()).liveActivity).toBe(false);
    expect(r.api.of("end")).toHaveLength(1);
    expect(r.api.of("setEntries").at(-1)?.arg).toEqual({});
    // Every Mac is told again, the owner and the others.
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(1);
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(2);
    const before = r.api.calls.length;
    r.api.pushToken("ef56"); // an activity that lands while it is off is ended, not adopted
    await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: 2, approval: approval(1) })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.calls.slice(before).map((c) => c.what)).toEqual(["end"]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(1);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(0);

    expect(await r.live.setEnabled(true)).toBe(true);
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).toMatchObject({ token: "ef56", start_token: "ab12" });
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(0);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start").length + r.api.of("update").length).toBeGreaterThan(0);
  });

  it("off clears each Mac once, though ending the activity says its token is gone", async () => {
    const r = await rig();
    r.api.pushToken("ef56", "act-1");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await r.ack(MINI_NAME, "clear_activity");
    // As on the phone: the end makes ActivityKit report the activity's token gone, at once.
    r.api.end = async () => {
      r.api.calls.push({ what: "end", at: r.clock.now() });
      r.api.pushToken(null, "act-1");
    };
    const studio = r.socketOf(STUDIO_NAME).commandsOf("clear_activity").length;
    const mini = r.socketOf(MINI_NAME).commandsOf("clear_activity").length;
    expect(await r.live.setEnabled(false)).toBe(true);
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(studio + 1);
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(mini + 1);
    expect(r.api.of("end")).toHaveLength(1);
  });

  it("off while a start is on its way: that activity is ended too", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    let finish = () => {};
    r.api.start = async (state) => {
      r.api.calls.push({ what: "start", at: r.clock.now(), arg: state });
      await new Promise<void>((resolve) => (finish = resolve));
    };
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    expect(r.api.of("start")).toHaveLength(1);
    await r.live.setEnabled(false);
    expect(r.api.of("end")).toHaveLength(1);
    finish();
    await settle();
    expect(r.api.of("end")).toHaveLength(2);
  });

  it("an activity the hub started that lands after the switch went off is ended", async () => {
    await saveSettings({ liveActivity: false });
    const r = await rig({ macs: [STUDIO_NAME] });
    r.api.pushToken("cd34", "hub-started");
    await settle();
    expect(r.api.of("end")).toHaveLength(1);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([]);
  });

  it("one that lands before the setting is read, with the switch off, is ended once it is read", async () => {
    await saveSettings({ liveActivity: false });
    const sockets = new Sockets();
    const clock = new FakeClock();
    const store = createOctetStore();
    const fleet = new Fleet({ factory: () => sockets.factory, store, platform: "ios", clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 } });
    const api = new FakeLive(clock.now);
    const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
    live.start();
    api.pushToken("cd34", "hub-started");
    expect(api.of("end")).toHaveLength(0);
    await settle();
    expect(api.of("end")).toHaveLength(1);
    live.stop();
  });

  it("off from the start: nothing is shown or registered", async () => {
    await saveSettings({ liveActivity: false });
    const r = await rig();
    expect(r.live.enabled).toBe(false);
    r.api.pushToStartToken("ab12");
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.shown()).toEqual([]);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([]);
  });
});

describe("the activity that is already there (one activity, two drivers)", () => {
  it("adopts a live activity while every session is idle: it is updated, then ended 5 minutes on", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await r.list(STUDIO_NAME, [info(A)]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.shown()).toEqual([]); // nothing runs or waits: the app starts nothing
    r.api.pushToken("cd34", "hub-started"); // the hub started one while the phone was away
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start")).toEqual([]);
    expect(r.api.of("update")).toEqual([expect.objectContaining({ arg: expect.objectContaining({ kind: "done" }) })]);
    expect(r.api.of("end")).toEqual([]);
    await r.advance(IDLE_END_MS);
    expect(r.api.of("end")).toHaveLength(1);
  });

  it("an adopted activity is updated, not started again, when a turn starts", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.api.pushToken("cd34", "hub-started");
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start")).toEqual([]);
    expect(r.api.of("update").at(-1)?.arg).toMatchObject({ kind: "running", session: A });
  });

  it("keeps each activity's token apart: an old activity's end leaves the live one's token registered", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.api.pushToStartToken("ab12");
    r.api.pushToken("cd34", "old");
    r.api.pushToken("ef56", "new");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    const before = r.socketOf(STUDIO_NAME).commandsOf("register_activity").length;
    r.api.pushToken(null, "old");
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(before);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).toMatchObject({ token: "ef56", start_token: "ab12" });
    // A null that names no activity is ignored while one that is named lives.
    r.api.pushToken(null);
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(before);
    // The live one ends: only the push-to-start token is left.
    r.api.pushToken(null, "new");
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).toEqual(expect.not.objectContaining({ token: expect.anything() }));
  });

  it("a null for an activity it never saw is ignored: the activity is still live, nothing counts as dismissed", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    expect(r.api.of("start")).toHaveLength(1);
    r.api.pushToken(null, "never-seen"); // one that had already ended when it was looked at
    await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: 1, approval: approval(1) })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start")).toHaveLength(1);
    expect(r.api.of("update").at(-1)?.arg).toMatchObject({ kind: "approval" });
    r.live.setForeground(false);
    r.live.setForeground(true);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start")).toHaveLength(1);
  });

  it("an activity the user dismissed is not started again by coming back to the app, only by a change", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    expect(r.api.of("start")).toHaveLength(1);
    r.api.pushToken("cd34", "mine");
    await settle();
    r.api.pushToken(null, "mine"); // dismissed from the Lock Screen
    await r.advance(DRIVE_GAP_MS);
    r.live.setForeground(false);
    r.live.setForeground(true);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.api.of("start")).toHaveLength(1);
    await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: 1, approval: approval(1) })]);
    await r.advance(DRIVE_GAP_MS);
    expect(r.api.of("start")).toHaveLength(2);
  });
});

describe("the app is kept running until a background send settles", () => {
  const background = async (r: Awaited<ReturnType<typeof rig>>) => {
    r.fleet.setForeground(false);
    r.live.setForeground(false);
    await settle();
    // Leaving with nothing on its way settles once (any hold from a token as it left); the
    // hand-overs below are counted from here.
    expect(r.api.handOversSettled).toBe(1);
    r.api.handOversSettled = 0;
  };
  const ID = "6F9619FF-8B86-D011-B42D-00C04FC964FF";
  const id = (n: number) => ID.slice(0, -2) + String(n).padStart(2, "0");
  /** The newest background connection: it says welcome, and answers its one request with `reply`. */
  const answer = async (r: Awaited<ReturnType<typeof rig>>, type: string, reply: (req: unknown) => object) => {
    const socket = r.sockets.all.at(-1);
    socket?.open();
    socket?.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(r.api.settledDenies.length + r.api.handOversSettled).toBe(0); // not before the answer
    const req = socket?.commandsOf(type)[0]?.req;
    socket?.receive(reply(req));
    await settle();
  };

  it.each([
    ["acked", (req: unknown) => ({ type: "ack", req, message: "answered" })],
    ["refused", (req: unknown) => ({ type: "refused", req, code: "not_allowed", reason: "No such approval" })],
  ])("a background Deny the Mac %s is settled once", async (_what, reply) => {
    const r = await rig();
    await background(r);
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "5", id: id(1) });
    await settle();
    await answer(r, "answer", reply);
    expect(r.api.settledDenies).toEqual([id(1)]);
    await r.advance(HAND_OVER_MS);
    expect(r.api.settledDenies).toEqual([id(1)]);
  });

  it("a background Deny that is lost (no answer in 10 s) is settled once, and not again when it goes later", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await background(r);
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "5", id: id(2) });
    await settle();
    expect(r.api.settledDenies).toEqual([]);
    await r.advance(HAND_OVER_MS);
    expect(r.api.settledDenies).toEqual([id(2)]);
    r.fleet.setForeground(true);
    r.live.setForeground(true);
    await settle();
    await r.ready(STUDIO_NAME);
    expect(r.socketOf(STUDIO_NAME).commandsOf("answer")).toHaveLength(1);
    expect(r.api.settledDenies).toEqual([id(2)]);
  });

  it("a Deny that sends nothing, or goes over the link, or waits for it, is settled once", async () => {
    const r = await rig();
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(3) }), info(B)]);
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "3", id: id(3) }); // over the link
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "3", id: id(4) }); // the same again
    r.api.deny({ mac: "evil.example.com", session: A, approval: "3", id: id(5) }); // no such Mac
    r.api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "3x", id: id(6) }); // no approval id
    r.api.deny({ mac: STUDIO_CHANNEL, session: B, approval: "7", id: id(7) }); // nothing waits there
    await settle();
    expect(r.api.settledDenies.sort()).toEqual([id(4), id(5), id(6), id(7)].sort());
    const [sent] = r.socketOf(STUDIO_NAME).commandsOf("answer");
    r.socketOf(STUDIO_NAME).receive({ type: "ack", req: sent?.req, message: "answered" });
    await settle();
    expect(r.api.settledDenies.filter((d) => d === id(3))).toHaveLength(1);
    // Out of reach in front: held for the link, and settled now.
    r.socketOf(MINI_NAME).drop();
    await settle();
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "4", id: id(8) });
    await settle();
    expect(r.api.settledDenies.filter((d) => d === id(8))).toHaveLength(1);
    await r.advance(60_000);
    await r.ready(MINI_NAME);
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toHaveLength(1);
    expect(r.api.settledDenies).toHaveLength(6);
  });

  it("a Deny handed over before the pairings are read is settled once it is sent", async () => {
    await SecureStore.setItemAsync(tokenKeyOf(STUDIO), TOKEN);
    await saveSettings({ pairings: [pairingOf(STUDIO, STUDIO_NAME, RELAY, { macName: "studio" })] });
    const sockets = new Sockets();
    const clock = new FakeClock();
    const store = createOctetStore();
    const fleet = new Fleet({ factory: () => sockets.factory, store, platform: "ios", clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 } });
    const api = new FakeLive(clock.now);
    const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
    live.start();
    fleet.setForeground(false);
    live.setForeground(false);
    api.deny({ mac: STUDIO_CHANNEL, session: A, approval: "6", id: id(9) });
    expect(api.settledDenies).toEqual([]);
    await fleet.init();
    await settle();
    const socket = sockets.last;
    socket.open();
    socket.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(api.settledDenies).toEqual([]);
    socket.receive({ type: "ack", req: socket.commandsOf("answer")[0]?.req, message: "answered" });
    await settle();
    expect(api.settledDenies).toEqual([id(9)]);
    live.stop();
  });

  it.each([
    ["acked", (req: unknown) => ({ type: "ack", req, message: "Activity registered" })],
    ["refused", (req: unknown) => ({ type: "refused", req, code: "invalid", reason: "No" })],
  ])("a hand-over the owner %s is settled once", async (_what, reply) => {
    const r = await rig();
    await background(r);
    r.api.pushToken("cd34", "hub-started");
    r.api.pushToken("cd34", "hub-started"); // said twice: one hand-over
    await settle();
    await answer(r, "register_activity", reply);
    expect(r.api.handOversSettled).toBe(1);
    await r.advance(HAND_OVER_MS);
    expect(r.api.handOversSettled).toBe(1);
  });

  it("a hand-over that is lost (no answer in 10 s) is settled once", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await background(r);
    r.api.pushToken("cd34", "a");
    await settle();
    expect(r.api.handOversSettled).toBe(0);
    await r.advance(HAND_OVER_MS);
    expect(r.api.handOversSettled).toBe(1);
  });

  it("a token in the background with nothing to hand over is settled at once", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.api.pushToken("cd34", "a");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await background(r);
    r.api.pushToken("cd34", "a"); // the owner has it already
    await settle();
    expect(r.api.handOversSettled).toBe(1);
  });

  it("a token that came as the app was leaving, sent over the link, is settled when the app is in the background", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    // The native side holds background time for it (the app was leaving); JS still thought it was
    // in front, and sent it over the link.
    r.api.pushToken("cd34", "a");
    await settle();
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(1);
    expect(r.api.handOversSettled).toBe(0);
    r.live.setForeground(false);
    expect(r.api.handOversSettled).toBe(1);
  });

  it("leaving the front settles nothing while a hand-over is on its way; its end does", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await background(r);
    const left = r.api.handOversSettled; // 0: counted from the leave
    r.api.pushToken("cd34", "a");
    await settle();
    r.live.setForeground(true);
    r.live.setForeground(false); // back and away again, the hand-over still out
    expect(r.api.handOversSettled).toBe(left);
    await r.advance(HAND_OVER_MS);
    expect(r.api.handOversSettled).toBe(left + 1);
  });

  it("a cold launch in the background for a token is settled only after the hand-over it waited for", async () => {
    await SecureStore.setItemAsync(tokenKeyOf(STUDIO), TOKEN);
    await saveSettings({ deviceName: "iPhone", pairings: [pairingOf(STUDIO, STUDIO_NAME, RELAY, { macName: "studio" })] });
    const sockets = new Sockets();
    const clock = new FakeClock();
    const store = createOctetStore();
    const fleet = new Fleet({ factory: () => sockets.factory, store, platform: "ios", clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 } });
    const api = new FakeLive(clock.now);
    const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
    fleet.setForeground(false);
    live.start();
    live.setForeground(false);
    api.pushToken("cd34", "hub-started"); // before the settings or the pairings are read
    expect(api.handOversSettled).toBe(0);
    await fleet.init();
    await settle();
    expect(api.handOversSettled).toBe(0);
    const [socket] = sockets.all;
    socket?.open();
    socket?.receive({ type: "welcome", device: "iPhone" });
    await settle();
    socket?.receive({ type: "ack", req: socket.commandsOf("register_activity")[0]?.req, message: "Activity registered" });
    await settle();
    expect(api.handOversSettled).toBe(1);
    live.stop();
  });
});

describe("the hand-over in the background", () => {
  /** The app goes to the background, as the shell does it: every link closes. */
  const background = async (r: Awaited<ReturnType<typeof rig>>) => {
    r.fleet.setForeground(false);
    r.live.setForeground(false);
    await settle();
  };

  it("an activity token that comes in the background goes to the owner alone, over a background connection that only registers, and closes", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await background(r);
    const before = r.sockets.all.length;
    r.api.pushToken("cd34", "hub-started");
    r.api.pushToken("cd34", "hub-started"); // said twice: one hand-over
    await settle();
    const opened = r.sockets.all.slice(before);
    expect(opened.map((s) => s.url)).toEqual([urlOf(STUDIO_NAME)]);
    const [studio] = opened;
    studio?.open();
    studio?.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(studio?.commands().map((c) => c.type)).toEqual(["hello", "register_activity"]);
    // The hub isn't to take it for the app in front.
    expect(studio?.commandsOf("hello")[0]).toMatchObject({ background: true });
    expect(studio?.commandsOf("register_activity")[0]).toMatchObject({ token: "cd34", start_token: "ab12" });
    studio?.receive({ type: "ack", req: studio.commandsOf("register_activity")[0]?.req, message: "Activity registered" });
    await settle();
    expect(studio?.closed).toBe(true);
    expect(r.clock.pending).toBe(0);

    // Back in the foreground: studio has it already; mini is the other Mac and gets none.
    r.fleet.setForeground(true);
    r.live.setForeground(true);
    await settle();
    for (const host of [STUDIO_NAME, MINI_NAME]) await r.ready(host);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([]);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toEqual([]);
  });

  it("a hand-over the owner never answers is closed at the end of the window, and told over the link back in the foreground", async () => {
    const r = await rig();
    await background(r);
    const before = r.sockets.all.length;
    r.api.pushToken("cd34", "a");
    await settle();
    const [studio] = r.sockets.all.slice(before);
    studio?.open();
    studio?.receive({ type: "welcome", device: "iPhone" });
    await settle();
    await r.advance(HAND_OVER_MS);
    expect(studio?.closed).toBe(true);
    r.fleet.setForeground(true);
    r.live.setForeground(true);
    await settle();
    await r.ready(STUDIO_NAME);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toEqual([expect.objectContaining({ token: "cd34" })]);
  });

  it("a cold launch in the background for the token: handed over to the owner once the settings and pairings are read, once", async () => {
    for (const id of [STUDIO, MINI]) await SecureStore.setItemAsync(tokenKeyOf(id), TOKEN);
    await saveSettings({
      deviceName: "iPhone",
      pairings: [
        pairingOf(STUDIO, STUDIO_NAME, RELAY, { macName: "studio" }),
        pairingOf(MINI, MINI_NAME, RELAY, { macName: "mini" }),
      ],
    });
    const sockets = new Sockets();
    const clock = new FakeClock();
    const store = createOctetStore();
    const fleet = new Fleet({ factory: () => sockets.factory, store, platform: "ios", clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 } });
    const api = new FakeLive(clock.now);
    const live = new LiveActivity({ api, fleet, store, now: clock.now, schedule: clock.schedule });
    // iOS launches the app in the background: the shell starts, nothing is in the foreground.
    fleet.setForeground(false);
    live.start();
    live.setForeground(false);
    api.pushToken("cd34", "hub-started"); // before the settings or the pairings are read
    api.pushToken("cd34", "hub-started");
    expect(sockets.all).toEqual([]);
    await fleet.init();
    await settle();
    // To the owner alone: the first paired Mac, with none saved.
    expect(sockets.all.map((s) => s.url)).toEqual([urlOf(STUDIO_NAME)]);
    for (const socket of sockets.all) {
      socket.open();
      socket.receive({ type: "welcome", device: "iPhone" });
    }
    await settle();
    for (const socket of sockets.all) {
      expect(socket.commands().map((c) => c.type)).toEqual(["hello", "register_activity"]);
      socket.receive({ type: "ack", req: socket.commandsOf("register_activity")[0]?.req, message: "Activity registered" });
    }
    await settle();
    expect(sockets.all.every((s) => s.closed)).toBe(true);
    // Anything else the Fleet says meanwhile hands nothing over again.
    await fleet.rename(STUDIO, "Studio");
    await settle();
    expect(sockets.all).toHaveLength(1);
    live.stop();
  });

  it("a hand-over that fails is not tried again in the background for the same token", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    await background(r);
    const before = r.sockets.all.length;
    r.api.pushToken("cd34", "a");
    await settle();
    await r.advance(HAND_OVER_MS); // the Mac never answers
    await r.fleet.rename(STUDIO, "Studio");
    await settle();
    expect(r.sockets.all.length).toBe(before + 1);
  });

  it("unpairing a Mac stops a hand-over to it that is under way: no register_activity can follow the goodbye", async () => {
    const r = await rig();
    await background(r);
    const before = r.sockets.all.length;
    r.api.pushToken("cd34", "a");
    await settle();
    const studio = r.sockets.all.slice(before).find((s) => s.url === urlOf(STUDIO_NAME));
    await r.fleet.unpair(STUDIO);
    await settle();
    expect(studio?.closed).toBe(true);
    studio?.open();
    studio?.receive({ type: "welcome", device: "iPhone" });
    await settle();
    expect(studio?.commandsOf("register_activity")).toEqual([]);
  });

  it("nothing is handed over with the switch off, or for a token that is gone", async () => {
    const r = await rig({ macs: [STUDIO_NAME] });
    r.api.pushToken("cd34", "a");
    await settle();
    await r.ack(STUDIO_NAME, "register_activity");
    await background(r);
    const before = r.sockets.all.length;
    r.api.pushToken(null, "a");
    await settle();
    expect(r.sockets.all.length).toBe(before);
    await r.live.setEnabled(false);
    r.api.pushToken("ef56", "b");
    await settle();
    expect(r.sockets.all.length).toBe(before);
  });
});

describe("Live Activities turned off or on in the phone's settings", () => {
  it("off: every Mac forgets the tokens and the App Group empties, at the next foreground; on again: registered again", async () => {
    const r = await rig();
    r.api.pushToStartToken("ab12");
    await r.list(STUDIO_NAME, [info(A, { approvals: 1, approval: approval(1) })]);
    await r.ack(STUDIO_NAME, "register_activity");
    await r.ack(MINI_NAME, "clear_activity");
    r.live.setForeground(false);
    r.api.supported = false;
    r.live.setForeground(true);
    await settle();
    expect(r.live.supported).toBe(false);
    expect(r.socketOf(STUDIO_NAME).commandsOf("clear_activity")).toHaveLength(1);
    expect(r.socketOf(MINI_NAME).commandsOf("clear_activity")).toHaveLength(2);
    expect(r.api.of("setEntries").at(-1)?.arg).toEqual({});
    r.api.pushToken("cd34", "x");
    await r.list(STUDIO_NAME, [info(A, { phase: "running", approvals: 2, approval: approval(1) })]);
    await r.advance(5 * DRIVE_GAP_MS);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity")).toHaveLength(1);
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(0);
    expect(r.api.shown().filter((c) => (c.arg as ActivityState).approvals === 2)).toEqual([]);

    r.live.setForeground(false);
    r.api.supported = true;
    r.live.setForeground(true);
    await settle();
    expect(r.live.supported).toBe(true);
    expect(r.socketOf(STUDIO_NAME).commandsOf("register_activity").at(-1)).toMatchObject({ token: "cd34", start_token: "ab12" });
    expect(r.socketOf(MINI_NAME).commandsOf("register_activity")).toHaveLength(0);
    expect(Object.keys((r.api.of("setEntries").at(-1)?.arg ?? {}) as object)).toEqual([`${STUDIO_CHANNEL}/${A}`]);
  });

  it("supported only from a later foreground: it starts following then", async () => {
    const r = await rig({ macs: [STUDIO_NAME], supported: false });
    expect(r.api.listening()).toBe(0);
    r.live.setForeground(false);
    r.api.supported = true;
    r.live.setForeground(true);
    await settle();
    expect(r.api.listening()).toBe(3);
    await r.list(STUDIO_NAME, [info(A, { phase: "running" })]);
    expect(r.api.of("start")).toHaveLength(1);
  });
});

describe("what the App Group and Deny take", () => {
  it("leaves out a row whose raw id is not a session id", () => {
    const row = (raw: string) => ({ ...info(raw, { approvals: 1, approval: approval(1) }), id: keyOf(STUDIO, raw), mac: STUDIO, raw }) as ListedSession;
    expect(Object.keys(widgetEntries([row(A), row("../x"), row("4821")], () => STUDIO_CHANNEL))).toEqual([`${STUDIO_CHANNEL}/${A}`]);
  });

  it("a Deny names its Mac by the channel id exactly: in another case it names none", async () => {
    const r = await rig();
    await r.list(MINI_NAME, [info(A, { approvals: 1, approval: approval(3) })]);
    r.api.deny({ mac: MINI_CHANNEL.toUpperCase(), session: A, approval: "3" });
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toEqual([]);
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "3" });
    r.api.deny({ mac: MINI_CHANNEL, session: A, approval: "3" });
    await settle();
    expect(r.socketOf(MINI_NAME).commandsOf("answer")).toHaveLength(1);
  });
});
