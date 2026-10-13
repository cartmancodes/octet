import { act, fireEvent, render, screen } from "@testing-library/react-native";
import * as Haptics from "expo-haptics";
import { StyleSheet } from "react-native";

import { Refusal, type RequestBody } from "../client/HubClient";
import { dark, light } from "../theme";
import { Composer } from "./Composer";
import { APPROVAL_DEMO, isSlash } from "./send";

function rig() {
  const sent: RequestBody[] = [];
  const waiting: { resolve: () => void; reject: (e: unknown) => void }[] = [];
  const send = jest.fn((body: RequestBody) => {
    sent.push(body);
    return new Promise<unknown>((resolve, reject) => waiting.push({ resolve: () => resolve({}), reject }));
  });
  return { sent, send, waiting };
}
const flush = () => act(async () => {});
const input = () => screen.getByLabelText("Message");
const sendButton = (name = "Send") => screen.getByRole("button", { name });
const disabled = (node: { props: { accessibilityState?: { disabled?: boolean }; "aria-disabled"?: boolean } }) =>
  node.props.accessibilityState?.disabled === true || node.props["aria-disabled"] === true;

beforeEach(() => jest.mocked(Haptics.impactAsync).mockClear());

describe("the composer", () => {
  it("says Send when idle, and Queue with the count while running", async () => {
    const view = await render(<Composer session="s" phase="idle" queued={0} send={rig().send} />);
    expect(screen.getByText("Send")).toBeTruthy();
    expect(screen.queryByText(/Queue/)).toBeNull();
    await view.rerender(<Composer session="s" phase="running" queued={0} send={rig().send} />);
    expect(screen.getByText("Queue")).toBeTruthy();
    await view.rerender(<Composer session="s" phase="running" queued={3} send={rig().send} />);
    expect(screen.getByText("Queue · 3 queued")).toBeTruthy();
    expect(sendButton("Queue")).toBeTruthy();
  });

  it("cannot send empty or blank text", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    expect(disabled(sendButton())).toBe(true);
    await fireEvent.press(sendButton());
    await fireEvent.changeText(input(), "  \n ");
    expect(disabled(sendButton())).toBe(true);
    await fireEvent.press(sendButton());
    expect(r.send).not.toHaveBeenCalled();
  });

  it("does not send a slash command, and says where they run", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "/compact");
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    expect(disabled(sendButton())).toBe(true);
    await fireEvent.press(sendButton());
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
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "  /help");
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    expect(disabled(sendButton())).toBe(true);
    await fireEvent.changeText(input(), "/model x");
    expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
    expect(disabled(sendButton())).toBe(true);
    await fireEvent.press(sendButton());
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.changeText(input(), "/usr/lib is broken");
    expect(screen.queryByText("Slash commands run on the desk")).toBeNull();
    await fireEvent.press(sendButton());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "/usr/lib is broken" }]);
  });

  it("sends the prompt with a light tap, and clears the text only after the ack", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "fix the hub");
    await fireEvent.press(sendButton());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "fix the hub" }]);
    expect(Haptics.impactAsync).toHaveBeenCalledWith("light");
    expect(input().props.value).toBe("fix the hub"); // not before the ack
    expect(disabled(sendButton())).toBe(true); // one send at a time
    await fireEvent.press(sendButton());
    expect(r.send).toHaveBeenCalledTimes(1);
    r.waiting[0]?.resolve();
    await flush();
    expect(input().props.value).toBe("");
  });

  it("offers Try again for an unsent prompt, keeping the text", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "fix the hub");
    await fireEvent.press(sendButton());
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

  it("never offers a resend of a lost prompt, and keeps the text", async () => {
    const r = rig();
    await render(<Composer session="s" phase="running" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "fix the hub");
    await fireEvent.press(sendButton("Queue"));
    r.waiting[0]?.reject(new Refusal("lost", "The Mac did not answer in time"));
    await flush();
    expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(input().props.value).toBe("fix the hub");
  });

  it("shows any other refusal in the hub's words, without Try again", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "hi");
    await fireEvent.press(sendButton());
    r.waiting[0]?.reject(new Refusal("busy", "A turn is already open <b>here</b>"));
    await flush();
    expect(screen.getByText("A turn is already open <b>here</b>")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(input().props.value).toBe("hi");
  });

  it("refuses text over 64 KiB as UTF-8 bytes, and sends exactly 64 KiB", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "é".repeat(32_769)); // 32,769 characters, 65,538 bytes
    await fireEvent.press(sendButton());
    expect(screen.getByText("That's too long to send (64 KB max).")).toBeTruthy();
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.changeText(input(), "é".repeat(32_768)); // exactly 65,536 bytes
    expect(screen.queryByText("That's too long to send (64 KB max).")).toBeNull();
    await fireEvent.press(sendButton());
    expect(r.send).toHaveBeenCalledTimes(1);
  });

  it("keeps what the user typed while the send was out", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "first");
    await fireEvent.press(sendButton());
    await fireEvent.changeText(input(), "first, and more");
    r.waiting[0]?.resolve();
    await flush();
    expect(input().props.value).toBe("first, and more");
  });

  it("draws a refusal in the text colour, not amber, which is the approval sheet's", async () => {
    const r = rig();
    await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "hi");
    await fireEvent.press(sendButton());
    r.waiting[0]?.reject(new Refusal("busy", "The Mac is busy"));
    await flush();
    const colour = (StyleSheet.flatten(screen.getByText("The Mac is busy").props.style) as { color?: string }).color;
    expect([dark.fg, light.fg]).toContain(colour);
  });

  it("offline, keeps the words but cannot send them, until the Mac is back", async () => {
    const r = rig();
    const view = await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "fix the hub");
    await view.rerender(<Composer session="s" phase="idle" queued={0} send={r.send} offline />);
    expect(input().props.value).toBe("fix the hub");
    expect(disabled(sendButton())).toBe(true);
    await fireEvent.press(sendButton());
    expect(r.send).not.toHaveBeenCalled();
    await view.rerender(<Composer session="s" phase="idle" queued={0} send={r.send} offline={false} />);
    expect(input().props.value).toBe("fix the hub");
    expect(disabled(sendButton())).toBe(false);
    await fireEvent.press(sendButton());
    expect(r.sent).toEqual([{ type: "prompt", session: "s", text: "fix the hub" }]);
  });

  it("offline, Try again sends nothing", async () => {
    const r = rig();
    const view = await render(<Composer session="s" phase="idle" queued={0} send={r.send} />);
    await fireEvent.changeText(input(), "hi");
    await fireEvent.press(sendButton());
    r.waiting[0]?.reject(new Refusal("unsent", "Can't reach your Mac"));
    await flush();
    await view.rerender(<Composer session="s" phase="idle" queued={0} send={r.send} offline />);
    const retry = screen.getByRole("button", { name: "Try again" });
    expect(disabled(retry)).toBe(true);
    await fireEvent.press(retry);
    expect(r.send).toHaveBeenCalledTimes(1);
  });
});
