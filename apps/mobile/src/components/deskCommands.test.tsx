// /new, /session, /export and /help from the prompt line: each goes to the Mac as a `command`
// request, and its ack is shown the way the brief rules: /new confirms first, /session and /help
// open the ack's text in a sheet, /export toasts it. Each row is off while offline, while an
// approval waits, and while a command is out; the words clear only when a command ran.
import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, render, screen, within } from "@testing-library/react-native";
import { StyleSheet } from "react-native";

import { Refusal } from "../client/HubClient";
import { saveSettings } from "../persist";
import { octetStore } from "../store";
import { toastStore, hideToast } from "../toast";
import { approval, controls, disabled, flush, input, rig, settle } from "./controlsRig";
import { FRESH_QUESTION } from "./CommandSheet";
import { FRESH_NOTE } from "./deskCommand";

const row = (cmd: string) => screen.getByRole("button", { name: new RegExp(`^${cmd}, `) });
const submit = () => fireEvent(input(), "submitEditing");
const button = (name: string) => screen.queryByRole("button", { name });
const type = (text: string) => fireEvent.changeText(input(), text);
const toastText = () => toastStore.getState().toast?.text ?? null;

beforeEach(async () => {
  await AsyncStorage.clear();
  await saveSettings({ faceId: false });
  octetStore.getState().reset();
  hideToast();
});
afterEach(() => hideToast());

describe("the six slash rows", () => {
  it("lists /model, /mode, /new, /session, /export and /help for `/`", async () => {
    await render(controls(rig().send));
    await type("/");
    for (const cmd of ["/model", "/mode", "/new", "/session", "/export", "/help"]) expect(row(cmd)).toBeTruthy();
    expect(within(row("/new")).getByText("Fresh context")).toBeTruthy();
    expect(within(row("/session")).getByText("Session info")).toBeTruthy();
    expect(within(row("/export")).getByText("Save journal")).toBeTruthy();
    expect(within(row("/help")).getByText("List commands")).toBeTruthy();
  });

  it("each is a 44 pt button", async () => {
    await render(controls(rig().send));
    await type("/");
    for (const cmd of ["/new", "/session", "/export", "/help"]) {
      const style = StyleSheet.flatten(row(cmd).props.style) as { height?: number; minHeight?: number };
      expect(Math.max(style.height ?? 0, style.minHeight ?? 0)).toBeGreaterThanOrEqual(44);
    }
  });
});

describe("/new", () => {
  it("asks first, with Cancel and Start fresh, and sends nothing until Start fresh", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/new");
    await fireEvent.press(row("/new"));
    expect(screen.getByText(FRESH_QUESTION)).toBeTruthy();
    expect(FRESH_QUESTION).toBe("Start a fresh conversation? The current one stays in the journal.");
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.press(button("Cancel") as never);
    expect(screen.queryByText(FRESH_QUESTION)).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
    expect(input().props.value).toBe("/new");
    await fireEvent.press(row("/new"));
    await fireEvent.press(button("Start fresh") as never);
    expect(r.sent).toEqual([{ type: "command", session: "s", name: "new" }]);
    // The words clear when the Mac took it.
    expect(input().props.value).toBe("/new");
    r.waiting[0]?.resolve({ message: "Starting a fresh conversation" });
    await flush();
    expect(input().props.value).toBe("");
    expect(screen.queryByText(FRESH_QUESTION)).toBeNull();
  });

  it("return runs it too, through the same ask", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/new");
    await submit();
    expect(screen.getByText(FRESH_QUESTION)).toBeTruthy();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("a refusal shows the desk's words and keeps the text", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/new");
    await fireEvent.press(row("/new"));
    await fireEvent.press(button("Start fresh") as never);
    r.waiting[0]?.reject(new Refusal("turn_open", "A turn is running; wait or cancel it first"));
    await flush();
    expect(screen.getByText("A turn is running; wait or cancel it first")).toBeTruthy();
    expect(input().props.value).toBe("/new");
    expect(button("Try again")).toBeNull();
  });
});

describe("/new marks the fresh start", () => {
  const hold = () =>
    octetStore.getState().applyBatch([
      { v: 1, type: "subscribed", session: "s", from: "start", journals: ["j1"] },
      { v: 1, type: "event", session: "s", journal: "j1", seq: 1, event: { type: "user", data: "old question" } },
      { v: 1, type: "replayed", session: "s" } as never, // the replay is over
    ]);
  const notes = () => (octetStore.getState().bySession.get("s")?.transcript.entries ?? []).filter((e) => e.kind === "note");

  it("adds one divider to the session's transcript on the ack, and new events land below it", async () => {
    expect(FRESH_NOTE).toBe("Fresh conversation — the earlier one is above.");
    hold();
    const r = rig();
    await render(controls(r.send));
    await type("/new");
    await fireEvent.press(row("/new"));
    await fireEvent.press(button("Start fresh") as never);
    expect(notes()).toHaveLength(0); // nothing until the Mac took it
    r.waiting[0]?.resolve({ message: "Starting a fresh conversation" });
    await flush();
    expect(notes()).toEqual([expect.objectContaining({ text: FRESH_NOTE })]);
    octetStore.getState().applyBatch([{ v: 1, type: "event", session: "s", journal: "j2", seq: 1, event: { type: "user", data: "new question" } }]);
    const kinds = (octetStore.getState().bySession.get("s")?.transcript.entries ?? []).map((e) => ("text" in e ? e.text : ""));
    expect(kinds).toEqual(["old question", FRESH_NOTE, "new question"]);
  });

  it("a refused /new, and the other commands, add none", async () => {
    hold();
    const r = rig();
    await render(controls(r.send));
    await type("/new");
    await fireEvent.press(row("/new"));
    await fireEvent.press(button("Start fresh") as never);
    r.waiting[0]?.reject(new Refusal("turn_open", "A turn is running"));
    await flush();
    await type("/export");
    await submit();
    r.waiting[1]?.resolve({ message: "Exported" });
    await flush();
    expect(notes()).toHaveLength(0);
  });
});

describe("the /new ask for a screen reader", () => {
  it("has its question as a header, and the sheet is modal", async () => {
    await render(controls(rig().send));
    await type("/new");
    await fireEvent.press(row("/new"));
    expect(screen.getByRole("header", { name: FRESH_QUESTION })).toBeTruthy();
    expect(within(screen.getByTestId("fresh-confirm")).getByTestId("fresh-confirm-sheet").props.accessibilityViewIsModal).toBe(true);
  });
});

describe("/session and /help", () => {
  it("/session sends the command and opens the ack's text in a selectable monospace sheet", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/session");
    await fireEvent.press(row("/session"));
    expect(r.sent).toEqual([{ type: "command", session: "s", name: "session" }]);
    expect(screen.queryByTestId("command-sheet")).toBeNull();
    r.waiting[0]?.resolve({ message: "Session s1\nclaude / Sonnet\n3 turns" });
    await flush();
    const sheet = within(screen.getByTestId("command-sheet"));
    const text = sheet.getByText("Session s1\nclaude / Sonnet\n3 turns");
    expect(text.props.selectable).toBe(true);
    const style = StyleSheet.flatten(text.props.style) as { fontFamily?: string };
    expect(style.fontFamily).toMatch(/Menlo|monospace/);
    expect(input().props.value).toBe("");
    await fireEvent.press(sheet.getByRole("button", { name: "Close" }));
    expect(screen.queryByTestId("command-sheet")).toBeNull();
  });

  it("/help opens the hub's list the same way, by return", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/help");
    await submit();
    expect(r.sent).toEqual([{ type: "command", session: "s", name: "help" }]);
    r.waiting[0]?.resolve({ message: "Commands from the phone: /model · /mode · /new · /session · /export · /help." });
    await flush();
    expect(within(screen.getByTestId("command-sheet")).getByText(/^Commands from the phone: /)).toBeTruthy();
  });

  it("renders hub text as plain text: escapes and controls are cleaned, markup is just letters", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/session");
    await submit();
    r.waiting[0]?.resolve({ message: "\u001b[31mred\u001b[0m <b>bold</b> [x](http://evil.example) \u0007end" });
    await flush();
    const sheet = within(screen.getByTestId("command-sheet"));
    expect(sheet.getByText("red <b>bold</b> [x](http://evil.example) end")).toBeTruthy();
    expect(sheet.queryByRole("link")).toBeNull();
  });

  it("a refusal shows its words and opens no sheet", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/session");
    await submit();
    r.waiting[0]?.reject(new Refusal("not_connected", "The desk is not connected"));
    await flush();
    expect(screen.getByText("The desk is not connected")).toBeTruthy();
    expect(screen.queryByTestId("command-sheet")).toBeNull();
    expect(input().props.value).toBe("/session");
  });

  it("an ack with no text still opens the sheet, saying so", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/help");
    await submit();
    r.waiting[0]?.resolve({});
    await flush();
    expect(within(screen.getByTestId("command-sheet")).getByText("The desk sent no details")).toBeTruthy();
  });
});

describe("/export", () => {
  it("sends the command and shows the ack as a toast", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/export");
    await fireEvent.press(row("/export"));
    expect(r.sent).toEqual([{ type: "command", session: "s", name: "export" }]);
    expect(toastText()).toBeNull();
    r.waiting[0]?.resolve({ message: "Exported the journal to /Users/me/w/j1.jsonl" });
    await flush();
    expect(toastText()).toBe("Exported the journal to /Users/me/w/j1.jsonl");
    expect(screen.queryByTestId("command-sheet")).toBeNull();
    expect(input().props.value).toBe("");
  });

  it("a refusal (busy, or a failed export) shows its words in the line, not a toast", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/export");
    await submit();
    r.waiting[0]?.reject(new Refusal("busy", "Wait for: a shell command"));
    await flush();
    expect(screen.getByText("Wait for: a shell command")).toBeTruthy();
    expect(toastText()).toBeNull();
    expect(input().props.value).toBe("/export");
  });
});

describe("the words", () => {
  it("clear on the ack only if they are still what was run; words typed meanwhile are kept", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/export");
    await submit();
    await type("and then fix the hub");
    r.waiting[0]?.resolve({ message: "done" });
    await flush();
    expect(input().props.value).toBe("and then fix the hub");
  });
});

describe("the rows are off", () => {
  const NEW_ROWS = ["/new", "/session", "/export", "/help"];

  it("offline: they send nothing and keep the words", async () => {
    const r = rig();
    await render(controls(r.send, { offline: true }));
    await type("/");
    for (const cmd of NEW_ROWS) {
      expect(disabled(row(cmd))).toBe(true);
      await fireEvent.press(row(cmd));
    }
    await type("/help");
    await submit();
    await type("/new");
    await submit();
    expect(r.send).not.toHaveBeenCalled();
    expect(screen.queryByText(FRESH_QUESTION)).toBeNull();
    expect(input().props.value).toBe("/new");
  });

  it("while an approval waits: they send nothing and keep the words", async () => {
    const r = rig();
    await render(controls(r.send, { approvals: [approval(1)] }));
    await type("/");
    for (const cmd of NEW_ROWS) {
      expect(disabled(row(cmd))).toBe(true);
      await fireEvent.press(row(cmd));
    }
    await type("/session");
    await submit();
    await type("/new");
    await submit();
    expect(r.send).not.toHaveBeenCalled();
    expect(screen.queryByText(FRESH_QUESTION)).toBeNull();
    expect(input().props.value).toBe("/new");
  });

  it("while a command is out: the others wait, and run again once it is answered", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("/export");
    await submit();
    await type("/");
    for (const cmd of NEW_ROWS) expect(disabled(row(cmd))).toBe(true);
    await fireEvent.press(row("/help"));
    await type("/session");
    await submit();
    expect(r.sent.map((b) => (b as { name?: string }).name)).toEqual(["export"]);
    expect(input().props.value).toBe("/session");
    r.waiting[0]?.resolve({ message: "done" });
    await flush();
    await type("/");
    expect(disabled(row("/help"))).toBe(false);
  });

  it("while a prompt is out, the new rows wait too", async () => {
    const r = rig();
    await render(controls(r.send));
    await type("fix it");
    await submit();
    await type("/help");
    expect(disabled(row("/help"))).toBe(true);
  });

  it("Start fresh offline sends nothing", async () => {
    const r = rig();
    const view = await render(controls(r.send));
    await type("/new");
    await fireEvent.press(row("/new"));
    await view.rerender(controls(r.send, { offline: true }));
    await fireEvent.press(button("Start fresh") as never);
    expect(r.send).not.toHaveBeenCalled();
  });
});

describe("a sheet over an approval", () => {
  it("covers it: an approval that comes while the sheet is up cannot be answered until it closes and re-arms", async () => {
    jest.useFakeTimers();
    try {
      const r = rig();
      const view = await render(controls(r.send));
      await type("/session");
      await submit();
      r.waiting[0]?.resolve({ message: "Session s" });
      await flush();
      await view.rerender(controls(r.send, { approvals: [approval(1)] }));
      await settle(1_000);
      expect(disabled(screen.getByRole("button", { name: "Allow" }))).toBe(true);
      await fireEvent.press(within(screen.getByTestId("command-sheet")).getByRole("button", { name: "Close" }));
      await settle(450);
      expect(disabled(screen.getByRole("button", { name: "Allow" }))).toBe(false);
    } finally {
      jest.useRealTimers();
    }
  });
});
