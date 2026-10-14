// The slash menu over the prompt line: shown while the text is a `/` word, `/model` (opens the ▾
// menu) and `/mode` (the next mode, as ⇧tab), the top match marked ▸; Tab completes, return runs.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { fireEvent, render, screen, within } from "@testing-library/react-native";
import { StyleSheet } from "react-native";

import { onMenuRequest } from "../convoMenu";
import { saveSettings } from "../persist";
import { octetStore } from "../store";
import { dark, light } from "../theme";
import { controls, disabled, flush, input, key, rig } from "./controlsRig";
import { SLASH_COMMANDS, slashMatches } from "./SlashMenu";

const row = (cmd: string) => screen.getByRole("button", { name: new RegExp(`^${cmd}, `) });
const rows = () => screen.queryAllByRole("button", { name: /^\/mode?l?, / });
const submit = () => fireEvent(input(), "submitEditing");

let unlisten: (() => void) | null = null;
beforeEach(async () => {
  await AsyncStorage.clear();
  await saveSettings({ faceId: false });
  octetStore.getState().reset();
});
afterEach(() => {
  unlisten?.();
  unlisten = null;
});

describe("the slash menu", () => {
  it("offers /model and /mode in Phase A", () => {
    expect(SLASH_COMMANDS.map((c) => [c.cmd, c.desc])).toEqual([
      ["/model", "Switch model"],
      ["/mode", "Permissions"],
    ]);
  });

  it("matches a `/` word with no space, by prefix, an exact match first", () => {
    const cmds = (text: string) => slashMatches(text).map((c) => c.cmd);
    expect(cmds("/")).toEqual(["/model", "/mode"]);
    expect(cmds("/mo")).toEqual(["/model", "/mode"]);
    expect(cmds("/mode")).toEqual(["/mode", "/model"]);
    expect(cmds("/model")).toEqual(["/model"]);
    expect(cmds("/MOD")).toEqual(["/model", "/mode"]);
    expect(cmds("  /mo")).toEqual(["/model", "/mode"]);
    expect(cmds("/mo ")).toEqual([]);
    expect(cmds("/model x")).toEqual([]);
    expect(cmds("/compact")).toEqual([]);
    expect(cmds("/usr/lib")).toEqual([]);
    expect(cmds("model")).toEqual([]);
    expect(cmds("")).toEqual([]);
  });

  it("shows while the text is a `/` word: each row ▸ /cmd, what it does and its value", async () => {
    await render(controls(rig().send, { mode: "accept-edits", model: "Sonnet" }));
    expect(rows()).toHaveLength(0);
    await fireEvent.changeText(input(), "/");
    expect(within(row("/model")).getByText("▸")).toBeTruthy();
    expect(within(row("/model")).getByText("Switch model")).toBeTruthy();
    expect(within(row("/model")).getByText("Sonnet")).toBeTruthy();
    expect(within(row("/mode")).queryByText("▸")).toBeNull();
    expect(within(row("/mode")).getByText("Permissions")).toBeTruthy();
    expect(within(row("/mode")).getByText("accept-edits")).toBeTruthy();
    const top = (StyleSheet.flatten(within(row("/model")).getByText("/model").props.style) as { color?: string }).color;
    expect([dark.accent, light.accent]).toContain(top);
    await fireEvent.changeText(input(), "/mode");
    expect(within(row("/mode")).getByText("▸")).toBeTruthy();
    await fireEvent.changeText(input(), "/mode ");
    expect(rows()).toHaveLength(0);
    await fireEvent.changeText(input(), "fix it");
    expect(rows()).toHaveLength(0);
  });

  it("rows are 44 pt buttons", async () => {
    await render(controls(rig().send));
    await fireEvent.changeText(input(), "/");
    const style = StyleSheet.flatten(row("/model").props.style) as { height?: number; minHeight?: number };
    expect(Math.max(style.height ?? 0, style.minHeight ?? 0)).toBeGreaterThanOrEqual(44);
  });

  it("Tab completes the top match", async () => {
    await render(controls(rig().send));
    await fireEvent.changeText(input(), "/mo");
    await fireEvent(input(), "keyPress", { nativeEvent: { key: "Tab" } });
    expect(input().props.value).toBe("/model");
  });

  it("return runs the top match: /model opens the ▾ menu, and the text clears", async () => {
    const opened = jest.fn();
    unlisten = onMenuRequest("s", opened);
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "/mo");
    await submit();
    expect(opened).toHaveBeenCalledTimes(1);
    expect(input().props.value).toBe("");
    expect(r.send).not.toHaveBeenCalled();
  });

  it("⏎ runs it too, and a tap on a row runs that row", async () => {
    const opened = jest.fn();
    unlisten = onMenuRequest("s", opened);
    const r = rig();
    await render(controls(r.send, { mode: "ask" }));
    await fireEvent.changeText(input(), "/model");
    expect(disabled(key("Run /model (⏎)"))).toBe(false);
    await fireEvent.press(key("Run /model (⏎)"));
    expect(opened).toHaveBeenCalledTimes(1);
    await fireEvent.changeText(input(), "/");
    await fireEvent.press(row("/mode"));
    expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode: "accept-edits" }]);
    expect(input().props.value).toBe("");
  });

  it("/mode cycles the mode like ⇧tab, never to full access", async () => {
    const r = rig();
    await render(controls(r.send, { mode: "auto", modes: ["ask", "accept-edits", "auto", "full-access"] }));
    await fireEvent.changeText(input(), "/mode");
    await submit();
    expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode: "ask" }]);
  });

  it("/model asks only this session's menu to open", async () => {
    const mine = jest.fn();
    const other = jest.fn();
    unlisten = onMenuRequest("s", mine);
    const off = onMenuRequest("t", other);
    try {
      await render(controls(rig().send));
      await fireEvent.changeText(input(), "/model");
      await submit();
      expect(mine).toHaveBeenCalledTimes(1);
      expect(other).not.toHaveBeenCalled();
    } finally {
      off();
    }
  });

  it("other `/` text is not sent, and says where slash commands run", async () => {
    const r = rig();
    await render(controls(r.send));
    await fireEvent.changeText(input(), "/compact");
    expect(rows()).toHaveLength(0);
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    await submit();
    await flush();
    expect(r.send).not.toHaveBeenCalled();
    expect(input().props.value).toBe("/compact");
  });

  it("while the menu shows, the note does not", async () => {
    await render(controls(rig().send));
    await fireEvent.changeText(input(), "/mo");
    expect(screen.queryByText("Slash commands run on the desk")).toBeNull();
  });

  it("offline, a row sends nothing", async () => {
    const r = rig();
    await render(controls(r.send, { offline: true }));
    await fireEvent.changeText(input(), "/mode");
    await fireEvent.press(row("/mode"));
    await submit();
    expect(r.send).not.toHaveBeenCalled();
    expect(input().props.value).toBe("/mode");
  });
});
