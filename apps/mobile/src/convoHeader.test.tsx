// The Conversation's header (☰, the session's name ▾, Aa, ⋯), its model and mode menu, and the
// Session info sheet. The menu carries the guarded full-access flow and the provider-switch
// confirm that the old Mode and model sheet had.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, render, screen, within } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { Profiler } from "react";
import { AppState, StyleSheet } from "react-native";

import { Refusal, type BatchItem, type RequestBody } from "./client/HubClient";
import { ConvoHeader } from "./components/ConvoHeader";
import { APPROVAL_WAITING, FULL_ACCESS_ON, MENU_DISMISS_MS, MODE_HELP, ModelModeMenu, WAITING_FOR_MAC } from "./components/ModelModeMenu";
import { closeDrawer, drawerStore, setDrawerEnabled } from "./drawer";
import { SETTINGS_KEY, saveSettings } from "./persist";
import { requestMenu } from "./convoMenu";
import type { CatalogInfo, Phase, SessionInfo } from "./protocol";
import { octetStore } from "./store";
import { resetTextSize, textSizeStore } from "./textSize";
import { dark, light } from "./theme";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

const CATALOG: CatalogInfo = {
  modes: ["ask", "accept-edits", "auto", "full-access"],
  current_mode: "ask",
  models: [
    { engine: "claude", selection: "sonnet", name: "Sonnet", id: "claude-sonnet-5" },
    { engine: "claude", selection: "opus", name: "Opus", id: "claude-opus-5" },
    { engine: "codex", selection: "gpt", name: "GPT", id: "gpt-6" },
    { engine: "claude", selection: "haiku", name: "Haiku", id: "claude-haiku-5" },
  ],
  current: { engine: "claude", selection: "sonnet" },
};

const SESSION = "a1b2c3d4e5";

const info = (extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id: SESSION,
  engine: "claude",
  workspace: "/Users/me/Projects/octet",
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

/** A `send` the test settles by hand. */
function rig() {
  const sent: RequestBody[] = [];
  const waiting: { resolve: () => void; reject: (e: unknown) => void }[] = [];
  const send = jest.fn((body: RequestBody) => {
    sent.push(body);
    return new Promise<unknown>((resolve, reject) => waiting.push({ resolve: () => resolve({}), reject }));
  });
  return { sent, send, waiting };
}
type Rig = ReturnType<typeof rig>;

/** The store, as the hub would fill it: the session listed and, when given, its catalog. */
async function hear(catalog: CatalogInfo | null, extra: Partial<SessionInfo> = {}) {
  const frames: object[] = [{ v: 1, type: "sessions", sessions: [info(extra)] }];
  if (catalog) frames.push({ v: 1, type: "catalog", session: SESSION, catalog });
  await act(async () => void octetStore.getState().applyBatch(frames as BatchItem[]));
}

const flush = () => act(async () => void (await new Promise((resolve) => setTimeout(resolve, 0))));

interface Opts {
  live?: boolean;
  phase?: Phase;
  offline?: boolean;
  approval?: boolean;
  focused?: boolean;
  onCovered?: (covered: boolean) => void;
}
const header = (r: Rig, o: Opts = {}) => (
  <ConvoHeader
    session={SESSION}
    send={r.send}
    top={0}
    live={o.live ?? true}
    offline={o.offline ?? false}
    approval={o.approval ?? false}
    focused={o.focused ?? true}
    phase={o.phase}
    onCovered={o.onCovered ?? noop}
  />
);
const noop = () => undefined;

const button = (name: string | RegExp) => screen.queryByRole("button", { name });
const press = async (name: string | RegExp) => {
  await fireEvent.press(screen.getByRole("button", { name }));
  await flush();
};
const title = () => screen.getByRole("button", { name: /^octet, / });
const openMenu = async () => {
  await fireEvent.press(title());
  await flush();
};
const color = (node: { props: { style?: unknown } }): unknown =>
  (StyleSheet.flatten(node.props.style as never) as { color?: unknown } | undefined)?.color;

beforeEach(async () => {
  await AsyncStorage.clear();
  await saveSettings({ faceId: false });
  octetStore.getState().reset();
  resetTextSize();
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
});
afterEach(() => {
  closeDrawer();
  setDrawerEnabled(false);
});

describe("the conversation header", () => {
  it("shows ☰, the session's name ▾ with model · mode under it, Aa and ⋯", async () => {
    await hear(CATALOG);
    await render(header(rig()));
    expect(screen.getByText("octet")).toBeTruthy();
    expect(screen.getByText("▾")).toBeTruthy();
    expect(screen.getByText("Sonnet · ask")).toBeTruthy();
    expect(title().props.accessibilityLabel).toBe("octet, Sonnet · ask");
    for (const name of ["Menu", "Text size", "Session info"]) expect(button(name)).toBeTruthy();
    // Each is at least 44 × 44.
    for (const name of ["Menu", "Text size", "Session info"]) {
      const style = StyleSheet.flatten(screen.getByRole("button", { name }).props.style);
      expect(style.minHeight ?? style.height).toBeGreaterThanOrEqual(44);
      expect(style.minWidth ?? style.width).toBeGreaterThanOrEqual(44);
    }
  });

  it("falls back to the listed model and mode before a catalog, and to the mode alone", async () => {
    await hear(null, { model: "claude-opus-5", mode: "auto" });
    const view = await render(header(rig()));
    expect(screen.getByText("claude-opus-5 · auto")).toBeTruthy();
    await hear(null, { mode: "auto" });
    await view.rerender(header(rig()));
    expect(screen.getByText("auto")).toBeTruthy();
  });

  it("keeps the title once the session leaves the list", async () => {
    await hear(CATALOG);
    await render(header(rig()));
    await act(async () => void octetStore.getState().applyBatch([{ v: 1, type: "sessions", sessions: [] }] as BatchItem[]));
    expect(screen.getByText("octet")).toBeTruthy();
  });

  it("☰ opens the drawer", async () => {
    setDrawerEnabled(true);
    await hear(CATALOG);
    await render(header(rig()));
    await press("Menu");
    expect(drawerStore.getState().open).toBe(true);
  });

  it("draws nothing again while the session streams, or while another session changes", async () => {
    await hear(CATALOG);
    let commits = 0;
    await render(
      <Profiler id="header" onRender={() => void (commits += 1)}>
        {header(rig())}
      </Profiler>,
    );
    commits = 0;
    let seq = 0;
    for (let b = 0; b < 10; b++) {
      await act(async () => {
        octetStore.getState().applyBatch(
          Array.from({ length: 10 }, (_, i) => ({
            v: 1,
            type: "event",
            session: SESSION,
            journal: "j1",
            seq: ++seq,
            event: { type: "text", data: ` ${b * 10 + i}` },
          })) as BatchItem[],
        );
      });
    }
    await act(async () => {
      octetStore.getState().applyBatch([
        { v: 1, type: "sessions", sessions: [info(), { ...info(), id: "other", phase: "running" }] },
        { v: 1, type: "state", session: SESSION, state: { phase: "running", queue: [], queued: 0, mode: "ask", approvals: [], now: "Thinking" } },
      ] as BatchItem[]);
    });
    expect(octetStore.getState().bySession.get(SESSION)?.transcript.entries.length).toBeGreaterThan(0);
    expect(commits).toBe(0);
  });

  describe("Aa", () => {
    const saved = async () => JSON.parse((await AsyncStorage.getItem(SETTINGS_KEY)) ?? "{}").textSize;

    it("cycles the transcript's text size 13 → 14 → 12 → 13 and saves it", async () => {
      await hear(CATALOG);
      await render(header(rig()));
      expect(textSizeStore.getState().size).toBe(13);
      await press("Text size");
      expect(textSizeStore.getState().size).toBe(14);
      expect(await saved()).toBe(14);
      await press("Text size");
      expect(textSizeStore.getState().size).toBe(12);
      expect(await saved()).toBe(12);
      await press("Text size");
      expect(textSizeStore.getState().size).toBe(13);
      expect(await saved()).toBe(13);
    });

    it("does not draw the header again", async () => {
      await hear(CATALOG);
      let commits = 0;
      await render(
        <Profiler id="header" onRender={() => void (commits += 1)}>
          {header(rig())}
        </Profiler>,
      );
      commits = 0;
      await press("Text size");
      await press("Text size");
      expect(textSizeStore.getState().size).toBe(12);
      expect(commits).toBe(0);
    });
  });

  describe("⋯ Session info", () => {
    it("shows engine / model, the mode, the workspace and the session id, and closes", async () => {
      await hear(CATALOG);
      const covered = jest.fn();
      await render(header(rig(), { onCovered: covered }));
      await press("Session info");
      const sheet = within(screen.getByTestId("info"));
      expect(sheet.getByText("Session info")).toBeTruthy();
      expect(sheet.getByText("claude / Sonnet")).toBeTruthy();
      expect(sheet.getByText("ask")).toBeTruthy();
      expect(sheet.getByText("/Users/me/Projects/octet")).toBeTruthy();
      expect(sheet.getByText(SESSION)).toBeTruthy();
      expect(covered).toHaveBeenLastCalledWith(true);
      await press("Close");
      expect(screen.queryByTestId("info")).toBeNull();
      expect(covered).toHaveBeenLastCalledWith(false);
    });

    it("opens from the menu's Session info row, with the session's short id", async () => {
      await hear(CATALOG);
      await render(header(rig()));
      await openMenu();
      expect(screen.getByText("a1b2c3 ›")).toBeTruthy();
      await press("Session info, a1b2c3");
      // The info sheet waits for the menu's Modal to go (iOS drops a Modal shown over one leaving).
      expect(screen.queryByTestId("menu")).toBeNull();
      expect(screen.queryByTestId("info")).toBeNull();
      await act(async () => void (await new Promise((resolve) => setTimeout(resolve, MENU_DISMISS_MS + 50))));
      expect(within(screen.getByTestId("info")).getByText(SESSION)).toBeTruthy();
    });
  });

  // Moved from the old session controls: End lives in Session info now.
  describe("End, in ⋯ Session info", () => {
    const openInfo = () => press("Session info");
    const disabledNow = (name: string) => screen.getByRole("button", { name }).props.accessibilityState?.disabled === true;

    it("shows End only when idle, and asks first", async () => {
      await hear(CATALOG);
      const r = rig();
      const view = await render(header(r, { phase: "idle" }));
      await openInfo();
      expect(button("End session")).toBeTruthy();
      await press("End session");
      expect(r.send).not.toHaveBeenCalled();
      expect(screen.getByText("End this session?")).toBeTruthy();
      await press("Keep it");
      expect(screen.queryByText("End this session?")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
      await press("End session");
      await press("Yes, end it");
      expect(r.sent).toEqual([{ type: "end", session: SESSION }]);
      await view.rerender(header(r, { phase: "running" }));
      expect(button("End session")).toBeNull();
      for (const phase of ["connecting", "stopped"] as const) {
        await view.rerender(header(r, { phase }));
        expect(button("End session")).toBeNull();
      }
    });

    it("is not offered for a session that takes no commands", async () => {
      await hear(CATALOG);
      await render(header(rig(), { phase: "idle", live: false }));
      await openInfo();
      expect(button("End session")).toBeNull();
    });

    it("shows the refusal of an end, in fg, and the confirmation goes", async () => {
      await hear(CATALOG);
      const r = rig();
      await render(header(r, { phase: "idle" }));
      await openInfo();
      await press("End session");
      await press("Yes, end it");
      r.waiting[0]?.reject(new Refusal("turn_open", "A turn is still open"));
      await flush();
      expect([dark.fg, light.fg]).toContain(color(screen.getByText("A turn is still open")));
      expect(screen.queryByText("End this session?")).toBeNull();
    });

    it("forgets a half-made End when the phase changes", async () => {
      await hear(CATALOG);
      const r = rig();
      const view = await render(header(r, { phase: "idle" }));
      await openInfo();
      await press("End session");
      expect(screen.getByText("End this session?")).toBeTruthy();
      await view.rerender(header(r, { phase: "running" }));
      await view.rerender(header(r, { phase: "idle" }));
      expect(screen.queryByText("End this session?")).toBeNull();
      expect(button("End session")).toBeTruthy();
    });

    it("forgets a half-made End when the sheet closes", async () => {
      await hear(CATALOG);
      await render(header(rig(), { phase: "idle" }));
      await openInfo();
      await press("End session");
      await press("Close");
      await openInfo();
      expect(screen.queryByText("End this session?")).toBeNull();
    });

    it("says Ending… after End's ack, disabled, until the phase moves on", async () => {
      await hear(CATALOG);
      const r = rig();
      const view = await render(header(r, { phase: "idle" }));
      await openInfo();
      await press("End session");
      await press("Yes, end it");
      r.waiting[0]?.resolve();
      await flush();
      expect(button("End session")).toBeNull();
      expect(disabledNow("Ending…")).toBe(true);
      await press("Ending…");
      expect(r.send).toHaveBeenCalledTimes(1);
      await view.rerender(header(r, { phase: "running" }));
      await view.rerender(header(r, { phase: "idle" }));
      expect(button("End session")).toBeTruthy();
    });

    it("offline, End sends nothing, then comes back with the link", async () => {
      await hear(CATALOG);
      const r = rig();
      const view = await render(header(r, { phase: "idle", offline: true }));
      await openInfo();
      expect(disabledNow("End session")).toBe(true);
      await press("End session");
      expect(button("Yes, end it")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
      await view.rerender(header(r, { phase: "idle" }));
      expect(disabledNow("End session")).toBe(false);
    });

    it("a half-made End cannot be confirmed once the link drops", async () => {
      await hear(CATALOG);
      const r = rig();
      const view = await render(header(r, { phase: "idle" }));
      await openInfo();
      await press("End session");
      await view.rerender(header(r, { phase: "idle", offline: true }));
      expect(disabledNow("Yes, end it")).toBe(true);
      await press("Yes, end it");
      expect(r.send).not.toHaveBeenCalled();
    });
  });
});

describe("the ▾ menu", () => {
  it("opens for the prompt line's /model, for this session only, and not for one that takes no commands", async () => {
    await hear(CATALOG);
    const view = await render(header(rig()));
    await act(async () => void requestMenu("someone-else"));
    expect(screen.queryByTestId("menu")).toBeNull();
    await act(async () => void requestMenu(SESSION));
    expect(screen.getByTestId("menu")).toBeTruthy();
    await press("Close");
    await view.rerender(header(rig(), { live: false }));
    await act(async () => void requestMenu(SESSION));
    expect(screen.queryByTestId("menu")).toBeNull();
  });

  it("opens for /model only on the screen in front: a copy behind another screen ignores it", async () => {
    await hear(CATALOG);
    const view = await render(header(rig(), { focused: false }));
    let ran = true;
    await act(async () => void (ran = requestMenu(SESSION)));
    expect(ran).toBe(false);
    expect(screen.queryByTestId("menu")).toBeNull();
    await view.rerender(header(rig(), { focused: true }));
    await act(async () => void (ran = requestMenu(SESSION)));
    expect(ran).toBe(true);
    expect(screen.getByTestId("menu")).toBeTruthy();
  });

  it("lists the session engine's models with ✓ on the current one, and other engines' after", async () => {
    await hear(CATALOG);
    await render(header(rig()));
    await openMenu();
    const menu = within(screen.getByTestId("menu"));
    const names = menu.getAllByText(/^(MODEL · claude|MODEL · codex|Sonnet|Opus|Haiku|GPT)$/).map((n) => n.props.children);
    expect(names).toEqual(["MODEL · claude", "Sonnet", "Opus", "Haiku", "MODEL · codex", "GPT"]);
    const current = screen.getByRole("button", { name: "Sonnet, current" });
    expect(within(current).getByText("✓")).toBeTruthy();
    expect([dark.accent, light.accent]).toContain(color(within(current).getByText("Sonnet")));
    expect(within(screen.getByRole("button", { name: "Opus" })).queryByText("✓")).toBeNull();
    expect(StyleSheet.flatten(current.props.style).minHeight).toBeGreaterThanOrEqual(44);
  });

  it("a model tap sends set_model, and the ack closes the menu", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await press("Opus");
    expect(r.sent).toEqual([{ type: "set_model", session: SESSION, selection: "opus" }]);
    expect(screen.getAllByText("Changing…").length).toBeGreaterThan(0);
    await act(async () => r.waiting[0]?.resolve());
    expect(screen.queryByTestId("menu")).toBeNull();
  });

  it("does nothing for the current model or mode", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await press("Sonnet, current");
    await fireEvent.press(screen.getByRole("tab", { name: "ask" }));
    await flush();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("warns about a provider switch and sends only after Switch", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await press("GPT");
    expect(r.send).not.toHaveBeenCalled();
    expect(screen.getByText("Switches to Codex; the conversation carries over")).toBeTruthy();
    await press("Opus"); // another choice drops the warning
    expect(screen.queryByText(/Switches to/)).toBeNull();
    await act(async () => r.waiting[0]?.resolve());
    await hear(CATALOG);
    await openMenu();
    await press("GPT");
    await press("Switch");
    expect(r.sent.at(-1)).toEqual({ type: "set_model", session: SESSION, selection: "gpt" });
  });

  it("the Segmented control sends set_mode, and shows the current mode's help", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    expect(screen.getByText("PERMISSION MODE")).toBeTruthy();
    const tabs = screen.getAllByRole("tab").map((t) => t.props.accessibilityLabel);
    expect(tabs).toEqual(["ask", "accept-edits", "auto"]);
    expect(screen.getByRole("tab", { name: "ask" }).props.accessibilityState.selected).toBe(true);
    expect(screen.getByText(MODE_HELP.ask)).toBeTruthy();
    expect(MODE_HELP.ask).toBe("Ask before every command and edit.");
    await fireEvent.press(screen.getByRole("tab", { name: "auto" }));
    await flush();
    expect(r.sent).toEqual([{ type: "set_mode", session: SESSION, mode: "auto" }]);
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    await act(async () => r.waiting[0]?.resolve());
    // The menu stays; the Mac's catalog moves the selection and the help.
    await hear({ ...CATALOG, current_mode: "auto" });
    expect(screen.getByRole("tab", { name: "auto" }).props.accessibilityState.selected).toBe(true);
    expect(screen.getByText("Run commands and edits without asking.")).toBeTruthy();
    await hear({ ...CATALOG, current_mode: "accept-edits" });
    expect(screen.getByText("Edit files without asking; still ask before commands.")).toBeTruthy();
  });

  it("offers only the modes the catalog lists", async () => {
    await hear({ ...CATALOG, modes: ["ask", "auto"] });
    await render(header(rig()));
    await openMenu();
    expect(screen.getAllByRole("tab").map((t) => t.props.accessibilityLabel)).toEqual(["ask", "auto"]);
    expect(button("Full access…")).toBeNull();
  });

  it("shows a refusal in the hub's words, in fg, and stays open", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await fireEvent.press(screen.getByRole("tab", { name: "auto" }));
    await flush();
    await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish or cancel the current turn first")));
    const note = screen.getByText("Finish or cancel the current turn first");
    expect([dark.fg, light.fg]).toContain(color(note));
    expect(screen.getByTestId("menu")).toBeTruthy();
    await press("Opus"); // sending again clears it
    expect(screen.queryByText("Finish or cancel the current turn first")).toBeNull();
  });

  it("says a lost command may have reached the Mac", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await press("Opus");
    await act(async () => r.waiting[0]?.reject(new Refusal("lost", "connection dropped")));
    expect(screen.getByText(/Not sure this reached the Mac/)).toBeTruthy();
  });

  it("dropdown_without_catalog: says it is waiting for the Mac and lists no models", async () => {
    await hear(null);
    await render(header(rig()));
    expect(screen.getByText("octet")).toBeTruthy();
    await openMenu();
    expect(screen.getByText(WAITING_FOR_MAC)).toBeTruthy();
    expect(WAITING_FOR_MAC).toBe("Waiting for the Mac…");
    expect(screen.queryByText(/^MODEL · /)).toBeNull();
    expect(screen.queryAllByRole("tab")).toHaveLength(0);
    expect(button("Full access…")).toBeNull();
    expect(button("Session info, a1b2c3")).toBeTruthy();
    // The catalog comes: the menu fills in.
    await hear(CATALOG);
    expect(screen.queryByText(WAITING_FOR_MAC)).toBeNull();
    expect(button("Sonnet, current")).toBeTruthy();
  });

  it("closes on the scrim and on Android back, and says it covers the screen while open", async () => {
    await hear(CATALOG);
    const covered = jest.fn();
    await render(header(rig(), { onCovered: covered }));
    await openMenu();
    expect(covered).toHaveBeenLastCalledWith(true);
    await fireEvent.press(screen.getByTestId("menu-scrim"));
    expect(screen.queryByTestId("menu")).toBeNull();
    expect(covered).toHaveBeenLastCalledWith(false);
    await openMenu();
    await act(async () => screen.getByTestId("menu").props.onRequestClose());
    expect(screen.queryByTestId("menu")).toBeNull();
  });

  it("a screen reader closes it by its Close button, or by escape, but not while a change is out", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    const closer = () => screen.getByRole("button", { name: "Close" });
    expect(closer().props.accessibilityState).toMatchObject({ disabled: false });
    await press("Close");
    expect(screen.queryByTestId("menu")).toBeNull();
    await openMenu();
    await act(async () => screen.getByTestId("menu-panel").props.onAccessibilityEscape());
    expect(screen.queryByTestId("menu")).toBeNull();
    // A change is out: Close is off, and neither it nor escape closes the menu.
    await openMenu();
    await press("Opus");
    expect(closer().props.accessibilityState).toMatchObject({ disabled: true });
    await fireEvent.press(closer());
    await act(async () => screen.getByTestId("menu-panel").props.onAccessibilityEscape());
    expect(screen.getByTestId("menu")).toBeTruthy();
    await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish the turn first")));
    expect(closer().props.accessibilityState).toMatchObject({ disabled: false });
    await press("Close");
    expect(screen.queryByTestId("menu")).toBeNull();
  });

  it("cannot be closed while a change is out; the refusal lands in it", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await press("Opus");
    await fireEvent.press(screen.getByTestId("menu-scrim"));
    await act(async () => screen.getByTestId("menu").props.onRequestClose());
    expect(screen.getByTestId("menu")).toBeTruthy();
    await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish the turn first")));
    expect(screen.getByText("Finish the turn first")).toBeTruthy();
    await fireEvent.press(screen.getByTestId("menu-scrim"));
    expect(screen.queryByTestId("menu")).toBeNull();
  });

  it("points out an approval that comes while it is open, in amber", async () => {
    await hear(CATALOG);
    const view = await render(header(rig()));
    await openMenu();
    expect(screen.queryByText(APPROVAL_WAITING)).toBeNull();
    await view.rerender(header(rig(), { approval: true }));
    expect([dark.amber, light.amber]).toContain(color(screen.getByText(APPROVAL_WAITING)));
  });

  it("offline, its choices are off and send nothing", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r, { offline: true }));
    await openMenu();
    expect(button("Opus")?.props.accessibilityState?.disabled).toBe(true);
    expect(screen.getByRole("tab", { name: "auto" }).props.accessibilityState?.disabled).toBe(true);
    await press("Opus");
    await fireEvent.press(screen.getByRole("tab", { name: "auto" }));
    await press("Full access…");
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("is not offered for a session that takes no commands", async () => {
    await hear(CATALOG);
    await render(header(rig(), { live: false }));
    expect(title().props.accessibilityState).toMatchObject({ disabled: true });
    await openMenu();
    expect(screen.queryByTestId("menu")).toBeNull();
    expect(screen.queryByText("▾")).toBeNull();
  });

  it("clips a long model name in its label", async () => {
    await hear({ ...CATALOG, models: [{ engine: "claude", selection: "long", name: "M".repeat(300), id: "long" }], current: null });
    await render(header(rig()));
    await openMenu();
    expect(screen.getByRole("button", { name: /^MMM/ }).props.accessibilityLabel.length).toBeLessThanOrEqual(130);
  });

  it("marks no model current when the catalog names none", async () => {
    await hear({ ...CATALOG, current: null });
    await render(header(rig()));
    await openMenu();
    expect(button("Sonnet, current")).toBeNull();
    expect(screen.queryByText("✓")).toBeNull();
    await fireEvent.press(screen.getByTestId("menu-scrim"));
    const { current: _drop, ...bare } = CATALOG;
    void _drop;
    await hear(bare);
    await openMenu();
    expect(button("Sonnet, current")).toBeNull();
    expect(screen.queryByText("✓")).toBeNull();
  });

  it("treats every model as the same engine when none is current", async () => {
    await hear({ ...CATALOG, current: null });
    const r = rig();
    await render(header(r));
    await openMenu();
    await press("GPT");
    expect(screen.queryByText(/Switches to/)).toBeNull();
    expect(r.sent).toEqual([{ type: "set_model", session: SESSION, selection: "gpt" }]);
  });

  it("lists a duplicated model once", async () => {
    await hear({ ...CATALOG, models: [...CATALOG.models, { engine: "claude", selection: "sonnet", name: "Sonnet again", id: "x" }] });
    await render(header(rig()));
    await openMenu();
    expect(screen.getAllByRole("button", { name: /^Sonnet/ })).toHaveLength(1);
    expect(screen.queryByText("Sonnet again")).toBeNull();
  });
});

describe("full access from the menu", () => {
  const FULL = "Full access…";
  const field = () => screen.getByLabelText("Type full access to confirm");
  const menu = (r: Rig, catalog: CatalogInfo = CATALOG, offline = false) => (
    <ModelModeMenu session={SESSION} engine="claude" catalog={catalog} send={r.send} offline={offline} top={0} onClose={noop} onInfo={noop} />
  );

  it("opens the guarded flow: Face ID, then the exact words, then set_mode with confirm", async () => {
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await press(FULL);
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(r.send).not.toHaveBeenCalled();
    const turnOn = () => screen.getByRole("button", { name: "Turn on full access" });
    expect(turnOn().props.accessibilityState.disabled).toBe(true);
    for (const wrong of ["Full access", "full access ", " full access", "fullaccess"]) {
      await fireEvent.changeText(field(), wrong);
      expect(turnOn().props.accessibilityState.disabled).toBe(true);
    }
    await fireEvent.changeText(field(), "full access");
    await press("Turn on full access");
    expect(r.sent).toEqual([{ type: "set_mode", session: SESSION, mode: "full-access", confirm: "full access" }]);
    await act(async () => r.waiting[0]?.resolve());
    expect(screen.queryByTestId("menu")).toBeNull();
  });

  it("warns in amber, and asks for Face ID even with the setting off", async () => {
    const r = rig();
    await render(menu(r));
    expect([dark.amber, light.amber]).toContain(color(screen.getByText("The agent can run any command without asking.")));
    await press(FULL);
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
  });

  it("sends nothing and shows no field when Face ID fails", async () => {
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
    const r = rig();
    await render(menu(r));
    await press(FULL);
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("says it needs Face ID when there is none", async () => {
    auth.isEnrolledAsync.mockResolvedValueOnce(false);
    const r = rig();
    await render(menu(r));
    await press(FULL);
    expect(screen.getByText("Full access needs Face ID set up on this phone.")).toBeTruthy();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("does nothing when full access is the current mode", async () => {
    const r = rig();
    await render(menu(r, { ...CATALOG, current_mode: "full-access" }));
    expect(screen.getAllByRole("tab").every((t) => !t.props.accessibilityState.selected)).toBe(true);
    await press("Full access, current");
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(r.send).not.toHaveBeenCalled();
    // The help line says it is on, rather than repeating the row's warning.
    expect(screen.getByText(FULL_ACCESS_ON)).toBeTruthy();
    expect(FULL_ACCESS_ON).toBe("Full access is on.");
    expect(screen.getAllByText("The agent can run any command without asking.")).toHaveLength(1);
  });

  it("says it needs Face ID when the phone has no biometric hardware", async () => {
    auth.hasHardwareAsync.mockResolvedValueOnce(false);
    const r = rig();
    await render(menu(r));
    await press(FULL);
    expect(screen.getByText("Full access needs Face ID set up on this phone.")).toBeTruthy();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("focuses the field", async () => {
    await render(menu(rig()));
    await press(FULL);
    expect(field().props.autoFocus).toBe(true);
  });

  it("arms nothing when the desk changes the mode while Face ID is up", async () => {
    let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
    auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
    const r = rig();
    const view = await render(menu(r));
    await fireEvent.press(screen.getByRole("button", { name: FULL }));
    await flush();
    await view.rerender(menu(r, { ...CATALOG, current_mode: "auto" }));
    await act(async () => finish({ success: true }));
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("a refusal disarms it: the next try asks for Face ID again", async () => {
    const r = rig();
    await render(menu(r));
    await press(FULL);
    await fireEvent.changeText(field(), "full access");
    await press("Turn on full access");
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull(); // sent: spent
    await act(async () => r.waiting[0]?.reject(new Refusal("not_allowed", "Full access is off on this Mac")));
    expect(screen.getByText("Full access is off on this Mac")).toBeTruthy();
    expect(button("Turn on full access")).toBeNull();
    await press(FULL);
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
    expect(field().props.value).toBe("");
  });

  it("gives one prompt for a double tap", async () => {
    let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
    auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
    const r = rig();
    await render(menu(r));
    const row = screen.getByRole("button", { name: FULL });
    await fireEvent.press(row);
    await fireEvent.press(row);
    await act(async () => finish({ success: true }));
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
  });

  it("arms nothing if the menu closed while Face ID was up", async () => {
    let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
    auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
    await hear(CATALOG);
    const r = rig();
    await render(header(r));
    await openMenu();
    await fireEvent.press(screen.getByRole("button", { name: FULL }));
    await flush();
    await act(async () => screen.getByTestId("menu").props.onRequestClose());
    await openMenu();
    await act(async () => finish({ success: true }));
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("sends nothing if the session stopped taking commands while Face ID was up", async () => {
    let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
    auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
    await hear(CATALOG);
    const r = rig();
    const view = await render(header(r));
    await openMenu();
    await fireEvent.press(screen.getByRole("button", { name: FULL }));
    await flush();
    await view.rerender(header(r, { live: false }));
    await act(async () => finish({ success: true }));
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("is disarmed when the desk changes the mode", async () => {
    const r = rig();
    const view = await render(menu(r));
    await press(FULL);
    expect(field()).toBeTruthy();
    await view.rerender(menu(r, { ...CATALOG, current_mode: "auto" }));
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
    await view.rerender(menu(r, CATALOG));
    expect(button("Turn on full access")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("can be cancelled", async () => {
    const r = rig();
    await render(menu(r));
    await press(FULL);
    await press("Cancel full access");
    expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("armed before the link dropped, cannot be turned on while offline", async () => {
    const r = rig();
    const view = await render(menu(r));
    await press(FULL);
    await fireEvent.changeText(field(), "full access");
    await view.rerender(menu(r, CATALOG, true));
    const turnOn = screen.getByRole("button", { name: "Turn on full access" });
    expect(turnOn.props.accessibilityState.disabled).toBe(true);
    await fireEvent.press(turnOn);
    expect(r.send).not.toHaveBeenCalled();
  });

  describe("leaving the app", () => {
    let listeners: ((state: string) => void)[] = [];
    beforeEach(() => {
      listeners = [];
      jest.spyOn(AppState, "addEventListener").mockImplementation((_type, listener) => {
        listeners.push(listener as (state: string) => void);
        return { remove: () => void (listeners = listeners.filter((l) => l !== listener)) } as ReturnType<typeof AppState.addEventListener>;
      });
    });
    afterEach(() => jest.restoreAllMocks());
    const change = (state: string) => act(async () => listeners.forEach((l) => l(state)));

    it("forgets a Face ID pass: back in the app, the words are not enough", async () => {
      const r = rig();
      await render(menu(r));
      await press(FULL);
      expect(field()).toBeTruthy();
      await change("background");
      await change("active");
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("a prompt still up when the app goes to the background arms nothing when it passes", async () => {
      let pass: () => void = () => {};
      auth.authenticateAsync.mockImplementationOnce(() => new Promise((resolve) => (pass = () => resolve({ success: true }))));
      const r = rig();
      await render(menu(r));
      await fireEvent.press(screen.getByRole("button", { name: FULL }));
      await change("inactive");
      await change("background");
      await act(async () => pass());
      await change("active");
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("asks for no Face ID while the app is not in front", async () => {
      const r = rig();
      await render(menu(r));
      const state = jest.replaceProperty(AppState, "currentState", "background");
      try {
        await fireEvent.press(screen.getByRole("button", { name: FULL }));
        await flush();
        expect(auth.authenticateAsync).not.toHaveBeenCalled();
        expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      } finally {
        state.restore();
      }
    });

    it("forgets it on the app switcher too, but not while its own Face ID prompt is up", async () => {
      let pass: () => void = () => {};
      auth.authenticateAsync.mockImplementationOnce(() => new Promise((resolve) => (pass = () => resolve({ success: true }))));
      const r = rig();
      await render(menu(r));
      await fireEvent.press(screen.getByRole("button", { name: FULL }));
      await change("inactive"); // the system prompt itself
      await act(async () => pass());
      await change("active");
      expect(field()).toBeTruthy();
      await change("inactive");
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });
  });
});
