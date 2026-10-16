// The Island's Allow: `octet://approve/<host>/<raw session>/<approval>` opens that approval in its
// session, where Allow still asks for Face ID and waits out the arm. A forged, stale or unknown
// link opens the Inbox and sends nothing.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { router } from "expo-router";
import { renderRouter, screen } from "expo-router/testing-library";
import * as SecureStore from "expo-secure-store";
import { AppState } from "react-native";

import RootLayout from "../app/_layout";
import { redirectSystemPath } from "../app/+native-intent";
import Pair from "../app/pair";
import Sessions from "../app/index";
import Session from "../app/session/[id]";
import { FakeClock, Sockets } from "./client/fakes";
import { APPROVE_WAIT_MS, HubContext, Fleet, TOKEN_KEY } from "./hub";
import { handOverApproveLink, parseApproveLink, watchApproveLinks } from "./navigation";
import { saveSettings } from "./persist";
import type { SessionInfo } from "./protocol";
import { octetStore } from "./store";
import { k, newM } from "./testKeys";

jest.mock("expo-camera", () => ({
  CameraView: () => null,
  useCameraPermissions: () => [{ granted: false, canAskAgain: false, status: "denied" }, jest.fn()],
}));

const TOKEN = "cd".repeat(32);
const HOST = "mac.tail1234.ts.net";
const S = "4821-1790000000";
const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

const summary = { action: "Run tests", command: "make test", files: [], cwd: "/w", scope: "once" };
const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/Users/me/Projects/${id}`,
  mode: "ask",
  phase: "running",
  approvals: 0,
  from_phone: false,
  ...extra,
});
const waiting = (id = S, approval = 3) => info(id, { approvals: 1, approval: { id: approval, summary } });
const link = (host: string, raw: string, approval: string | number) => `octet://approve/${host}/${raw}/${approval}`;

describe("parseApproveLink", () => {
  it("takes a host, a raw session id and an approval id, each percent-decoded", () => {
    expect(parseApproveLink(link(HOST, S, 3))).toEqual({ host: HOST, raw: S, approval: 3 });
    expect(parseApproveLink("octet://approve/mac%2Etail1234.ts.net/4821%2D1790000000/%33")).toEqual({ host: HOST, raw: S, approval: 3 });
  });

  it("takes an empty host as none: the only pairing's", () => {
    expect(parseApproveLink(`octet://approve//${S}/3`)).toEqual({ host: null, raw: S, approval: 3 });
  });

  it.each([
    ["another scheme", `https://approve/${HOST}/${S}/3`],
    ["another action", `octet://allow/${HOST}/${S}/3`],
    ["a missing part", `octet://approve/${HOST}/${S}`],
    ["an extra part", `octet://approve/${HOST}/${S}/3/4`],
    ["a query", `octet://approve/${HOST}/${S}/3?allow=1`],
    ["a fragment", `octet://approve/${HOST}/${S}/3#x`],
    ["a host with a space", `octet://approve/evil%20host/${S}/3`],
    ["a host with a slash", `octet://approve/a%2Fb/${S}/3`],
    ["a host with an empty label", `octet://approve/a..b/${S}/3`],
    ["a host too long", `octet://approve/${"a".repeat(254)}/${S}/3`],
    ["a path for a session", `octet://approve/${HOST}/..%2Fsettings/3`],
    ["a session id of another shape", `octet://approve/${HOST}/4821/3`],
    ["no session", `octet://approve/${HOST}//3`],
    ["an approval that is no number", `octet://approve/${HOST}/${S}/x`],
    ["a negative approval", `octet://approve/${HOST}/${S}/-1`],
    ["an approval in exponent form", `octet://approve/${HOST}/${S}/1e3`],
    ["an approval past the safe integers", `octet://approve/${HOST}/${S}/99999999999999999999`],
    ["bad percent-encoding", `octet://approve/${HOST}/${S}/%zz`],
    ["nothing", ""],
  ])("refuses %s", (_what, url) => {
    expect(parseApproveLink(url)).toBeNull();
  });
});

describe("the system path of an approve link", () => {
  it("is handed to the shell, and the router is given nothing; any other path is left as it is", () => {
    const got: string[] = [];
    const stop = watchApproveLinks((url) => got.push(url));
    expect(redirectSystemPath({ path: link(HOST, S, 3), initial: false })).toBeNull();
    expect(redirectSystemPath({ path: "octet://approve/garbage", initial: true })).toBeNull();
    stop();
    expect(got).toEqual([link(HOST, S, 3), "octet://approve/garbage"]);
    expect(redirectSystemPath({ path: "octet://", initial: true })).toBe("octet://");
    expect(redirectSystemPath({ path: "/session/x", initial: false })).toBe("/session/x");
  });
});

beforeEach(async () => {
  await AsyncStorage.clear();
  await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
  await saveSettings({ hubUrl: `wss://${HOST}/` });
  octetStore.getState().reset();
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
  jest.spyOn(AppState, "addEventListener").mockImplementation(() => ({ remove: jest.fn() }));
});
afterEach(() => jest.restoreAllMocks());

let seq = 0;
// renderRouter runs on fake timers.
const wait = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));

/** The app, paired to one Mac; it connects, and lists `list` unless that is null. */
async function launch(list: SessionInfo[] | null) {
  seq = 0;
  const sockets = new Sockets();
  const clock = new FakeClock();
  const hub = new Fleet({
    factory: sockets.factory,
    store: octetStore,
    platform: "ios",
    clientOptions: { now: clock.now, schedule: clock.schedule, random: () => 1 },
    newId: newM,
  });
  const Layout = () => (
    <HubContext.Provider value={hub}>
      <RootLayout />
    </HubContext.Provider>
  );
  const view = renderRouter({
    _layout: Layout,
    index: Sessions,
    pair: Pair,
    "session/[id]": Session,
    recent: () => null,
    new: () => null,
    settings: () => null,
    usage: () => null,
    changes: () => null,
  });
  await view;
  await act(async () => {
    sockets.last.open();
    sockets.last.receive({ type: "welcome", device: "iPhone" });
    if (list) sockets.last.receive({ type: "sessions", sessions: list });
    clock.advance(50);
  });
  return { hub, sockets, clock, view };
}
type Rig = Awaited<ReturnType<typeof launch>>;

async function feed({ sockets, clock }: Rig, ...frames: object[]) {
  await act(async () => {
    for (const frame of frames) sockets.last.receive(frame);
    clock.advance(50);
  });
}

const open = async (url: string) => act(async () => handOverApproveLink(url));
const answers = (rig: Rig) => rig.sockets.last.commandsOf("answer");
const ev = (event: object, session = S) => ({ type: "event", session, journal: "j1", seq: ++seq, event });
const subscribed = (session = S) => ({ type: "subscribed", session, from: "start", journals: ["j1"] });

describe("approve_link_never_allows_alone", () => {
  it("opens the approval in its session; nothing is sent until the arm and Face ID", async () => {
    const rig = await launch([waiting()]);
    await open(link(HOST, S, 3));
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
    await wait(2_000);
    expect(answers(rig)).toEqual([]);
    expect(auth.authenticateAsync).not.toHaveBeenCalled();

    // The session streams its approval: the Permission card shows it, armed only 400 ms on.
    await feed(rig, subscribed(), ev({ type: "approval", data: { id: 3, detail: "", summary } }));
    expect(screen.getByText("make test")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Allow" }));
    await act(async () => {});
    expect(answers(rig)).toEqual([]);
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    await wait(450);

    // Face ID refused: nothing goes.
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
    await fireEvent.press(screen.getByRole("button", { name: "Allow" }));
    await act(async () => {});
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(answers(rig)).toEqual([]);

    // Face ID passed: the one answer.
    await fireEvent.press(screen.getByRole("button", { name: "Allow" }));
    await act(async () => {});
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
    expect(answers(rig)).toEqual([expect.objectContaining({ session: S, approval: 3, allow: true })]);
  });

  it("opens the approval's session from any screen, as a tap does", async () => {
    const rig = await launch([waiting()]);
    await act(async () => router.push("/settings"));
    await open(link(HOST, S, 3));
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
    expect(answers(rig)).toEqual([]);
  });

  it("a newer link while one waits for its Mac's list: only the newer one counts", async () => {
    const rig = await launch(null);
    await open(link(HOST, S, 3));
    await open("octet://approve/garbage");
    await feed(rig, { type: "sessions", sessions: [waiting()] });
    expect(rig.view.getPathname()).toBe("/");
  });

  it("waits for its Mac's list before it decides", async () => {
    const rig = await launch(null);
    await open(link(HOST, S, 3));
    expect(rig.view.getPathname()).toBe("/");
    await feed(rig, { type: "sessions", sessions: [waiting()] });
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
    expect(answers(rig)).toEqual([]);
  });

  it("a cold start from the link opens it once the app knows it is paired", async () => {
    handOverApproveLink(link(HOST, S, 3)); // before the shell listens
    const rig = await launch([waiting()]);
    await wait(100);
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
    expect(answers(rig)).toEqual([]);
  });

  it("with one Mac, a link with no host is that Mac's", async () => {
    const rig = await launch([waiting()]);
    await open(`octet://approve//${S}/3`);
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
  });

  it.each([
    ["forged", "octet://approve/garbage"],
    ["for an unknown Mac", link("evil.example.com", S, 3)],
    ["stale: its approval was answered", link(HOST, S, 9)],
    ["for a session its Mac doesn't list", link(HOST, "4822-1790000000", 3)],
    ["for an approval that waits behind the front one", link(HOST, "4824-1790000000", 4)],
  ])("a link %s opens the Inbox and sends nothing", async (_what, url) => {
    const rig = await launch([waiting(), info("4823-1790000000"), info("4824-1790000000", { approvals: 2, approval: { id: 3, summary } })]);
    await act(async () => router.push("/settings"));
    expect(rig.view.getPathname()).toBe("/settings");
    await open(url);
    await wait(10_000);
    expect(rig.view.getPathname()).toBe("/");
    expect(answers(rig)).toEqual([]);
    expect(rig.sockets.last.commandsOf("subscribe")).toEqual([]);
  });

  it("a link for an approval behind the front one opens the Inbox, even with the stream holding it", async () => {
    const rig = await launch([info(S, { approvals: 2, approval: { id: 3, summary } })]);
    await open(link(HOST, S, 3));
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
    await feed(rig, subscribed(), ev({ type: "approval", data: { id: 3, detail: "", summary } }), ev({ type: "approval", data: { id: 4, detail: "", summary } }));
    expect(octetStore.getState().bySession.get(k(S))?.approvals.map((a) => a.id)).toEqual([3, 4]);
    await act(async () => router.push("/settings"));
    await open(link(HOST, S, 4));
    await wait(10_000);
    expect(rig.view.getPathname()).toBe("/");
    expect(answers(rig)).toEqual([]);
  });

  it("a link for a session that ended (its view kept, marked ended) opens the Inbox", async () => {
    const rig = await launch([waiting()]);
    await open(link(HOST, S, 3));
    expect(rig.view.getPathname()).toBe(`/session/${k(S)}`);
    await feed(rig, subscribed());
    await feed(rig, { type: "sessions", sessions: [] }); // the session ended
    expect(octetStore.getState().bySession.get(k(S))?.ended).toBe(true);
    await act(async () => router.push("/settings"));
    const before = rig.sockets.last.commandsOf("subscribe").length;
    await open(link(HOST, S, 3));
    await wait(10_000);
    expect(rig.view.getPathname()).toBe("/");
    expect(answers(rig)).toEqual([]);
    expect(rig.sockets.last.commandsOf("subscribe")).toHaveLength(before);
  });

  it("a session with nothing waiting opens the Inbox", async () => {
    const rig = await launch([info(S)]);
    await open(link(HOST, S, 3));
    await wait(10_000);
    expect(rig.view.getPathname()).toBe("/");
  });

  it("a Mac that never lists: the Inbox, and nothing opens later", async () => {
    const rig = await launch(null);
    await open(link(HOST, S, 3));
    // The Fleet's clock: past the wait for the list, short of the link's keepalive.
    await act(async () => rig.clock.advance(APPROVE_WAIT_MS + 1_000));
    await feed(rig, { type: "sessions", sessions: [waiting()] });
    expect(rig.view.getPathname()).toBe("/");
  });
});
