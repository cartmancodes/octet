// The approval toast: "<name> needs approval · View" when an approval newly arrives for a session
// that is not the one on screen, never while the Inbox is up.
import { act, fireEvent, screen } from "@testing-library/react-native";
import { router } from "expo-router";
import { Text } from "react-native";

import { newApproval } from "./components/ApprovalToasts";
import { closeDrawer } from "./drawer";
import type { ApprovalInfo, SessionInfo } from "./protocol";
import { hear, info, launch, pairedStorage, type Rig } from "./screenRig";
import { hideToast, toastStore } from "./toast";

const gist = (id: number): ApprovalInfo => ({
  id,
  summary: { action: "Bash", command: `make test-${id}`, files: [], scope: "this command once", cwd: "/w" },
});
const waiting = (id: string, approvalId: number, extra: Partial<SessionInfo> = {}): SessionInfo =>
  info(id, { approvals: 1, phase: "running", approval: gist(approvalId), ...extra });

const ROUTES = {
  settings: () => <Text>Settings screen</Text>,
  recent: () => <Text>Recent screen</Text>,
  new: () => <Text>New session screen</Text>,
};

beforeEach(async () => {
  await pairedStorage();
  closeDrawer();
  hideToast();
});
afterEach(() => {
  closeDrawer();
  hideToast();
  jest.restoreAllMocks();
});

const flush = () => act(async () => {});
const toastText = () => toastStore.getState().toast?.text ?? null;
/** Opens the app on the Inbox, then moves to `path`. */
async function openAt(path: string, list: SessionInfo[]): Promise<Rig> {
  const rig = await launch("/", ROUTES, list);
  if (path !== "/") {
    await act(async () => router.push(path as never));
    await flush();
  }
  return rig;
}

describe("newApproval", () => {
  const row = (id: string, approvals: number, front?: number) => info(id, { approvals, ...(front === undefined ? {} : { approval: gist(front) }) });

  it("finds a session whose count rose or whose front approval changed", () => {
    expect(newApproval([row("a", 0)], [row("a", 1, 1)])?.id).toBe("a");
    expect(newApproval([row("a", 1, 1)], [row("a", 2, 1)])?.id).toBe("a");
    expect(newApproval([row("a", 1, 1)], [row("a", 1, 2)])?.id).toBe("a");
    expect(newApproval([], [row("a", 1, 1)])?.id).toBe("a");
  });

  it("finds nothing when a count falls or holds, and nothing for no approvals", () => {
    expect(newApproval([row("a", 2, 1)], [row("a", 1, 1)])).toBeNull();
    expect(newApproval([row("a", 1, 1)], [row("a", 1, 1)])).toBeNull();
    expect(newApproval([row("a", 1, 1)], [row("a", 0)])).toBeNull();
    expect(newApproval([row("a", 0)], [row("a", 0)])).toBeNull();
  });

  it("takes the first of several, in the list's order", () => {
    expect(newApproval([row("a", 0), row("b", 0)], [row("a", 1, 1), row("b", 1, 1)])?.id).toBe("a");
  });
});

describe("the approval toast", () => {
  it("shows '<name> needs approval' with View when a session off screen gets an approval", async () => {
    const rig = await openAt("/settings", [info("b")]);
    expect(toastText()).toBeNull();
    await hear(rig, [waiting("b", 1)]);
    expect(screen.getByText("b needs approval")).toBeTruthy();
    expect(screen.getByRole("button", { name: "View" })).toBeTruthy();
  });

  it("View opens that session, and the toast goes", async () => {
    const rig = await openAt("/settings", [info("b")]);
    await hear(rig, [waiting("b", 1)]);
    await fireEvent.press(screen.getByRole("button", { name: "View" }));
    await flush();
    expect(rig.view.getPathname()).toBe("/session/b");
    expect(screen.getByTestId("opened").props.children).toBe("b");
    expect(toastText()).toBeNull();
  });

  it("shows when the count rises or the front approval changes, and not otherwise", async () => {
    const rig = await openAt("/settings", [waiting("b", 1)]);
    expect(toastText()).toBeNull(); // already waiting when the screen opened
    await hear(rig, [waiting("b", 1, { last_activity: 5 })]);
    expect(toastText()).toBeNull(); // the same approval
    await hear(rig, [waiting("b", 2)]);
    expect(toastText()).toBe("b needs approval"); // its front approval changed
    hideToast();
    await hear(rig, [waiting("b", 2, { approvals: 2 })]);
    expect(toastText()).toBe("b needs approval"); // one more waiting
    hideToast();
    await hear(rig, [waiting("b", 2, { approvals: 1 })]);
    expect(toastText()).toBeNull(); // fewer waiting
  });

  it("stays quiet for the approvals already waiting when the phone connects", async () => {
    await openAt("/settings", [waiting("b", 1), waiting("c", 4)]);
    expect(toastText()).toBeNull();
  });

  it("never shows while the Inbox is the current screen", async () => {
    const rig = await openAt("/", [info("b")]);
    await hear(rig, [waiting("b", 1)]);
    expect(toastText()).toBeNull();
  });

  it("does not show for the session on screen, but does for another", async () => {
    const rig = await openAt("/session/a", [info("a"), info("b")]);
    await hear(rig, [waiting("a", 1), info("b")]);
    expect(toastText()).toBeNull();
    await hear(rig, [waiting("a", 1), waiting("b", 7)]);
    expect(toastText()).toBe("b needs approval");
  });

  it("shows from the New session and Recent screens too", async () => {
    const rig = await openAt("/new", [info("b")]);
    await hear(rig, [waiting("b", 1)]);
    expect(toastText()).toBe("b needs approval");
  });

  it("shows hub text as plain text, never as a link", async () => {
    const rig = await openAt("/settings", [info("b")]);
    await hear(rig, [waiting("b", 1, { workspace: "/w/<b>[go](evil.test)" })]);
    const shown = screen.getByText("<b>[go](evil.test) needs approval");
    expect(shown.props.onPress).toBeUndefined();
  });
});
