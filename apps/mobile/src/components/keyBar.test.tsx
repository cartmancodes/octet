// The key bar under the prompt line: esc · ⇧tab · ↑ · / · ⏎, or y · n · esc while an approval
// waits. Each key goes through the same guards as the button it stands for.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, render, screen, within } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { Keyboard, StyleSheet } from "react-native";

import { Refusal } from "../client/HubClient";
import { saveSettings } from "../persist";
import { octetStore } from "../store";
import { dark, light } from "../theme";
import { approval, controls, disabled, flush, input, key, rig, settle } from "./controlsRig";
import { nextMode } from "./PromptLine";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;
const color = (name: string, glyph: string): unknown => {
  const label = within(screen.getByRole("button", { name })).getByText(glyph);
  return (StyleSheet.flatten(label.props.style) as { color?: unknown } | undefined)?.color;
};
const ESC = "Cancel (esc)";
const MODE = "Next mode (⇧tab)";
const UP = "Last prompt (↑)";
const SLASH = "Command (/)";
const SEND = "Send (⏎)";
const ALLOW = "Allow (y)";
const DENY = "Deny (n)";
const ESC_DENY = "Deny (esc)";

beforeEach(async () => {
  await AsyncStorage.clear();
  await saveSettings({ faceId: false });
  octetStore.getState().reset();
  auth.authenticateAsync.mockClear();
});

describe("the key bar", () => {
  it("shows esc ⇧tab ↑ / ⏎ with no approval waiting, and y n esc with one", async () => {
    const r = rig();
    const view = await render(controls(r.send));
    for (const glyph of ["esc", "⇧tab", "↑", "/", "⏎"]) expect(screen.getByText(glyph)).toBeTruthy();
    expect(screen.queryByText("y")).toBeNull();
    await view.rerender(controls(r.send, { approvals: [approval(1)] }));
    for (const glyph of ["y", "n", "esc"]) expect(screen.getByText(glyph)).toBeTruthy();
    for (const glyph of ["⇧tab", "↑", "/", "⏎"]) expect(screen.queryByText(glyph)).toBeNull();
    // Allow is once only: there is no `a` key.
    expect(screen.queryByText("a")).toBeNull();
  });

  it("keys are buttons of at least 44 pt", async () => {
    await render(controls(rig().send));
    for (const name of [ESC, MODE, UP, SLASH, SEND]) {
      const style = StyleSheet.flatten(key(name).props.style) as { height?: number; minHeight?: number };
      expect(Math.max(style.height ?? 0, style.minHeight ?? 0)).toBeGreaterThanOrEqual(44);
    }
  });

  describe("keys_follow_state_and_never_double_send", () => {
    beforeEach(() => jest.useFakeTimers());
    afterEach(() => jest.useRealTimers());

    it("offline, every key is off and sends nothing", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", offline: true }));
      await fireEvent.changeText(input(), "words");
      for (const name of [ESC, MODE, UP, SLASH, "Queue (⏎)"]) {
        expect(disabled(key(name))).toBe(true);
        await fireEvent.press(key(name));
      }
      await view.rerender(controls(r.send, { phase: "running", offline: true, approvals: [approval(1)] }));
      await settle(1_000);
      for (const name of [ALLOW, DENY, ESC_DENY]) {
        expect(disabled(key(name))).toBe(true);
        await fireEvent.press(key(name));
      }
      await flush();
      expect(r.send).not.toHaveBeenCalled();
      expect(input().props.value).toBe("words");
    });

    it("a command out holds every key that sends: esc twice cancels once, and ⇧tab and ⏎ wait", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running" }));
      await fireEvent.changeText(input(), "queued words");
      await fireEvent.press(key(ESC));
      expect(r.sent).toEqual([{ type: "cancel", session: "s" }]);
      expect(disabled(key(ESC))).toBe(true);
      expect(disabled(key(MODE))).toBe(true);
      expect(disabled(key("Queue (⏎)"))).toBe(true);
      await fireEvent.press(key(ESC));
      await fireEvent.press(key(MODE));
      await fireEvent.press(key("Queue (⏎)"));
      await fireEvent(input(), "submitEditing");
      expect(r.send).toHaveBeenCalledTimes(1);
      r.waiting[0]?.resolve();
      await flush();
      expect(disabled(key(ESC))).toBe(false);
      await fireEvent.press(key("Queue (⏎)"));
      expect(r.sent[1]).toEqual({ type: "prompt", session: "s", text: "queued words" });
    });

    it("a prompt out holds esc and ⇧tab too", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running" }));
      await fireEvent.changeText(input(), "more");
      await fireEvent.press(key("Queue (⏎)"));
      await fireEvent.press(key(ESC));
      await fireEvent.press(key(MODE));
      expect(r.sent.map((b) => b.type)).toEqual(["prompt"]);
    });

    it("with an approval waiting, esc denies exactly the front one, once, after the arm", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3), approval(4)] }));
      expect(disabled(key(ESC_DENY))).toBe(true);
      await fireEvent.press(key(ESC_DENY));
      expect(r.send).not.toHaveBeenCalled();
      await settle(400);
      expect(disabled(key(ESC_DENY))).toBe(false);
      await fireEvent.press(key(ESC_DENY));
      await fireEvent.press(key(ESC_DENY));
      await fireEvent.press(key(DENY));
      await fireEvent.press(key(ALLOW));
      await flush();
      expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 3, allow: false }]);
      r.waiting[0]?.resolve();
      await flush();
      // The next one is at the front, and arms again before a key answers it.
      expect(screen.getByText("Run command 4")).toBeTruthy();
      expect(disabled(key(ESC_DENY))).toBe(true);
      await fireEvent.press(key(ESC_DENY));
      expect(r.send).toHaveBeenCalledTimes(1);
    });

    it("n denies and y allows the front approval, like the card's buttons", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3), approval(4)] }));
      await settle(400);
      await fireEvent.press(key(DENY));
      await flush();
      r.waiting[0]?.resolve();
      await flush();
      await settle(400);
      await fireEvent.press(key(ALLOW));
      await flush();
      expect(r.sent).toEqual([
        { type: "answer", session: "s", approval: 3, allow: false },
        { type: "answer", session: "s", approval: 4, allow: true },
      ]);
    });

    it("y asks for Face ID when the setting is on, as Allow does; n never does", async () => {
      await saveSettings({ faceId: true });
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.press(key(ALLOW));
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 3, allow: true }]);
    });

    it("y and the card's Allow together send one answer", async () => {
      await saveSettings({ faceId: true });
      let pass: () => void = () => {};
      auth.authenticateAsync.mockImplementationOnce(() => new Promise((resolve) => void (pass = () => resolve({ success: true }))));
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.press(key(ALLOW));
      await fireEvent.press(screen.getByRole("button", { name: "Allow" }));
      await flush();
      await act(async () => pass());
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(r.send).toHaveBeenCalledTimes(1);
    });

    it("covered by the menu or the drawer, y n esc send nothing, and arm 400 ms after", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", approvals: [approval(3)], covered: true }));
      await settle(1_000);
      for (const name of [ALLOW, DENY, ESC_DENY]) {
        expect(disabled(key(name))).toBe(true);
        await fireEvent.press(key(name));
      }
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      expect(disabled(key(ESC_DENY))).toBe(true);
      await settle(400);
      expect(disabled(key(ESC_DENY))).toBe(false);
      expect(r.send).not.toHaveBeenCalled();
    });
  });

  describe("busy while its command is out", () => {
    const busy = (name: string) => key(name).props.accessibilityState?.busy === true;
    beforeEach(() => jest.useFakeTimers());
    afterEach(() => jest.useRealTimers());

    it("esc while its cancel is out, ⇧tab while its set_mode is, ⏎ while its prompt is", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running" }));
      expect(busy(ESC)).toBe(false);
      await fireEvent.press(key(ESC));
      expect(busy(ESC)).toBe(true);
      expect(busy(MODE)).toBe(false);
      expect(busy("Queue (⏎)")).toBe(false);
      r.waiting[0]?.resolve();
      await flush();
      expect(busy(ESC)).toBe(false);
      await fireEvent.press(key(MODE));
      expect(busy(MODE)).toBe(true);
      expect(busy(ESC)).toBe(false);
      r.waiting[1]?.resolve();
      await flush();
      expect(busy(MODE)).toBe(false);
      await fireEvent.changeText(input(), "more");
      await fireEvent.press(key("Queue (⏎)"));
      expect(busy("Queue (⏎)")).toBe(true);
      r.waiting[2]?.resolve();
      await flush();
      expect(busy("Queue (⏎)")).toBe(false);
    });

    it("y n esc while the answer is out", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      for (const name of [ALLOW, DENY, ESC_DENY]) expect(busy(name)).toBe(false);
      await fireEvent.press(key(DENY));
      await flush();
      for (const name of [ALLOW, DENY, ESC_DENY]) expect(busy(name)).toBe(true);
    });
  });

  describe("the safe area under it", () => {
    type Handler = () => void;
    let handlers: Map<string, Handler>;
    let removed: string[];
    beforeEach(() => {
      handlers = new Map();
      removed = [];
      jest.spyOn(Keyboard, "addListener").mockImplementation(((event: string, handler: Handler) => {
        handlers.set(event, handler);
        return { remove: () => void removed.push(event) } as never;
      }) as never);
    });
    afterEach(() => jest.restoreAllMocks());
    const pad = () => (StyleSheet.flatten(screen.getByTestId("key-bar").props.style) as { paddingBottom?: number }).paddingBottom;

    it("pads the home indicator only while the keyboard is down, and stops listening when it goes", async () => {
      const view = await render(controls(rig().send, { bottom: 34 }));
      expect(pad()).toBe(34);
      await act(async () => handlers.get("keyboardWillShow")?.());
      expect(pad()).toBe(8);
      await act(async () => handlers.get("keyboardWillHide")?.());
      expect(pad()).toBe(34);
      await view.unmount();
      expect(removed.sort()).toEqual(["keyboardWillHide", "keyboardWillShow"]);
    });

    it("keeps a little room when there is no home indicator", async () => {
      await render(controls(rig().send, { bottom: 0 }));
      expect(pad()).toBe(8);
    });
  });

  describe("esc", () => {
    it("cancels only while running, in accent; it is inert otherwise", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running" }));
      expect(disabled(key(ESC))).toBe(false);
      expect([dark.accent, light.accent]).toContain(color(ESC, "esc"));
      for (const phase of ["idle", "connecting", "stopped"] as const) {
        await view.rerender(controls(r.send, { phase }));
        expect(disabled(key(ESC))).toBe(true);
        await fireEvent.press(key(ESC));
      }
      expect(r.send).not.toHaveBeenCalled();
    });

    it("sends cancel, holds while it is out, and shows a refusal", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running" }));
      await fireEvent.press(key(ESC));
      expect(r.sent).toEqual([{ type: "cancel", session: "s" }]);
      await fireEvent.press(key(ESC));
      expect(r.send).toHaveBeenCalledTimes(1);
      r.waiting[0]?.reject(new Refusal("not_allowed", "Cancel is not allowed from a phone"));
      await flush();
      expect(screen.getByText("Cancel is not allowed from a phone")).toBeTruthy();
      await fireEvent.press(key(ESC));
      r.waiting[1]?.resolve();
      await flush();
      expect(screen.queryByText("Cancel is not allowed from a phone")).toBeNull();
    });

    it("draws a refusal in the text colour, not amber", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running" }));
      await fireEvent.press(key(ESC));
      r.waiting[0]?.reject(new Refusal("busy", "The Mac is busy"));
      await flush();
      const colour = (StyleSheet.flatten(screen.getByText("The Mac is busy").props.style) as { color?: string }).color;
      expect([dark.fg, light.fg]).toContain(colour);
    });
  });

  describe("⇧tab", () => {
    it("cycles ask → accept-edits → auto → ask, and never to full access", () => {
      expect(nextMode("ask", null)).toBe("accept-edits");
      expect(nextMode("accept-edits", null)).toBe("auto");
      expect(nextMode("auto", null)).toBe("ask");
      expect(nextMode("full-access", null)).toBe("ask");
      const all = ["ask", "accept-edits", "auto", "full-access"] as const;
      expect(nextMode("auto", all)).toBe("ask");
      // Only the modes the session offers.
      expect(nextMode("ask", ["ask", "auto", "full-access"])).toBe("auto");
      expect(nextMode("auto", ["ask", "auto"])).toBe("ask");
      expect(nextMode("ask", ["ask", "full-access"])).toBeNull();
      expect(nextMode("full-access", ["full-access"])).toBeNull();
    });

    it("sends set_mode for the next mode, and steps on from the one it sent until the catalog moves", async () => {
      const r = rig();
      const view = await render(controls(r.send, { mode: "ask", modes: ["ask", "accept-edits", "auto", "full-access"] }));
      await fireEvent.press(key(MODE));
      expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode: "accept-edits" }]);
      r.waiting[0]?.resolve();
      await flush();
      // Acked; the catalog frame has not come yet.
      await fireEvent.press(key(MODE));
      expect(r.sent[1]).toEqual({ type: "set_mode", session: "s", mode: "auto" });
      r.waiting[1]?.resolve();
      await flush();
      await view.rerender(controls(r.send, { mode: "auto", modes: ["ask", "accept-edits", "auto", "full-access"] }));
      await fireEvent.press(key(MODE));
      expect(r.sent[2]).toEqual({ type: "set_mode", session: "s", mode: "ask" });
      expect(r.sent.some((b) => b.type === "set_mode" && b.mode === "full-access")).toBe(false);
    });

    it("from full access goes back to ask", async () => {
      const r = rig();
      await render(controls(r.send, { mode: "full-access", modes: ["ask", "accept-edits", "auto", "full-access"] }));
      await fireEvent.press(key(MODE));
      expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode: "ask" }]);
    });

    it("is off when the session offers no other mode", async () => {
      const r = rig();
      await render(controls(r.send, { mode: "ask", modes: ["ask"] }));
      expect(disabled(key(MODE))).toBe(true);
      await fireEvent.press(key(MODE));
      expect(r.send).not.toHaveBeenCalled();
    });

    it("shows a refusal in the hub's words", async () => {
      const r = rig();
      await render(controls(r.send));
      await fireEvent.press(key(MODE));
      r.waiting[0]?.reject(new Refusal("not_allowed", "Modes are the desk's"));
      await flush();
      expect(screen.getByText("Modes are the desk's")).toBeTruthy();
    });
  });

  describe("↑", () => {
    async function sendText(r: ReturnType<typeof rig>, text: string) {
      await fireEvent.changeText(input(), text);
      await fireEvent.press(key(SEND));
      r.waiting.at(-1)?.resolve();
      await flush();
    }

    it("is off before anything was sent from this phone", async () => {
      const r = rig();
      await render(controls(r.send));
      expect(disabled(key(UP))).toBe(true);
    });

    it("brings back the last text sent, then the one before, and never sends it by itself", async () => {
      const r = rig();
      await render(controls(r.send));
      await sendText(r, "one");
      await sendText(r, "two");
      expect(input().props.value).toBe("");
      await fireEvent.press(key(UP));
      expect(input().props.value).toBe("two");
      await fireEvent.press(key(UP));
      expect(input().props.value).toBe("one");
      expect(disabled(key(UP))).toBe(true); // the oldest
      expect(r.send).toHaveBeenCalledTimes(2);
    });

    it("never replaces words the user typed", async () => {
      const r = rig();
      await render(controls(r.send));
      await sendText(r, "one");
      await fireEvent.changeText(input(), "my own words");
      expect(disabled(key(UP))).toBe(true);
      await fireEvent.press(key(UP));
      expect(input().props.value).toBe("my own words");
    });

    it("keeps only what was sent for this session, and not a text the Mac refused", async () => {
      const r = rig();
      const view = await render(controls(r.send));
      await fireEvent.changeText(input(), "refused");
      await fireEvent.press(key(SEND));
      r.waiting[0]?.reject(new Refusal("busy", "busy"));
      await flush();
      await fireEvent.changeText(input(), "");
      expect(disabled(key(UP))).toBe(true);
      await sendText(r, "for s");
      await view.rerender(controls(r.send, { session: "t" }));
      expect(disabled(key(UP))).toBe(true);
    });
  });

  describe("/", () => {
    it("starts a command: the text becomes / and the menu shows", async () => {
      const r = rig();
      await render(controls(r.send));
      await fireEvent.press(key(SLASH));
      expect(input().props.value).toBe("/");
      expect(screen.getByRole("button", { name: /^\/model, / })).toBeTruthy();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("never replaces words the user typed", async () => {
      const r = rig();
      await render(controls(r.send));
      await fireEvent.changeText(input(), "half a thought");
      expect(disabled(key(SLASH))).toBe(true);
      await fireEvent.press(key(SLASH));
      expect(input().props.value).toBe("half a thought");
    });
  });

  describe("⏎", () => {
    it("is accent with text to send, muted without", async () => {
      await render(controls(rig().send));
      expect([dark.muted, light.muted]).toContain(color(SEND, "⏎"));
      await fireEvent.changeText(input(), "go");
      expect([dark.accent, light.accent]).toContain(color(SEND, "⏎"));
    });
  });
});
