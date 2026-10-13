import AsyncStorage from "@react-native-async-storage/async-storage";
import { act, fireEvent, render, screen } from "@testing-library/react-native";
import * as Haptics from "expo-haptics";
import * as LocalAuthentication from "expo-local-authentication";
import { StyleSheet } from "react-native";

import { Refusal } from "../client/HubClient";
import type { PendingApproval } from "../model/session";
import { saveSettings } from "../persist";
import type { RequestBody } from "../client/HubClient";
import { dark, light } from "../theme";
import { ApprovalSheet } from "./ApprovalSheet";

const approval = (id: number, extra: Partial<PendingApproval["summary"]> = {}, detail = ""): PendingApproval => ({
  id,
  detail,
  summary: {
    action: `Run command ${id}`,
    command: `make test-${id}`,
    files: [],
    cwd: null,
    scope: "this command once",
    ...extra,
  },
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

const flush = () => act(async () => {});
const settle = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));
const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

beforeEach(async () => {
  jest.useFakeTimers();
  await AsyncStorage.clear();
  await saveSettings({ faceId: false });
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  jest.mocked(Haptics.notificationAsync).mockClear();
  auth.authenticateAsync.mockClear();
});
afterEach(() => {
  jest.useRealTimers();
});

const allow = () => screen.getByRole("button", { name: "Allow" });
const deny = () => screen.getByRole("button", { name: "Deny" });
const disabled = (node: { props: { accessibilityState?: { disabled?: boolean }; "aria-disabled"?: boolean } }) =>
  node.props.accessibilityState?.disabled === true || node.props["aria-disabled"] === true;

describe("approvals_queue_and_close_elsewhere", () => {
  it("leads with the action, the command, five files then the rest, the folder and what it allows", async () => {
    const files = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs", "g.rs"];
    const a = approval(7, {
      action: "Edit files",
      command: "cargo fmt",
      files,
      cwd: "/Users/me/octet",
      scope: "these files, once",
    });
    await render(<ApprovalSheet session="s" approvals={[a]} send={rig().send} />);
    expect(screen.getByText("Edit files")).toBeTruthy();
    expect(screen.getByText("$ cargo fmt")).toBeTruthy();
    for (const f of files.slice(0, 5)) expect(screen.getByText(f)).toBeTruthy();
    expect(screen.queryByText("f.rs")).toBeNull();
    expect(screen.getByText("+2 more")).toBeTruthy();
    expect(screen.getByText("in /Users/me/octet")).toBeTruthy();
    expect(screen.getByText("Allows: these files, once")).toBeTruthy();
  });

  it("leaves out what the request does not have, and draws hub text as text", async () => {
    const a = approval(1, { action: "<b>rm -rf</b> [x](http://evil)", command: null, files: [], cwd: null });
    await render(<ApprovalSheet session="s" approvals={[a]} send={rig().send} />);
    expect(screen.getByText("<b>rm -rf</b> [x](http://evil)")).toBeTruthy();
    expect(screen.queryByText(/^\$/)).toBeNull();
    expect(screen.queryByText(/^in /)).toBeNull();
    expect(screen.queryByText(/more$/)).toBeNull();
  });

  it("shows the full request on demand, only when there is one", async () => {
    const view = await render(
      <ApprovalSheet session="s" approvals={[approval(1, {}, "FULL REQUEST BODY")]} send={rig().send} />,
    );
    expect(screen.queryByText("FULL REQUEST BODY")).toBeNull();
    await fireEvent.press(screen.getByRole("button", { name: "Show full request" }));
    expect(screen.getByText("FULL REQUEST BODY")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Hide full request" }));
    expect(screen.queryByText("FULL REQUEST BODY")).toBeNull();
    await view.rerender(<ApprovalSheet session="s" approvals={[approval(1)]} send={rig().send} />);
    expect(screen.queryByRole("button", { name: /full request/ })).toBeNull();
  });

  it("is amber, the permission colour", async () => {
    await render(<ApprovalSheet session="s" approvals={[approval(1)]} send={rig().send} />);
    const colour = (StyleSheet.flatten(screen.getByText("Run command 1").props.style) as { color?: string }).color;
    expect([dark.amber, light.amber]).toContain(colour);
  });

  it("shows nothing without an approval", async () => {
    await render(<ApprovalSheet session="s" approvals={[]} send={rig().send} />);
    expect(screen.queryByRole("button", { name: "Allow" })).toBeNull();
  });

  it("queues: answering one shows the next, and says how many wait", async () => {
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(1), approval(2)]} send={r.send} />);
    expect(screen.getByText("Run command 1")).toBeTruthy();
    expect(screen.queryByText("Run command 2")).toBeNull();
    expect(screen.getByText("1 more waiting")).toBeTruthy();
    await settle(400);
    await fireEvent.press(deny());
    r.waiting[0]?.resolve();
    await flush();
    expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 1, allow: false }]);
    expect(screen.getByText("Run command 2")).toBeTruthy();
    expect(screen.queryByText("Run command 1")).toBeNull();
    expect(screen.queryByText(/more waiting/)).toBeNull();
  });

  it("drops an approval another device closed, and the next one takes its place", async () => {
    const r = rig();
    const view = await render(<ApprovalSheet session="s" approvals={[approval(1), approval(2)]} send={r.send} />);
    await view.rerender(<ApprovalSheet session="s" approvals={[approval(2)]} send={r.send} />);
    expect(screen.queryByText("Run command 1")).toBeNull();
    expect(screen.getByText("Run command 2")).toBeTruthy();
    await view.rerender(<ApprovalSheet session="s" approvals={[]} send={r.send} />);
    expect(screen.queryByText("Run command 2")).toBeNull();
    expect(r.sent).toEqual([]);
  });

  it("keeps Allow disabled for 400 ms after each approval comes to the front, Deny not", async () => {
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(1), approval(2)]} send={r.send} />);
    expect(disabled(allow())).toBe(true);
    expect(disabled(deny())).toBe(false);
    await fireEvent.press(allow());
    expect(r.send).not.toHaveBeenCalled();
    await settle(399);
    expect(disabled(allow())).toBe(true);
    await settle(1);
    expect(disabled(allow())).toBe(false);
    // the next one re-arms
    await fireEvent.press(allow());
    r.waiting[0]?.resolve();
    await flush();
    expect(screen.getByText("Run command 2")).toBeTruthy();
    expect(disabled(allow())).toBe(true);
    await settle(400);
    expect(disabled(allow())).toBe(false);
  });

  it("asks for Face ID before Allow when the setting is on, and sends after it", async () => {
    await saveSettings({ faceId: true });
    const r = rig();
    await render(
      <ApprovalSheet session="s" approvals={[approval(4, { action: "Delete the build folder" })]} send={r.send} />,
    );
    await settle(400);
    await fireEvent.press(allow());
    await flush();
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(auth.authenticateAsync.mock.calls[0]?.[0]?.promptMessage).toContain("Delete the build folder");
    expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 4, allow: true }]);
  });

  it("sends nothing when Face ID fails or is cancelled, and lets the user try again", async () => {
    await saveSettings({ faceId: true });
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(4)]} send={r.send} />);
    await settle(400);
    await fireEvent.press(allow());
    await flush();
    expect(r.send).not.toHaveBeenCalled();
    expect(disabled(allow())).toBe(false);
    await fireEvent.press(allow());
    await flush();
    expect(r.sent).toHaveLength(1);
  });

  it("refuses, and sends nothing, when Face ID is on but this phone has none set up", async () => {
    await saveSettings({ faceId: true });
    auth.isEnrolledAsync.mockResolvedValueOnce(false);
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(4)]} send={r.send} />);
    await settle(400);
    await fireEvent.press(allow());
    await flush();
    expect(
      screen.getByText("Face ID isn't set up on this phone. Turn it off in Settings to allow from here."),
    ).toBeTruthy();
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(r.send).not.toHaveBeenCalled();
  });

  it("never asks for Face ID to deny", async () => {
    await saveSettings({ faceId: true });
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(4)]} send={r.send} />);
    await fireEvent.press(deny());
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 4, allow: false }]);
  });

  it("does not ask for Face ID to allow when the setting is off", async () => {
    await saveSettings({ faceId: false });
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(5)]} send={r.send} />);
    await settle(400);
    await fireEvent.press(allow());
    await flush();
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(r.sent).toEqual([{ type: "answer", session: "s", approval: 5, allow: true }]);
  });

  it("shows a pending state until the ack, sends once, then taps success for an allow", async () => {
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(1)]} send={r.send} />);
    await settle(400);
    await fireEvent.press(allow());
    await flush();
    expect(screen.getByText("Sending…")).toBeTruthy();
    expect(disabled(allow())).toBe(true);
    expect(disabled(deny())).toBe(true);
    await fireEvent.press(allow());
    expect(r.send).toHaveBeenCalledTimes(1);
    expect(Haptics.notificationAsync).not.toHaveBeenCalled();
    r.waiting[0]?.resolve();
    await flush();
    expect(Haptics.notificationAsync).toHaveBeenCalledWith("success");
    expect(screen.queryByText("Sending…")).toBeNull();
  });

  it("does not tap success for a deny", async () => {
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(1)]} send={r.send} />);
    await fireEvent.press(deny());
    r.waiting[0]?.resolve();
    await flush();
    expect(Haptics.notificationAsync).not.toHaveBeenCalled();
  });

  it("shows the hub's words on a refusal and lets the user answer again", async () => {
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(1)]} send={r.send} />);
    await fireEvent.press(deny());
    r.waiting[0]?.reject(new Refusal("invalid", "That approval is already closed"));
    await flush();
    expect(screen.getByText("That approval is already closed")).toBeTruthy();
    expect(screen.queryByText("Sending…")).toBeNull();
    expect(disabled(deny())).toBe(false);
    expect(Haptics.notificationAsync).not.toHaveBeenCalled();
  });

  it("says a lost answer may not have reached the Mac, and offers no resend", async () => {
    const r = rig();
    await render(<ApprovalSheet session="s" approvals={[approval(1)]} send={r.send} />);
    await fireEvent.press(deny());
    r.waiting[0]?.reject(new Refusal("lost", "The Mac did not answer in time"));
    await flush();
    expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
    expect(screen.queryByText("The Mac did not answer in time")).toBeNull();
    expect(screen.queryByRole("button", { name: /try again/i })).toBeNull();
  });

  it("clears an old refusal when the next approval comes", async () => {
    const r = rig();
    const view = await render(<ApprovalSheet session="s" approvals={[approval(1), approval(2)]} send={r.send} />);
    await fireEvent.press(deny());
    r.waiting[0]?.reject(new Refusal("busy", "The Mac is busy"));
    await flush();
    expect(screen.getByText("The Mac is busy")).toBeTruthy();
    await view.rerender(<ApprovalSheet session="s" approvals={[approval(2)]} send={r.send} />);
    expect(screen.queryByText("The Mac is busy")).toBeNull();
  });
});
