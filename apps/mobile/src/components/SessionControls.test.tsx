import { act, fireEvent, render, screen } from "@testing-library/react-native";

import { Refusal, type RequestBody } from "../client/HubClient";
import type { Engine, Phase } from "../protocol";
import { SessionControls } from "./SessionControls";

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
const controls = (phase: Phase, engine: Engine, send: ReturnType<typeof rig>["send"]) => (
  <SessionControls session="s" phase={phase} engine={engine} send={send} />
);
const button = (name: string) => screen.queryByRole("button", { name });

describe("the session controls", () => {
  it("show Cancel only while running", async () => {
    const r = rig();
    const view = await render(controls("running", "claude", r.send));
    expect(button("Cancel")).toBeTruthy();
    for (const phase of ["idle", "connecting", "stopped"] as const) {
      await view.rerender(controls(phase, "claude", r.send));
      expect(button("Cancel")).toBeNull();
    }
  });

  it("cancel sends cancel, shows a pending state, and shows a refusal", async () => {
    const r = rig();
    await render(controls("running", "claude", r.send));
    await fireEvent.press(button("Cancel") as never);
    expect(r.sent).toEqual([{ type: "cancel", session: "s" }]);
    await fireEvent.press(button("Cancel") as never);
    expect(r.send).toHaveBeenCalledTimes(1);
    r.waiting[0]?.reject(new Refusal("not_allowed", "Cancel is not allowed from a phone"));
    await flush();
    expect(screen.getByText("Cancel is not allowed from a phone")).toBeTruthy();
    await fireEvent.press(button("Cancel") as never);
    r.waiting[1]?.resolve();
    await flush();
    expect(screen.queryByText("Cancel is not allowed from a phone")).toBeNull();
  });

  it("show Steer only for Codex while running", async () => {
    const r = rig();
    const view = await render(controls("running", "codex", r.send));
    expect(button("Steer")).toBeTruthy();
    await view.rerender(controls("running", "claude", r.send));
    expect(button("Steer")).toBeNull();
    await view.rerender(controls("idle", "codex", r.send));
    expect(button("Steer")).toBeNull();
  });

  it("steer opens a field, sends the text as steer, and clears it after the ack", async () => {
    const r = rig();
    await render(controls("running", "codex", r.send));
    await fireEvent.press(button("Steer") as never);
    await fireEvent.changeText(screen.getByLabelText("Steer message"), "use the other crate");
    await fireEvent.press(screen.getByRole("button", { name: "Send steer" }));
    expect(r.sent).toEqual([{ type: "steer", session: "s", text: "use the other crate" }]);
    expect(screen.getByLabelText("Steer message").props.value).toBe("use the other crate");
    r.waiting[0]?.resolve();
    await flush();
    expect(button("Send steer")).toBeNull(); // the field closes
  });

  it("steer keeps the text on a refusal, and offers Try again only for an unsent one", async () => {
    const r = rig();
    await render(controls("running", "codex", r.send));
    await fireEvent.press(button("Steer") as never);
    await fireEvent.changeText(screen.getByLabelText("Steer message"), "go left");
    await fireEvent.press(screen.getByRole("button", { name: "Send steer" }));
    r.waiting[0]?.reject(new Refusal("unsent", "Can't reach your Mac"));
    await flush();
    expect(screen.getByText("Can't reach your Mac")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
    expect(r.sent[1]).toEqual({ type: "steer", session: "s", text: "go left" });
    r.waiting[1]?.reject(new Refusal("lost", "The Mac did not answer in time"));
    await flush();
    expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
    expect(button("Try again")).toBeNull();
    expect(screen.getByLabelText("Steer message").props.value).toBe("go left");
  });

  it("steer refuses text over 64 KiB", async () => {
    const r = rig();
    await render(controls("running", "codex", r.send));
    await fireEvent.press(button("Steer") as never);
    await fireEvent.changeText(screen.getByLabelText("Steer message"), "x".repeat(65_537));
    await fireEvent.press(screen.getByRole("button", { name: "Send steer" }));
    expect(screen.getByText("That's too long to send (64 KB max).")).toBeTruthy();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("show End only when idle, and ask first", async () => {
    const r = rig();
    const view = await render(controls("idle", "claude", r.send));
    expect(button("End session")).toBeTruthy();
    expect(button("Cancel")).toBeNull();
    await fireEvent.press(button("End session") as never);
    expect(r.send).not.toHaveBeenCalled();
    expect(screen.getByText("End this session?")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Keep it" }));
    expect(screen.queryByText("End this session?")).toBeNull();
    expect(r.send).not.toHaveBeenCalled();
    await fireEvent.press(button("End session") as never);
    await fireEvent.press(screen.getByRole("button", { name: "Yes, end it" }));
    expect(r.sent).toEqual([{ type: "end", session: "s" }]);
    await view.rerender(controls("running", "claude", r.send));
    expect(button("End session")).toBeNull();
  });

  it("shows the refusal of an end, and the confirmation goes", async () => {
    const r = rig();
    await render(controls("idle", "claude", r.send));
    await fireEvent.press(button("End session") as never);
    await fireEvent.press(screen.getByRole("button", { name: "Yes, end it" }));
    r.waiting[0]?.reject(new Refusal("turn_open", "A turn is still open"));
    await flush();
    expect(screen.getByText("A turn is still open")).toBeTruthy();
    expect(screen.queryByText("End this session?")).toBeNull();
  });

  it("shows nothing when there is nothing to do", async () => {
    await render(controls("stopped", "claude", rig().send));
    expect(screen.queryByRole("button")).toBeNull();
  });
});
