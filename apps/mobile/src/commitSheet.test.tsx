// Commit and "Open PR on the desk" on the Changes screen: the commit sheet (prefilled from the
// session's first prompt, Face ID always, one at a time, a refusal kept in the sheet) and the PR
// sheet, which only says what to run at the Mac and sends nothing.
import { measureParentSize } from "@shopify/flash-list/dist/recyclerview/utils/measureLayout";
import { act, fireEvent, render, screen, within } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { router } from "expo-router";
import { Profiler } from "react";
import { StyleSheet } from "react-native";

import Changes from "../app/changes";
import { REQUEST_TIMEOUT_MS, type BatchItem } from "./client/HubClient";
import { CommitSheet } from "./components/CommitSheet";
import { LOST } from "./components/send";
import { saveSettings } from "./persist";
import type { FileChange, JournalEvent, SessionInfo } from "./protocol";
import { info, launch, pairedStorage, type Rig } from "./screenRig";
import { octetStore } from "./store";
import { applyM, k } from "./testKeys";
import { MONO, dark, light, type Theme } from "./theme";
import { hideToast, toastStore } from "./toast";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;
const SESSION = "a1b2c3d4";
const A = (extra: Partial<SessionInfo> = {}) => info(SESSION, { workspace: "/Users/me/Projects/octet", ...extra });

beforeEach(async () => {
  await pairedStorage();
  hideToast();
  auth.authenticateAsync.mockReset();
  auth.authenticateAsync.mockResolvedValue({ success: true });
  (measureParentSize as jest.Mock).mockReturnValue({ x: 0, y: 0, width: 400, height: 5000 });
});
afterEach(() => jest.restoreAllMocks());

const flush = () => act(async () => void jest.advanceTimersByTime(60));
const tones = (role: keyof Theme) => [dark[role], light[role]];
type Look = { color?: unknown; backgroundColor?: unknown; borderColor?: unknown; fontWeight?: unknown; fontFamily?: unknown; minHeight?: number; minWidth?: number };
const look = (node: { props: { style?: unknown } }) => StyleSheet.flatten(node.props.style as never) as Look;

const file = (path: string, add = 3, del = 1, status: FileChange["status"] = "M"): FileChange => ({ path, add, del, binary: false, status });
const FILES = [file("src/app.ts"), file("README.md", 20, 0)];

async function open(list: SessionInfo[] = [A()]) {
  const rig = await launch("/", { changes: Changes }, list);
  await act(async () => router.push({ pathname: "/changes", params: { id: k(SESSION) } }));
  await flush();
  return rig;
}
const requests = (rig: Rig, type: string) => rig.sockets.last.commandsOf(type);
const lastReq = (rig: Rig, type: string) => Number(requests(rig, type).at(-1)?.req);

async function answer(rig: Rig, extra: object = {}) {
  await act(async () => {
    rig.sockets.last.receive({ type: "changes", req: lastReq(rig, "changes"), branch: "main", upstream: "origin/main", ahead: 0, behind: 0, files: FILES, more: 0, ...extra });
  });
  await flush();
}
async function ack(rig: Rig, message: string) {
  await act(async () => {
    rig.sockets.last.receive({ type: "ack", req: lastReq(rig, "commit"), message });
  });
  await flush();
}
async function refuse(rig: Rig, reason: string, code: string) {
  await act(async () => {
    rig.sockets.last.receive({ type: "refused", req: lastReq(rig, "commit"), reason, code });
  });
  await flush();
}

let seq = 0;
const ev = (event: JournalEvent) => ({ type: "event", session: SESSION, journal: "a-1.jsonl", seq: ++seq, event });
const user = (text: string) => ev({ type: "user", data: text });
const approval = (id: number) =>
  ev({ type: "approval", data: { id, detail: "", summary: { action: "Run tests", command: null, files: [], cwd: null, scope: "once" } } });
async function feed(...frames: object[]) {
  await act(async () =>
    void applyM(octetStore, [{ type: "subscribed", session: SESSION, from: "start", journals: ["a-1.jsonl"] }, ...frames].map((f) => ({ v: 1, ...f })) as BatchItem[]),
  );
}

const actions = () => within(screen.getByTestId("changes-actions"));
const commitButton = () => actions().getByRole("button", { name: "Commit" });
const prButton = () => actions().getByRole("button", { name: "Open PR on the desk" });
const sheet = () => within(screen.getByTestId("commit-sheet-body"));
const field = () => sheet().getByLabelText("Commit message");
const confirm = () => sheet().getByRole("button", { name: "Commit the changes" });
async function press(node: Parameters<typeof fireEvent.press>[0]) {
  await fireEvent.press(node);
  await flush();
}
async function openSheet(rig: Rig) {
  await answer(rig);
  await press(commitButton());
}
/** What a press of `node` (a Pressable's host view) runs: its pressability config's `onPress`,
 * which React Native exposes under test, as the testing library's own press does. */
function onPressOf(node: ReturnType<typeof screen.getByRole>): () => void {
  type Config = { onPress?: () => void };
  const responder = (node.props as { onStartShouldSetResponder?: { testOnly_pressabilityConfig?: () => Config } }).onStartShouldSetResponder;
  const onPress = responder?.testOnly_pressabilityConfig?.().onPress;
  if (onPress === undefined) throw new Error("no onPress");
  return onPress;
}
/** A Face ID prompt that stays up until the test settles it. */
function holdFaceId() {
  let settle: (ok: boolean) => void = () => undefined;
  auth.authenticateAsync.mockImplementationOnce(
    () => new Promise((resolve) => (settle = (ok) => resolve(ok ? { success: true } : { success: false, error: "user_cancel" }))),
  );
  return async (ok: boolean) => {
    await act(async () => settle(ok));
    await flush();
  };
}

describe("the action bar", () => {
  it("holds Commit (an edge outline, fg) and Open PR on the desk (accent, bold), each at least 44 pt", async () => {
    const rig = await open();
    await answer(rig);
    const commit = commitButton();
    expect(tones("edge")).toContain(look(commit).borderColor);
    expect(tones("fg")).toContain(look(within(commit).getByText("Commit")).color);
    const pr = prButton();
    expect(tones("accent")).toContain(look(pr).borderColor);
    expect(tones("accent")).toContain(look(pr).backgroundColor);
    expect(look(within(pr).getByText("Open PR on the desk")).fontWeight).toBe("700");
    for (const button of [commit, pr]) {
      expect(look(button).minHeight).toBeGreaterThanOrEqual(44);
      expect(look(button).minWidth).toBeGreaterThanOrEqual(44);
    }
  });
});

describe("the commit sheet", () => {
  it("is prefilled with the session's first prompt as one line of at most 72 characters", async () => {
    const rig = await open();
    await feed(user(`  Make   the login\nform ${"long ".repeat(20)}`), user("Then the tests"));
    await openSheet(rig);
    expect(field().props.value).toBe(`Make the login form ${"long ".repeat(20)}`.slice(0, 72).trimEnd());
    expect(field().props.value.length).toBeLessThanOrEqual(72);
    expect(field().props.multiline).toBe(true);
    expect(field().props.maxLength).toBe(2000);
    expect(field().props.editable).not.toBe(false);
  });

  it("is empty when the session has no prompt in view, and Commit waits for a message", async () => {
    const rig = await open();
    await openSheet(rig);
    expect(field().props.value).toBe("");
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: true });
    await fireEvent.changeText(field(), "   \n ");
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: true });
    await press(confirm());
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    await fireEvent.changeText(field(), "Fix it");
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: false });
  });

  it("always asks for Face ID, even with Face ID to allow off, then sends the message as written", async () => {
    await saveSettings({ faceId: false });
    const rig = await open();
    await feed(user("Fix the login bug"));
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix the login bug\n\nThe form forgot the email.");
    await press(confirm());
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(auth.authenticateAsync.mock.calls[0]?.[0]?.promptMessage).toBe("Commit the changes");
    expect(requests(rig, "commit")).toEqual([
      { v: 1, type: "commit", req: expect.any(Number), session: SESSION, message: "Fix the login bug\n\nThe form forgot the email." },
    ]);
  });

  it("sends nothing when Face ID fails or is cancelled, and keeps the sheet and its text", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
    await press(confirm());
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(requests(rig, "commit")).toHaveLength(0);
    expect(field().props.value).toBe("Fix it");
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: false });
  });

  it("sends nothing on a phone with no Face ID, and says why", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    auth.isEnrolledAsync.mockResolvedValueOnce(false);
    await press(confirm());
    expect(requests(rig, "commit")).toHaveLength(0);
    expect(sheet().getByText("Face ID isn't set up on this phone, so it can't commit from here.")).toBeTruthy();
  });

  it("on the ack toasts the hash and the branch, closes the sheet and asks for the changes again", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    const listings = requests(rig, "changes").length;
    await ack(rig, "Committed 1a2b3c4");
    expect(toastStore.getState().toast?.text).toBe("Committed 1a2b3c4 on main");
    expect(screen.queryByTestId("commit-sheet")).toBeNull();
    expect(requests(rig, "changes")).toHaveLength(listings + 1);
  });

  it("names only the hash on a detached HEAD", async () => {
    const rig = await open();
    await answer(rig, { branch: null, upstream: null });
    await press(commitButton());
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    await ack(rig, "Committed 1a2b3c4");
    expect(toastStore.getState().toast?.text).toBe("Committed 1a2b3c4");
  });

  it("shows a refusal in the hub's words in the sheet, the text kept, and Commit can go again", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    await refuse(rig, "Finish or cancel the current turn first", "turn_open");
    expect(sheet().getByText("Finish or cancel the current turn first")).toBeTruthy();
    expect(tones("fg")).toContain(look(sheet().getByText("Finish or cancel the current turn first")).color);
    expect(field().props.value).toBe("Fix it");
    expect(toastStore.getState().toast).toBeNull();
    // A hook's own words, as the hub clipped them.
    await press(confirm());
    expect(requests(rig, "commit")).toHaveLength(2);
    await refuse(rig, "pre-commit: lint failed in src/app.ts", "not_allowed");
    expect(sheet().getByText("pre-commit: lint failed in src/app.ts")).toBeTruthy();
    expect(sheet().queryByText("Finish or cancel the current turn first")).toBeNull();
    expect(field().props.value).toBe("Fix it");
  });

  it("keeps a hook's last words, up to 2,000 characters, where the summary is", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    // The hub sends the end of what git said, marked as cut at its start.
    const words = `…${"lint: src/file.ts ok\n".repeat(90)}lint: 3 errors in src/app.ts`;
    expect(words.length).toBeLessThanOrEqual(2_000);
    expect(words.length).toBeGreaterThan(1_000);
    await refuse(rig, words, "not_allowed");
    expect(sheet().getByText(words)).toBeTruthy();
  });

  it("says what it commits, from the last listing: the files, and how many are new", async () => {
    const rig = await open();
    await answer(rig, { files: [file("src/app.ts"), file("new.ts", 1, 0, "?"), file("notes.md", 2, 0, "?")] });
    await press(commitButton());
    expect(sheet().getByText("Commits 3 files, including 2 new")).toBeTruthy();
    await press(sheet().getByRole("button", { name: "Cancel" }));
    await act(async () => screen.getByTestId("changes-list").props.refreshControl.props.onRefresh());
    await answer(rig, { files: FILES, more: 7 });
    await press(commitButton());
    expect(sheet().getByText("Commits about 9 files")).toBeTruthy();
  });

  it("draws nothing again while the session streams", async () => {
    await feed(user("Fix the login bug"));
    let commits = 0;
    const none = () => undefined;
    await render(
      <Profiler id="sheet" onRender={() => void (commits += 1)}>
        <CommitSheet session={k(SESSION)} scope={null} blocked={false} onCommitted={none} onLost={none} onClose={none} />
      </Profiler>,
    );
    expect(screen.getByLabelText("Commit message").props.value).toBe("Fix the login bug");
    commits = 0;
    for (let n = 0; n < 5; n++) {
      await act(async () => void applyM(octetStore, [{ v: 1, ...ev({ type: "text", data: ` word ${n}` }) }] as BatchItem[]));
    }
    expect(octetStore.getState().bySession.get(k(SESSION))?.transcript.entries.length).toBeGreaterThan(1);
    expect(commits).toBe(0);
  });

  it("sends one commit for a double tap, and none while one is out", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    const settle = holdFaceId();
    // Two taps in one frame: the second lands before the first has drawn the button busy.
    const tapCommit = onPressOf(confirm());
    await act(() => {
      tapCommit();
      tapCommit();
    });
    await flush();
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: true, busy: true });
    await settle(true);
    expect(requests(rig, "commit")).toHaveLength(1);
    await press(confirm());
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(requests(rig, "commit")).toHaveLength(1);
    await refuse(rig, "Git is busy; try again", "busy");
    await press(confirm());
    expect(requests(rig, "commit")).toHaveLength(2);
  });

  it("a commit whose answer was lost keeps the sheet with the words, and the list is asked for again", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    expect(requests(rig, "commit")).toHaveLength(1);
    const listings = requests(rig, "changes").length;
    // The Mac stays on the line (it answers every ping, and lists the sessions now and then) but
    // never answers the commit.
    const answered = new Set<number>();
    for (let waited = 0; waited <= REQUEST_TIMEOUT_MS; waited += 5_000) {
      await act(async () => {
        rig.clock.advance(5_000);
        rig.sockets.last.receive({ type: "sessions", sessions: [A()] });
        for (const ping of requests(rig, "ping")) {
          const req = Number(ping.req);
          if (answered.has(req)) continue;
          answered.add(req);
          rig.sockets.last.receive({ type: "ack", req, message: "pong" });
        }
      });
    }
    await flush();
    expect(rig.sockets.all).toHaveLength(1); // the link stayed up
    expect(sheet().getByText(LOST)).toBeTruthy();
    expect(field().props.value).toBe("Fix it");
    expect(toastStore.getState().toast).toBeNull();
    // It may have committed: the list says whether it did.
    expect(requests(rig, "changes")).toHaveLength(listings + 1);
  });

  it("a refusal that is a definite no asks for nothing more", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    const listings = requests(rig, "changes").length;
    await refuse(rig, "Nothing to commit", "invalid");
    expect(requests(rig, "changes")).toHaveLength(listings);
  });

  it("stays open while the commit is out, so its answer is seen", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await press(confirm());
    expect(sheet().getByRole("button", { name: "Cancel" }).props.accessibilityState).toMatchObject({ disabled: true });
    await press(screen.getByTestId("commit-sheet-scrim", { includeHiddenElements: true }));
    await act(async () => screen.getByTestId("commit-sheet").props.onRequestClose());
    expect(screen.getByTestId("commit-sheet")).toBeTruthy();
    await refuse(rig, "Nothing to commit", "invalid");
    expect(sheet().getByText("Nothing to commit")).toBeTruthy();
    await press(sheet().getByRole("button", { name: "Cancel" }));
    expect(screen.queryByTestId("commit-sheet")).toBeNull();
  });

  it("Cancel closes the sheet and sends nothing", async () => {
    const rig = await open();
    await openSheet(rig);
    const before = rig.sockets.last.sent.length;
    await press(sheet().getByRole("button", { name: "Cancel" }));
    expect(screen.queryByTestId("commit-sheet")).toBeNull();
    expect(rig.sockets.last.sent).toHaveLength(before);
  });
});

describe("offline and a waiting approval", () => {
  it("Commit is off while the Mac can't be reached", async () => {
    const rig = await open();
    await answer(rig);
    await act(async () => rig.sockets.last.drop());
    expect(commitButton().props.accessibilityState).toMatchObject({ disabled: true });
    await press(commitButton());
    expect(screen.queryByTestId("commit-sheet")).toBeNull();
  });

  it("Commit is off while an approval waits", async () => {
    const rig = await open();
    await feed(approval(1));
    await answer(rig);
    expect(commitButton().props.accessibilityState).toMatchObject({ disabled: true });
    await press(commitButton());
    expect(screen.queryByTestId("commit-sheet")).toBeNull();
  });

  it("the sheet's Commit goes off when the link drops or an approval comes, and nothing is sent", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    await feed(approval(1));
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: true });
    await press(confirm());
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    await act(async () => rig.sockets.last.drop());
    expect(confirm().props.accessibilityState).toMatchObject({ disabled: true });
    expect(requests(rig, "commit")).toHaveLength(0);
  });

  it("sends nothing when an approval comes while Face ID is up", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    const settle = holdFaceId();
    await press(confirm());
    await feed(approval(1));
    await settle(true);
    expect(requests(rig, "commit")).toHaveLength(0);
  });

  it("sends nothing when the link drops while Face ID is up", async () => {
    const rig = await open();
    await openSheet(rig);
    await fireEvent.changeText(field(), "Fix it");
    const settle = holdFaceId();
    await press(confirm());
    await act(async () => rig.sockets.last.drop());
    await settle(true);
    expect(rig.sockets.all.flatMap((s) => s.commandsOf("commit"))).toHaveLength(0);
  });
});

describe("Open PR on the desk", () => {
  it("shows the command to run at the Mac, selectable and monospace, and sends nothing", async () => {
    const rig = await open();
    await answer(rig);
    const before = rig.sockets.last.sent.length;
    await press(prButton());
    const pr = within(screen.getByTestId("pr-sheet-body"));
    expect(pr.getByText("Run this on the Mac:")).toBeTruthy();
    const command = pr.getByText("gh pr create");
    expect(command.props.selectable).toBe(true);
    expect(look(command).fontFamily).toBe(MONO);
    await press(pr.getByRole("button", { name: "Close" }));
    expect(screen.queryByTestId("pr-sheet")).toBeNull();
    expect(rig.sockets.last.sent).toHaveLength(before);
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
  });

  it("opens offline too, since it sends nothing", async () => {
    const rig = await open();
    await answer(rig);
    await act(async () => rig.sockets.last.drop());
    await press(prButton());
    expect(screen.getByTestId("pr-sheet")).toBeTruthy();
  });
});
