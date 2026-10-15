// The prompt line: `>` and one field, its placeholder by state, and what sending does — a prompt,
// a steer while a Codex turn runs, a deny then a prompt while an approval waits. The Composer's
// and the Steer field's behaviours, moved here.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, render, screen } from "@testing-library/react-native";
import * as Haptics from "expo-haptics";
import { readFileSync } from "node:fs";
import path from "node:path";
import { StyleSheet } from "react-native";

import { Refusal } from "../client/HubClient";
import { saveSettings } from "../persist";
import { octetStore } from "../store";
import { dark, light } from "../theme";
import { approval, controls, disabled, flush, input, key, rig, settle } from "./controlsRig";
import { JUST_APPEARED, PLACEHOLDER, PROMPT_LINES, PROMPT_MAX_HEIGHT, PROMPT_PAD, placeholderFor } from "./PromptLine";
import { APPROVAL_DEMO, isSlash } from "./send";

const sendKey = (name = "Send (⏎)") => key(name);
const submit = () => fireEvent(input(), "submitEditing");

beforeEach(async () => {
  await AsyncStorage.clear();
  await saveSettings({ faceId: false });
  octetStore.getState().reset();
  jest.mocked(Haptics.impactAsync).mockClear();
});

describe("the prompt line", () => {
  it("is `>` and a field that grows to 4 lines, then scrolls; return still sends", async () => {
    await render(controls(rig().send));
    expect(screen.getByText(">")).toBeTruthy();
    expect(input().props.multiline).toBe(true);
    expect(input().props.submitBehavior).toBe("submit");
    expect(input().props.scrollEnabled).not.toBe(false);
    expect(PROMPT_LINES).toBe(4);
    const style = StyleSheet.flatten(input().props.style) as { maxHeight?: number; lineHeight?: number; minHeight?: number };
    expect(style.maxHeight).toBe(PROMPT_MAX_HEIGHT);
    expect(PROMPT_MAX_HEIGHT).toBe(4 * (style.lineHeight ?? 0) + 2 * PROMPT_PAD);
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
  });

  it("sends text of several lines as it is, from ⏎ or return", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "first\nsecond");
    await fireEvent.press(sendKey());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "first\nsecond" }]);
  });

  it("takes its question placeholder from the question it is given", async () => {
    const view = await render(controls(rig().send, { question: true }));
    expect(input().props.placeholder).toBe(PLACEHOLDER.question);
    await view.rerender(controls(rig().send, { question: true, phase: "running" }));
    expect(input().props.placeholder).toBe(PLACEHOLDER.running);
    await view.rerender(controls(rig().send));
    expect(input().props.placeholder).toBe(PLACEHOLDER.idle);
  });

  it("marks the session's question answered when the Mac takes a prompt, and not when it refuses", async () => {
    const row = { id: "s", engine: "claude", workspace: "/w", mode: "ask", phase: "idle", approvals: 0, from_phone: false, question: true };
    octetStore.getState().applyBatch([{ v: 1, type: "sessions", sessions: [row] } as never]);
    const r = rig();
    await render(controls(r.send, { question: true }));
    await fireEvent.changeText(input(), "yes");
    await fireEvent.press(sendKey());
    await flush();
    r.waiting[0]?.reject(new Refusal("invalid", "No"));
    await flush();
    expect(octetStore.getState().sessions[0]?.question).toBe(true);
    await fireEvent.press(sendKey());
    await flush();
    r.waiting[1]?.resolve();
    await flush();
    expect(octetStore.getState().sessions[0]?.question).toBeUndefined();
  });

  it("focuses the field when the Inbox's Reply asked for it, and says the ask is spent when it goes", async () => {
    octetStore.getState().setReplyTo("s");
    const view = await render(controls(rig().send));
    expect(input().props.autoFocus).toBe(true);
    await view.unmount();
    expect(octetStore.getState().replyTo).toBeNull();
  });

  it("does not focus the field for another session's Reply", async () => {
    octetStore.getState().setReplyTo("other");
    await render(controls(rig().send));
    expect(input().props.autoFocus).toBeFalsy();
    expect(octetStore.getState().replyTo).toBe("other");
  });

  it("documents the field's height on PROMPT_LINES, and the early-return note on JUST_APPEARED", () => {
    const source = readFileSync(path.join(__dirname, "PromptLine.tsx"), "utf8").split("\n");
    const before = (name: string): string => {
      const at = source.findIndex((line) => line.startsWith(`export const ${name} `));
      expect(at).toBeGreaterThan(0);
      return source[at - 1] ?? "";
    };
    expect(before("PROMPT_LINES")).toBe("/** The field grows to this many lines, then scrolls. */");
    expect(before("JUST_APPEARED")).toBe("/** Said beside the prompt for a return in the 400 ms after an approval came: nothing was sent. */");
  });

  describe("placeholders follow the state", () => {
    it("are the spec's words", () => {
      expect(PLACEHOLDER).toEqual({
        approval: "Or say what to do instead",
        running: "Working… esc cancels",
        question: "Answer, or describe a change",
        idle: "Ask, describe a change, or /help",
      });
      expect(placeholderFor({ approval: true, running: true, question: true })).toBe(PLACEHOLDER.approval);
      expect(placeholderFor({ approval: false, running: true, question: true })).toBe(PLACEHOLDER.running);
      expect(placeholderFor({ approval: false, running: false, question: true })).toBe(PLACEHOLDER.question);
      expect(placeholderFor({ approval: false, running: false, question: false })).toBe(PLACEHOLDER.idle);
    });

    it("idle, running, and with an approval waiting", async () => {
      const r = rig();
      const view = await render(controls(r.send));
      expect(input().props.placeholder).toBe("Ask, describe a change, or /help");
      await view.rerender(controls(r.send, { phase: "running" }));
      expect(input().props.placeholder).toBe("Working… esc cancels");
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(1)] }));
      expect(input().props.placeholder).toBe("Or say what to do instead");
    });
  });

  it("⏎ says Send when idle, Queue while a Claude turn runs, and Steer while a Codex one does", async () => {
    const r = rig();
    const view = await render(controls(r.send));
    expect(sendKey("Send (⏎)")).toBeTruthy();
    await view.rerender(controls(r.send, { phase: "running" }));
    expect(sendKey("Queue (⏎)")).toBeTruthy();
    await view.rerender(controls(r.send, { phase: "running", engine: "codex" }));
    expect(sendKey("Steer (⏎)")).toBeTruthy();
    await view.rerender(controls(r.send, { phase: "idle", engine: "codex" }));
    expect(sendKey("Send (⏎)")).toBeTruthy();
  });

  it("cannot send empty or blank text", async () => {
    const r = rig();
    await render(controls(r.send));
    expect(disabled(sendKey())).toBe(true);
    await fireEvent.press(sendKey());
    await fireEvent.changeText(input(), "  \n ");
    expect(disabled(sendKey())).toBe(true);
    await fireEvent.press(sendKey());
    await submit();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("does not send a slash command, and says where they run", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "/compact");
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    expect(disabled(sendKey())).toBe(true);
    await fireEvent.press(sendKey());
    await submit();
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.changeText(input(), "fix the bug");
    expect(screen.queryByText("Slash commands run on the desk")).toBeNull();
  });

  it("follows the desk's rule for what is a slash command (crates/octet-tui/src/input.rs is_command_line)", () => {
    expect(isSlash("/model x")).toBe(true);
    expect(isSlash("  /help")).toBe(true);
    expect(isSlash("/compact\n")).toBe(true);
    expect(isSlash("/usr/lib is broken")).toBe(false); // a path, not a command name
    expect(isSlash("  /etc/hosts looks wrong")).toBe(false);
    expect(isSlash(APPROVAL_DEMO)).toBe(false); // the desk sends it as a prompt
    expect(APPROVAL_DEMO).toBe("/approval-demo");
    expect(isSlash(" /approval-demo ")).toBe(false);
    expect(isSlash("/approval-demo now")).toBe(true);
    expect(isSlash("fix /usr/lib")).toBe(false);
  });

  it("sends text that only starts like a path, and refuses a slash command even after spaces", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "  /compact");
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    expect(disabled(sendKey())).toBe(true);
    await fireEvent.changeText(input(), "/model x");
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    expect(disabled(sendKey())).toBe(true);
    await fireEvent.press(sendKey());
    await submit();
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.changeText(input(), "/usr/lib is broken");
    expect(screen.queryByText("Slash commands run on the desk")).toBeNull();
    await fireEvent.press(sendKey());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "/usr/lib is broken" }]);
  });

  it("sends the prompt with a light tap, and clears the text only after the ack", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "fix the hub");
    await fireEvent.press(sendKey());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "fix the hub" }]);
    expect(Haptics.impactAsync).toHaveBeenCalledWith("light");
    expect(input().props.value).toBe("fix the hub"); // not before the ack
    expect(disabled(sendKey())).toBe(true); // one send at a time
    await fireEvent.press(sendKey());
    await submit();
    expect(r.send).toHaveBeenCalledTimes(1);
    r.waiting[0]?.resolve();
    await flush();
    expect(input().props.value).toBe("");
  });

  it("sends from the keyboard's return key too", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "fix the hub");
    await submit();
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "fix the hub" }]);
  });

  it("offers Try again for an unsent prompt, keeping the text", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "fix the hub");
    await fireEvent.press(sendKey());
    r.waiting[0]?.reject(new Refusal("unsent", "Can't reach your Mac"));
    await flush();
    expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
    expect(input().props.value).toBe("fix the hub");
    await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
    expect(r.sent).toHaveLength(2);
    expect(r.sent[1]).toEqual({ type: "prompt", session: "s", text: "fix the hub" });
    r.waiting[1]?.resolve();
    await flush();
    expect(input().props.value).toBe("");
    expect(screen.queryByText("Can't reach your Mac")).toBeNull();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("Try again resends exactly what failed: a steer stays a steer after the turn ends", async () => {
    const r = rig();
    const view = await render(controls(r.send, { phase: "running", engine: "codex" }));
    await fireEvent.changeText(input(), "go left");
    await fireEvent.press(sendKey("Steer (⏎)"));
    r.waiting[0]?.reject(new Refusal("unsent", "Can't reach your Mac"));
    await flush();
    await view.rerender(controls(r.send, { phase: "idle", engine: "codex" }));
    await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
    expect(r.sent).toEqual([
      { type: "steer", session: "s", text: "go left" },
      { type: "steer", session: "s", text: "go left" },
    ]);
  });

  it("never offers a resend of a lost prompt, and keeps the text", async () => {
    const r = rig();
    await render(controls(r.send, { phase: "running" }));
    await fireEvent.changeText(input(), "fix the hub");
    await fireEvent.press(sendKey("Queue (⏎)"));
    r.waiting[0]?.reject(new Refusal("lost", "The Mac did not answer in time"));
    await flush();
    expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(input().props.value).toBe("fix the hub");
  });

  it("shows any other refusal in the hub's words, without Try again", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "hi");
    await fireEvent.press(sendKey());
    r.waiting[0]?.reject(new Refusal("busy", "A turn is already open <b>here</b>"));
    await flush();
    expect(screen.getByText("A turn is already open <b>here</b>")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(input().props.value).toBe("hi");
  });

  it("refuses text over 64 KiB as UTF-8 bytes, and sends exactly 64 KiB", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "é".repeat(32_769)); // 32,769 characters, 65,538 bytes
    await fireEvent.press(sendKey());
    expect(screen.getByText("That's too long to send (64 KB max).")).toBeTruthy();
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.changeText(input(), "é".repeat(32_768)); // exactly 65,536 bytes
    expect(screen.queryByText("That's too long to send (64 KB max).")).toBeNull();
    await fireEvent.press(sendKey());
    expect(r.send).toHaveBeenCalledTimes(1);
  });

  it("keeps what the user typed while the send was out", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "first");
    await fireEvent.press(sendKey());
    await fireEvent.changeText(input(), "first, and more");
    r.waiting[0]?.resolve();
    await flush();
    expect(input().props.value).toBe("first, and more");
  });

  it("draws a refusal in the text colour, not amber, which is the approval's", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "hi");
    await fireEvent.press(sendKey());
    r.waiting[0]?.reject(new Refusal("busy", "The Mac is busy"));
    await flush();
    const colour = (StyleSheet.flatten(screen.getByText("The Mac is busy").props.style) as { color?: string }).color;
    expect([dark.fg, light.fg]).toContain(colour);
  });

  it("offline, keeps the words but cannot send them, until the Mac is back", async () => {
    const r = rig();
    const view = await render(controls(r.send));
    await fireEvent.changeText(input(), "fix the hub");
    await view.rerender(controls(r.send, { offline: true }));
    expect(input().props.value).toBe("fix the hub");
    expect(disabled(sendKey())).toBe(true);
    await fireEvent.press(sendKey());
    await submit();
    expect(r.send).not.toHaveBeenCalled();
    await view.rerender(controls(r.send, { offline: false }));
    expect(input().props.value).toBe("fix the hub");
    expect(disabled(sendKey())).toBe(false);
    await fireEvent.press(sendKey());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "fix the hub" }]);
  });

  it("offline, Try again sends nothing", async () => {
    const r = rig();
    const view = await render(controls(r.send));
    await fireEvent.changeText(input(), "hi");
    await fireEvent.press(sendKey());
    r.waiting[0]?.reject(new Refusal("unsent", "Can't reach your Mac"));
    await flush();
    await view.rerender(controls(r.send, { offline: true }));
    const retry = screen.getByRole("button", { name: "Try again" });
    expect(disabled(retry)).toBe(true);
    await fireEvent.press(retry);
    expect(r.send).toHaveBeenCalledTimes(1);
  });

  it("while a Claude turn runs, sends a prompt, which queues", async () => {
    const r = rig();
    await render(controls(r.send, { phase: "running" }));
    await fireEvent.changeText(input(), "and then the docs");
    await fireEvent.press(sendKey("Queue (⏎)"));
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "and then the docs" }]);
  });

  describe("steer, while a Codex turn runs", () => {
    const codex = { phase: "running", engine: "codex" } as const;

    it("sends the text as steer, and clears it after the ack", async () => {
      const r = rig();
      await render(controls(r.send, codex));
      await fireEvent.changeText(input(), "use the other crate");
      await fireEvent.press(sendKey("Steer (⏎)"));
      expect(r.sent).toEqual([{ type: "steer", session: "s", text: "use the other crate" }]);
      expect(input().props.value).toBe("use the other crate");
      r.waiting[0]?.resolve();
      await flush();
      expect(input().props.value).toBe("");
    });

    it("sends path-like text, and refuses a slash command", async () => {
      const r = rig();
      await render(controls(r.send, codex));
      await fireEvent.changeText(input(), "/model x");
      expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
      await fireEvent.press(sendKey("Steer (⏎)"));
      expect(r.send).not.toHaveBeenCalled();
      await fireEvent.changeText(input(), "/usr/lib is broken");
      expect(screen.queryByText("Slash commands run on the desk")).toBeNull();
      await fireEvent.press(sendKey("Steer (⏎)"));
      expect(r.sent).toEqual([{ type: "steer", session: "s", text: "/usr/lib is broken" }]);
    });

    it("keeps the text on a refusal, and offers Try again only for an unsent one", async () => {
      const r = rig();
      await render(controls(r.send, codex));
      await fireEvent.changeText(input(), "go left");
      await fireEvent.press(sendKey("Steer (⏎)"));
      r.waiting[0]?.reject(new Refusal("unsent", "Can't reach your Mac"));
      await flush();
      expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
      await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
      expect(r.sent[1]).toEqual({ type: "steer", session: "s", text: "go left" });
      r.waiting[1]?.reject(new Refusal("lost", "The Mac did not answer in time"));
      await flush();
      expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
      expect(input().props.value).toBe("go left");
    });

    it("refuses text over 64 KiB", async () => {
      const r = rig();
      await render(controls(r.send, codex));
      await fireEvent.changeText(input(), "x".repeat(65_537));
      await fireEvent.press(sendKey("Steer (⏎)"));
      expect(screen.getByText("That's too long to send (64 KB max).")).toBeTruthy();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("goes as a prompt once the turn has ended: the kind is the one at the moment of sending", async () => {
      const r = rig();
      const view = await render(controls(r.send, codex));
      await fireEvent.changeText(input(), "go left");
      await view.rerender(controls(r.send, { phase: "idle", engine: "codex" }));
      expect(input().props.value).toBe("go left"); // the words outlive the turn
      await fireEvent.press(sendKey("Send (⏎)"));
      expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "go left" }]);
    });

    it("offline, keeps its words but cannot send them", async () => {
      const r = rig();
      const view = await render(controls(r.send, codex));
      await fireEvent.changeText(input(), "use the other file");
      await view.rerender(controls(r.send, { ...codex, offline: true }));
      expect(input().props.value).toBe("use the other file");
      expect(disabled(sendKey("Steer (⏎)"))).toBe(true);
      await fireEvent.press(sendKey("Steer (⏎)"));
      await submit();
      expect(r.send).not.toHaveBeenCalled();
    });
  });

  describe("while an approval waits", () => {
    beforeEach(() => jest.useFakeTimers());
    afterEach(() => jest.useRealTimers());

    it("sending denies the front approval, then sends the text as a prompt once the deny is acked", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "use the staging config instead");
      await submit();
      await flush();
      expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 3, allow: false }]);
      // A second return while the deny is out sends nothing more.
      await submit();
      await flush();
      expect(r.sent).toHaveLength(1);
      r.waiting[0]?.resolve();
      await flush();
      expect(r.sent).toEqual([
        { type: "answer", session: "s", approval: 3, allow: false },
        { type: "prompt", session: "s", text: "use the staging config instead" },
      ]);
      expect(input().props.value).toBe("use the staging config instead"); // until the prompt's ack
      r.waiting[1]?.resolve();
      await flush();
      expect(input().props.value).toBe("");
    });

    it("sends no prompt when another approval is at the front once the deny is acked, and keeps the words", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3), approval(4)] }));
      await settle(400);
      await fireEvent.changeText(input(), "use the staging config instead");
      await submit();
      await flush();
      r.waiting[0]?.resolve();
      await flush();
      expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 3, allow: false }]);
      expect(screen.getByText("Run command 4")).toBeTruthy();
      expect(input().props.value).toBe("use the staging config instead");
    });

    it("sends no prompt when a new approval came to the front while the deny was out", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "do it by hand");
      await submit();
      await flush();
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3), approval(5)] }));
      r.waiting[0]?.resolve();
      await flush();
      expect(r.sent.map((b) => b.type)).toEqual(["answer"]);
      expect(input().props.value).toBe("do it by hand");
    });

    it("Try again after a deny-then-prompt resends only the prompt, never a deny, and not while an approval is at the front", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "do it by hand");
      await submit();
      await flush();
      r.waiting[0]?.resolve();
      await flush();
      await view.rerender(controls(r.send, { phase: "running", approvals: [] }));
      r.waiting[1]?.reject(new Refusal("unsent", "Can't reach your Mac"));
      await flush();
      expect(r.sent.map((b) => b.type)).toEqual(["answer", "prompt"]);
      // A new approval comes: Try again would send the words past it.
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(5)] }));
      await settle(400);
      const retry = screen.getByRole("button", { name: "Try again" });
      expect(disabled(retry)).toBe(true);
      await fireEvent.press(retry);
      await flush();
      expect(r.sent.map((b) => b.type)).toEqual(["answer", "prompt"]);
      expect(input().props.value).toBe("do it by hand");
      // It is answered elsewhere: Try again resends the prompt as it was.
      await view.rerender(controls(r.send, { phase: "running", approvals: [] }));
      await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
      expect(r.sent).toEqual([
        { type: "answer", session: "s", approval: 3, allow: false },
        { type: "prompt", session: "s", text: "do it by hand" },
        { type: "prompt", session: "s", text: "do it by hand" },
      ]);
    });

    it("sends a prompt, not a steer, even while a Codex turn runs", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", engine: "codex", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "stop, do it by hand");
      await submit();
      await flush();
      r.waiting[0]?.resolve();
      await flush();
      expect(r.sent.map((b) => b.type)).toEqual(["answer", "prompt"]);
    });

    it("a refused deny sends no prompt, keeps the words, and the card says why", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      r.waiting[0]?.reject(new Refusal("invalid", "That approval is already closed"));
      await flush();
      expect(r.sent).toHaveLength(1);
      expect(screen.getByText("That approval is already closed")).toBeTruthy();
      expect(input().props.value).toBe("do something else");
    });

    it("before the 400 ms arm, sends nothing at all: neither the deny nor the prompt", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      expect(r.send).not.toHaveBeenCalled();
      expect(input().props.value).toBe("do something else");
    });

    it("a return before the arm says to wait, in muted text, until the arm passes", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      expect(screen.queryByText(JUST_APPEARED)).toBeNull();
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      expect(JUST_APPEARED).toBe("Wait a moment — the approval just appeared");
      const hint = screen.getByText(JUST_APPEARED);
      expect([dark.muted, light.muted]).toContain(StyleSheet.flatten(hint.props.style).color);
      expect(r.send).not.toHaveBeenCalled();
      await settle(400);
      expect(screen.queryByText(JUST_APPEARED)).toBeNull();
      await submit();
      await flush();
      expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 3, allow: false }]);
    });

    it("the note does not come back once the approval has armed, when it is disarmed and armed again", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      expect(screen.getByText(JUST_APPEARED)).toBeTruthy();
      await settle(400);
      expect(screen.queryByText(JUST_APPEARED)).toBeNull();
      // The link drops and comes back: the approval arms again, and until it does nothing is
      // wrong that the user needs telling about; they have not sent anything since.
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3)], offline: true }));
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await flush();
      expect(screen.queryByText(JUST_APPEARED)).toBeNull();
      await settle(400);
      expect(screen.queryByText(JUST_APPEARED)).toBeNull();
    });

    it("a return in that new window says to wait again", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3)], offline: true }));
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      expect(screen.getByText(JUST_APPEARED)).toBeTruthy();
    });

    it("covered (the menu or the drawer is up), sends nothing", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)], covered: true }));
      await settle(1_000);
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("sends no prompt when the link drops while the deny is out", async () => {
      const r = rig();
      const view = await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "do something else");
      await submit();
      await flush();
      await view.rerender(controls(r.send, { phase: "running", approvals: [approval(3)], offline: true }));
      r.waiting[0]?.resolve();
      await flush();
      expect(r.sent.map((b) => b.type)).toEqual(["answer"]);
      expect(input().props.value).toBe("do something else");
    });

    it("a slash command is neither a deny nor a prompt", async () => {
      const r = rig();
      await render(controls(r.send, { phase: "running", approvals: [approval(3)] }));
      await settle(400);
      await fireEvent.changeText(input(), "/compact");
      await submit();
      await flush();
      expect(r.send).not.toHaveBeenCalled();
    });
  });

  describe("a draft (a first prompt the Mac did not take)", () => {
    it("fills the field when it opens, and is taken", async () => {
      octetStore.getState().setDraft("s", "kept words");
      await render(controls(rig().send));
      expect(input().props.value).toBe("kept words");
      expect(octetStore.getState().drafts.has("s")).toBe(false);
    });

    it("that arrives after the line opened goes ahead of what was typed", async () => {
      await render(controls(rig().send));
      await fireEvent.changeText(input(), "and more");
      await act(async () => octetStore.getState().setDraft("s", "first words"));
      expect(input().props.value).toBe("first words\nand more");
      expect(input().props.multiline).toBe(true); // shown on two lines, as typed
      expect(octetStore.getState().drafts.has("s")).toBe(false);
    });

    it("of another session stays where it is", async () => {
      octetStore.getState().setDraft("other", "not mine");
      await render(controls(rig().send));
      expect(input().props.value).toBe("");
      expect(octetStore.getState().drafts.get("other")).toBe("not mine");
    });
  });
});
