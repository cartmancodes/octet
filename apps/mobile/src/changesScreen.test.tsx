// The Changes screen (`/changes?id=<session>&file=<path>`): what the session's workspace changed
// against HEAD, each file's diff on a tap, the Checks and Branch cards; and the ways there (a
// file-edit tool entry's "view ›", Usage's and Session info's "Changes ›").
import { measureParentSize } from "@shopify/flash-list/dist/recyclerview/utils/measureLayout";
import { act, fireEvent, screen, within } from "@testing-library/react-native";
import { router } from "expo-router";
import { StyleSheet } from "react-native";

import Changes, { ASK_AGAIN, LOADING_DIFF } from "../app/changes";
import Session from "../app/session/[id]";
import Usage from "../app/usage";
import { BINARY_NOTE, CLIPPED_NOTE, NO_TESTS } from "./changes";
import type { BatchItem } from "./client/HubClient";
import { MENU_DISMISS_MS } from "./components/ModelModeMenu";
import type { FileChange, JournalEvent, SessionInfo, ToolData } from "./protocol";
import { info, launch, pairedStorage, type Rig } from "./screenRig";
import { octetStore } from "./store";
import { dark, light, type Theme } from "./theme";

const SESSION = "a1b2c3d4";
const A = (extra: Partial<SessionInfo> = {}) => info(SESSION, { workspace: "/Users/me/Projects/octet", ...extra });

beforeEach(async () => {
  await pairedStorage();
  // A tall list, so FlashList draws every row (it draws only what is near the view).
  (measureParentSize as jest.Mock).mockReturnValue({ x: 0, y: 0, width: 400, height: 5000 });
});
afterEach(() => jest.restoreAllMocks());

const flush = () => act(async () => void jest.advanceTimersByTime(60));
const tones = (role: keyof Theme) => [dark[role], light[role]];
type Look = { color?: unknown; backgroundColor?: unknown; textDecorationLine?: unknown; opacity?: unknown };
const look = (node: { props: { style?: unknown } }) => StyleSheet.flatten(node.props.style as never) as Look;

const file = (path: string, add = 3, del = 1, extra: Partial<FileChange> = {}): FileChange => ({ path, add, del, binary: false, status: "M", ...extra });
const FILES = [file("src/app.ts", 3, 1), file("README.md", 20, 0, { status: "A" })];

/** The app on the Inbox, then Changes on top of it for `id`. */
async function open(params: Record<string, string> = {}, list: SessionInfo[] = [A()]) {
  const rig = await launch("/", { changes: Changes, usage: Usage }, list);
  await act(async () => router.push({ pathname: "/changes", params: { id: SESSION, ...params } }));
  await flush();
  return rig;
}
const requests = (rig: Rig, type: string) => rig.sockets.last.commandsOf(type);
const lastReq = (rig: Rig, type: string) => Number(requests(rig, type).at(-1)?.req);

/** The Mac answers the newest `changes` request. */
async function answer(rig: Rig, files: FileChange[] = FILES, extra: object = {}, req = lastReq(rig, "changes")) {
  await act(async () => {
    rig.sockets.last.receive({ type: "changes", req, branch: "main", upstream: "origin/main", ahead: 0, behind: 0, files, more: 0, ...extra });
  });
  await flush();
}
/** The Mac answers the newest `diff` request. */
async function answerDiff(rig: Rig, text: string, extra: object = {}) {
  const sent = requests(rig, "diff").at(-1);
  await act(async () => {
    rig.sockets.last.receive({ type: "diff", req: Number(sent?.req), path: sent?.path, text, clipped: false, binary: false, ...extra });
  });
  await flush();
}
async function refuse(rig: Rig, type: string, reason: string, code: string, req = lastReq(rig, type)) {
  await act(async () => {
    rig.sockets.last.receive({ type: "refused", req, reason, code });
  });
  await flush();
}
const row = (path: string) => screen.getByRole("button", { name: new RegExp(`^${path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}, `) });
const tap = async (path: string) => {
  await fireEvent.press(row(path));
  await flush();
};
const list = () => screen.getByTestId("changes-list");
async function pull() {
  await act(async () => {
    list().props.refreshControl.props.onRefresh();
  });
  await flush();
}

let seq = 0;
const ev = (event: JournalEvent) => ({ type: "event", session: SESSION, journal: "a-1.jsonl", seq: ++seq, event });
const tool = (id: string, name: string, target: string, phase: ToolData["phase"]) => ev({ type: "tool", data: { id, name, target, phase, detail: "" } });
/** Frames for the session straight into the store, as the hub would deliver them. */
async function feed(...frames: object[]) {
  await act(async () =>
    void octetStore.getState().applyBatch([{ type: "subscribed", session: SESSION, from: "start", journals: ["a-1.jsonl"] }, ...frames].map((f) => ({ v: 1, ...f })) as BatchItem[]),
  );
}

const DIFF = [
  "diff --git a/src/app.ts b/src/app.ts",
  "index 1111111..2222222 100644",
  "--- a/src/app.ts",
  "+++ b/src/app.ts",
  "@@ -1,3 +1,3 @@",
  " const a = 1;",
  "-const b = 2;",
  "+const b = 3;",
  "",
].join("\n");

describe("the header", () => {
  it("is ‹, Changes, and the name, the file count and the lines under it", async () => {
    const rig = await open();
    expect(screen.getByText("Changes")).toBeTruthy();
    expect(screen.getByText("octet")).toBeTruthy(); // before the Mac answers
    await answer(rig);
    expect(screen.getByText("octet · 2 files · +23 −1")).toBeTruthy();
    const back = screen.getByRole("button", { name: "Back" });
    const style = StyleSheet.flatten(back.props.style) as { minHeight?: number; minWidth?: number };
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
    expect(style.minWidth).toBeGreaterThanOrEqual(44);
  });

  it("says no changes for none", async () => {
    const rig = await open();
    await answer(rig, []);
    expect(screen.getByText("octet · no changes")).toBeTruthy();
    expect(screen.getByText("Nothing changed since the last commit")).toBeTruthy();
  });

  it("‹ goes back", async () => {
    const rig = await open();
    await fireEvent.press(screen.getByRole("button", { name: "Back" }));
    await flush();
    expect(rig.view.getPathname()).toBe("/");
  });
});

describe("the files", () => {
  it("asks the Mac for the session's changes once the connection is ready", async () => {
    const rig = await open();
    expect(requests(rig, "changes")).toEqual([{ v: 1, type: "changes", req: expect.any(Number), session: SESSION }]);
    expect(screen.getByText("Loading changes…")).toBeTruthy();
  });

  it("lists each file: its status, name, directory, lines and bar", async () => {
    const rig = await open();
    await answer(rig, [file("src/app.ts", 3, 1), file("README.md", 20, 0, { status: "A" }), file("crates/x/y.rs", 0, 0, { status: "D" })]);
    const app = within(row("src/app.ts"));
    expect(app.getByText("M")).toBeTruthy();
    expect(tones("muted")).toContain(look(app.getByText("M")).color);
    expect(app.getByText("app.ts")).toBeTruthy();
    expect(app.getByText("src")).toBeTruthy();
    expect(tones("muted")).toContain(look(app.getByText("src")).color);
    expect(app.getByText("+3 −1")).toBeTruthy();
    expect(app.getByTestId("bar-on").props.children).toBe("■");
    expect(app.getByTestId("bar-off").props.children).toBe("■■■■");
    expect(tones("accent")).toContain(look(app.getByTestId("bar-on")).color);
    expect(tones("edge")).toContain(look(app.getByTestId("bar-off")).color);

    const readme = within(row("README.md"));
    expect(readme.getByText("A")).toBeTruthy();
    expect(readme.getByTestId("bar-on").props.children).toBe("■■■");
    expect(within(row("crates/x/y.rs")).getByText("D")).toBeTruthy();
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ expanded: false });
    const style = StyleSheet.flatten(row("src/app.ts").props.style) as { minHeight?: number };
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
  });

  it("counts the files the Mac did not list", async () => {
    const rig = await open();
    await answer(rig, FILES, { more: 3 });
    expect(screen.getByText("3 more files on the Mac")).toBeTruthy();
    expect(screen.getByText("octet · 5 files · +23 −1")).toBeTruthy();
  });

  it("a tap asks for the file's diff and draws its lines", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toEqual([{ v: 1, type: "diff", req: expect.any(Number), session: SESSION, path: "src/app.ts" }]);
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ expanded: true });
    expect(screen.getByText("Loading the diff…")).toBeTruthy();
    await answerDiff(rig, DIFF);
    expect(screen.queryByText("Loading the diff…")).toBeNull();
    // The header lines are git's, not the change's.
    expect(screen.queryByText("+++ b/src/app.ts")).toBeNull();
    expect(screen.queryByText(/^index /)).toBeNull();

    const lines = screen.getAllByTestId("diff-line");
    expect(lines).toHaveLength(4);
    const [hunk, plain, del, add] = lines.map((l) => within(l));
    expect(tones("muted")).toContain(look(hunk!.getByText("@@ -1,3 +1,3 @@")).color);
    expect(tones("fg")).toContain(look(plain!.getByText("const a = 1;")).color);
    expect(look(lines[1]!).backgroundColor).toBeUndefined();

    const deleted = del!.getByText("const b = 2;");
    expect(tones("muted")).toContain(look(deleted).color);
    expect(look(deleted).textDecorationLine).toBe("line-through");
    expect(tones("muted")).toContain(look(del!.getByText("−")).color);

    expect(tones("selected")).toContain(look(lines[3]!).backgroundColor);
    expect(tones("accent")).toContain(look(add!.getByText("+")).color);
    expect(tones("fg")).toContain(look(add!.getByText("const b = 3;")).color);
  });

  it("keeps a diff for the screen: closing and opening again asks nothing", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF);
    await tap("src/app.ts");
    expect(screen.queryAllByTestId("diff-line")).toHaveLength(0);
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ expanded: false });
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toHaveLength(1);
    expect(screen.getAllByTestId("diff-line")).toHaveLength(4);
  });

  it("asks once while a diff is on its way", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await tap("src/app.ts");
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toHaveLength(1);
    await answerDiff(rig, DIFF);
    expect(screen.getAllByTestId("diff-line")).toHaveLength(4);
  });

  it("says when the rest of a diff is on the Mac, and when a file is binary", async () => {
    const rig = await open();
    await answer(rig, [file("src/app.ts"), file("logo.png", 0, 0, { binary: true, status: "A" })]);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF, { clipped: true });
    expect(screen.getByText(CLIPPED_NOTE)).toBeTruthy();
    await tap("logo.png");
    expect(requests(rig, "diff").at(-1)).toMatchObject({ path: "logo.png" });
    await answerDiff(rig, "diff --git a/logo.png b/logo.png\nBinary files /dev/null and b/logo.png differ\n", { binary: true });
    expect(screen.getByText(BINARY_NOTE)).toBeTruthy();
    expect(screen.queryByText(/^Binary files/)).toBeNull();
  });

  it("shows a refused diff in the hub's words, and asks again on the next opening", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await refuse(rig, "diff", "fatal: bad object HEAD", "unavailable");
    const words = screen.getByText("fatal: bad object HEAD");
    expect(tones("fg")).toContain(look(words).color);
    await tap("src/app.ts");
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toHaveLength(2);
    await answerDiff(rig, DIFF);
    expect(screen.queryByText("fatal: bad object HEAD")).toBeNull();
    expect(screen.getAllByTestId("diff-line")).toHaveLength(4);
  });

  it("draws a hostile path as plain text", async () => {
    const rig = await open();
    const hostile = "docs/\u001b]8;;https://evil.test\u0007<img src=x onerror=alert(1)>[click](javascript:alert(1))\u001b]8;;\u0007‮.md";
    await answer(rig, [file(hostile)]);
    const name = "<img src=x onerror=alert(1)>[click](javascript:alert(1)).md";
    const text = screen.getByText(name);
    expect(text.type).toBe("Text");
    expect(screen.queryAllByRole("link")).toHaveLength(0);
    expect(screen.queryByText(/evil\.test/)).toBeNull();
    expect(screen.queryByText(/\u001b/)).toBeNull();
    // The diff is asked for by the hub's own path, never drawn.
    await tap(`docs/${name}`);
    expect(requests(rig, "diff").at(-1)?.path).toBe(hostile);
    await answerDiff(rig, "@@ -0,0 +1 @@\n+<script>alert(1)</script>\u001b]8;;https://evil.test\u0007x\u001b]8;;\u0007\n");
    expect(screen.getByText("<script>alert(1)</script>x").type).toBe("Text");
    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });
});

describe("the hub's own path", () => {
  it("is what a diff is asked for by, and only the cleaned path shows", async () => {
    const rig = await open();
    const raw = "src/z\u200bw\u200fx\ry.ts";
    await answer(rig, [file(raw)]);
    const shown = "src/zwx y.ts"; // the label's newline, as a screen reader's whitespace
    expect(screen.getByText("zwx\ny.ts")).toBeTruthy();
    expect(screen.queryByText(/[\u200b\u200f\r]/)).toBeNull();
    await tap(shown);
    expect(requests(rig, "diff").at(-1)?.path).toBe(raw);
    await answerDiff(rig, DIFF);
    expect(screen.getAllByTestId("diff-line")).toHaveLength(4);
    expect(screen.queryByText(/[\u200b\u200f\r]/)).toBeNull();
  });

  it("keeps two paths that clean alike apart: two rows, two diffs", async () => {
    const rig = await open();
    const first = "a\u200b.ts";
    const second = "a\u200f.ts";
    await answer(rig, [file(first, 1, 0), file(second, 2, 0)]);
    const rows = () => screen.getAllByRole("button", { name: /^a\.ts, / });
    expect(rows()).toHaveLength(2);
    await fireEvent.press(rows()[0]!);
    await flush();
    expect(requests(rig, "diff").at(-1)?.path).toBe(first);
    await answerDiff(rig, "@@ -0,0 +1 @@\n+first\n");
    await fireEvent.press(rows()[1]!);
    await flush();
    expect(requests(rig, "diff").at(-1)?.path).toBe(second);
    await answerDiff(rig, "@@ -0,0 +1 @@\n+second\n");
    expect(screen.getByText("first")).toBeTruthy();
    expect(screen.getByText("second")).toBeTruthy();
    expect(rows().map((r) => r.props.accessibilityState.expanded)).toEqual([true, true]);
    // Closing one leaves the other open.
    await fireEvent.press(rows()[0]!);
    await flush();
    expect(screen.queryByText("first")).toBeNull();
    expect(screen.getByText("second")).toBeTruthy();
  });

  it("an edited file that two cleaned paths both name opens neither", async () => {
    const rig = await open({ file: "/Users/me/Projects/octet/a.ts" });
    await answer(rig, [file("a\u200b.ts"), file("a\u200f.ts")]);
    expect(screen.getAllByRole("button", { name: /^a\.ts, / }).map((r) => r.props.accessibilityState.expanded)).toEqual([false, false]);
    expect(requests(rig, "diff")).toHaveLength(0);
  });
});

describe("a diff past what is drawn", () => {
  it("ends with the note when the phone stops short of the hub's lines", async () => {
    // A parent tall enough for FlashList to draw all 2,000 lines (each measures 100).
    (measureParentSize as jest.Mock).mockReturnValue({ x: 0, y: 0, width: 400, height: 300_000 });
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, `@@ -1 +1 @@\n${"+x\n".repeat(2_050)}`, { clipped: false });
    expect(screen.getAllByTestId("diff-line")).toHaveLength(2_000);
    expect(screen.getByText(CLIPPED_NOTE)).toBeTruthy();
  });
});

describe("late answers", () => {
  it("drops a diff that answers a request from before a pull", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    const before = requests(rig, "diff").at(-1);
    await pull();
    await act(async () => {
      rig.sockets.last.receive({ type: "diff", req: Number(before?.req), path: "src/app.ts", text: "@@ -1 +1 @@\n+stale\n", clipped: false, binary: false });
    });
    await flush();
    expect(screen.queryByText("stale")).toBeNull();
    await answer(rig);
    expect(requests(rig, "diff")).toHaveLength(2);
    await answerDiff(rig, "@@ -1 +1 @@\n+fresh\n");
    expect(screen.getByText("fresh")).toBeTruthy();
    expect(screen.queryByText("stale")).toBeNull();
  });

  it("drops a diff that answers after the screen left", async () => {
    const error = jest.spyOn(console, "error").mockImplementation(() => {});
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await act(async () => router.back());
    await flush();
    await answerDiff(rig, DIFF);
    expect(screen.queryAllByTestId("diff-line")).toHaveLength(0);
    expect(error).not.toHaveBeenCalled();
  });
});

describe("a reload that is not a pull", () => {
  /** The link drops and comes back: the screen lists again on the new one. */
  async function reconnect(rig: Rig) {
    await act(async () => rig.sockets.last.drop());
    await act(async () => {
      rig.clock.advance(60_000);
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    await flush();
  }

  it("asks again for an open diff whose file looks as it was, showing the kept one meanwhile", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF);
    await reconnect(rig);
    await answer(rig);
    // The same counts can hide a new edit: the open file's diff is asked for again.
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts" })]);
    expect(screen.getAllByTestId("diff-line")).toHaveLength(4);
    expect(screen.queryByText(LOADING_DIFF)).toBeNull();
    await answerDiff(rig, DIFF.replace("const b = 3;", "const b = 4;"));
    expect(screen.getByText("const b = 4;")).toBeTruthy();
    expect(screen.queryByText("const b = 3;")).toBeNull();
  });

  it("forgets a closed file's diff, so opening it again asks for a fresh one", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF);
    await tap("src/app.ts");
    await reconnect(rig);
    await answer(rig);
    expect(requests(rig, "diff")).toHaveLength(0);
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts" })]);
    expect(screen.getByText(LOADING_DIFF)).toBeTruthy();
  });

  it.each([
    ["its lines added", { add: 9 }],
    ["its lines deleted", { del: 9 }],
    ["its status", { status: "R" as const }],
  ])("asks again for a diff whose file changed %s", async (_what, change) => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF);
    await reconnect(rig);
    await answer(rig, [file("src/app.ts", 3, 1, change), FILES[1]!]);
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts" })]);
    expect(screen.getByText(LOADING_DIFF)).toBeTruthy();
  });
});

describe("pull to refresh", () => {
  it("asks again, forgets the diffs, and asks again for the open file's", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF);
    expect(list().props.refreshControl.props.refreshing).toBe(false);
    await pull();
    expect(requests(rig, "changes")).toHaveLength(2);
    expect(list().props.refreshControl.props.refreshing).toBe(true);
    expect(requests(rig, "diff")).toHaveLength(1); // not before the new listing
    await answer(rig, [file("src/app.ts", 4, 1), file("new.ts", 1, 0, { status: "?" })]);
    expect(list().props.refreshControl.props.refreshing).toBe(false);
    expect(screen.getByText("octet · 2 files · +5 −1")).toBeTruthy();
    expect(within(row("new.ts")).getByText("?")).toBeTruthy();
    expect(requests(rig, "diff")).toHaveLength(2);
    expect(requests(rig, "diff").at(-1)).toMatchObject({ path: "src/app.ts" });
  });

  it("closes a file the new listing no longer has", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await answerDiff(rig, DIFF);
    await pull();
    await answer(rig, [file("README.md")]);
    expect(requests(rig, "diff")).toHaveLength(1);
    expect(screen.queryAllByTestId("diff-line")).toHaveLength(0);
  });

  it("ignores a superseded refusal while a newer request is out", async () => {
    const rig = await open();
    const first = lastReq(rig, "changes");
    await pull();
    expect(requests(rig, "changes")).toHaveLength(2);
    await refuse(rig, "changes", "Superseded; ask again", "busy", first);
    expect(screen.queryByText("Superseded; ask again")).toBeNull();
    await answer(rig);
    expect(row("src/app.ts")).toBeTruthy();
    // An answer to the older request after all is not drawn over the newer one.
    await answer(rig, [file("old.ts")], {}, first);
    expect(screen.queryByText("old.ts")).toBeNull();
  });
});

describe("two diffs at a time", () => {
  const FIVE = ["a.ts", "b.ts", "c.ts", "d.ts", "e.ts"].map((p) => file(p));
  /** The diffs sent and not yet answered. */
  const out = (rig: Rig, answered: Set<number>) => requests(rig, "diff").filter((d) => !answered.has(Number(d.req)));
  /** The Mac answers the diff request `sent`. */
  async function answerOne(rig: Rig, sent: Record<string, unknown>, answered: Set<number>) {
    answered.add(Number(sent.req));
    await act(async () => {
      rig.sockets.last.receive({ type: "diff", req: Number(sent.req), path: sent.path, text: `@@ -1 +1 @@\n+${String(sent.path)}\n`, clipped: false, binary: false });
    });
    await flush();
  }
  /** Answers every diff as it is asked for, oldest first, checking that never more than two are out. */
  async function answerAll(rig: Rig, answered: Set<number>) {
    for (let next = out(rig, answered)[0]; next !== undefined; next = out(rig, answered)[0]) {
      expect(out(rig, answered).length).toBeLessThanOrEqual(2);
      await answerOne(rig, next, answered);
    }
  }
  async function openFive(rig: Rig, answered: Set<number>) {
    await answer(rig, FIVE);
    for (const f of FIVE) await tap(f.path);
    await answerAll(rig, answered);
    expect(screen.getAllByTestId("diff-line")).toHaveLength(10); // a hunk and a line each
  }

  it("asks for at most two at once, in the order the files were opened", async () => {
    const rig = await open();
    await answer(rig, FIVE);
    for (const f of FIVE) await tap(f.path);
    expect(requests(rig, "diff").map((d) => d.path)).toEqual(["a.ts", "b.ts"]);
    const answered = new Set<number>();
    await answerAll(rig, answered);
    expect(requests(rig, "diff").map((d) => d.path)).toEqual(["a.ts", "b.ts", "c.ts", "d.ts", "e.ts"]);
  });

  it("a pull with five files open keeps at most two diffs in flight", async () => {
    const rig = await open();
    const answered = new Set<number>();
    await openFive(rig, answered);
    await pull();
    await answer(rig, FIVE);
    expect(out(rig, answered).map((d) => d.path)).toEqual(["a.ts", "b.ts"]);
    await answerAll(rig, answered);
    expect(requests(rig, "diff").slice(5).map((d) => d.path)).toEqual(["a.ts", "b.ts", "c.ts", "d.ts", "e.ts"]);
    expect(screen.queryByText(LOADING_DIFF)).toBeNull();
    expect(screen.queryByText("Git is busy; try again")).toBeNull();
  });

  it("a pull clears the queue: the new listing's diffs go in order, none from before", async () => {
    const rig = await open();
    const answered = new Set<number>();
    await openFive(rig, answered);
    await pull();
    await answer(rig, FIVE);
    const before = out(rig, answered);
    expect(before.map((d) => d.path)).toEqual(["a.ts", "b.ts"]);
    await pull();
    await answer(rig, FIVE);
    // The two from before the pull are still out on the Mac: nothing more goes until they end.
    expect(out(rig, answered)).toHaveLength(2);
    for (const sent of before) await answerOne(rig, sent, answered);
    expect(out(rig, answered).map((d) => d.path)).toEqual(["a.ts", "b.ts"]);
    await answerAll(rig, answered);
    expect(requests(rig, "diff").slice(7).map((d) => d.path)).toEqual(["a.ts", "b.ts", "c.ts", "d.ts", "e.ts"]);
  });

  it("asks again for a diff the Mac was too busy for, at most three times, then shows its words", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    for (let tries = 1; tries <= 3; tries++) {
      await refuse(rig, "diff", "Git is busy; try again", "busy");
      expect(screen.queryByText("Git is busy; try again")).toBeNull();
      expect(screen.getByText(LOADING_DIFF)).toBeTruthy();
      await act(async () => void jest.advanceTimersByTime(1_000));
      expect(requests(rig, "diff")).toHaveLength(tries + 1);
    }
    await refuse(rig, "diff", "Git is busy; try again", "busy");
    await act(async () => void jest.advanceTimersByTime(5_000));
    expect(requests(rig, "diff")).toHaveLength(4);
    expect(screen.getByText("Git is busy; try again")).toBeTruthy();
  });

  it("a busy diff asked again comes in", async () => {
    const rig = await open();
    await answer(rig);
    await tap("src/app.ts");
    await refuse(rig, "diff", "Git is busy; try again", "busy");
    await act(async () => void jest.advanceTimersByTime(1_000));
    expect(requests(rig, "diff")).toHaveLength(2);
    await answerDiff(rig, DIFF);
    expect(screen.getAllByTestId("diff-line")).toHaveLength(4);
  });
});

describe("the list's keys", () => {
  it("keep a file whose path looks like another's diff line apart from that line", async () => {
    const rig = await open();
    await answer(rig, [file("a"), file("a\n0"), file("a\nloading")]);
    await tap("a");
    await answerDiff(rig, "@@ -1 +1 @@\n+x\n");
    expect(screen.getAllByTestId("diff-line")).toHaveLength(2);
    expect(screen.getAllByRole("button", { name: /^a/ })).toHaveLength(3);
  });
});

describe("refusals", () => {
  it("an opening while the listing has failed says to ask for the changes again", async () => {
    const rig = await open();
    await answer(rig);
    await pull();
    await refuse(rig, "changes", "Git is busy; try again", "busy");
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toHaveLength(0);
    expect(screen.getByText(ASK_AGAIN)).toBeTruthy();
    expect(screen.queryByText(LOADING_DIFF)).toBeNull();
    await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
    await flush();
    expect(screen.queryByText(ASK_AGAIN)).toBeNull();
    await answer(rig);
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts" })]);
  });

  it("the not-a-repository words under another code are an ordinary refusal", async () => {
    const rig = await open();
    await refuse(rig, "changes", "This folder isn't a git repository", "invalid");
    expect(screen.getByText("This folder isn't a git repository")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
  });

  it("shows the hub's words in fg, with Try again", async () => {
    const rig = await open();
    await refuse(rig, "changes", "Git is busy; try again", "busy");
    const words = screen.getByText("Git is busy; try again");
    expect(tones("fg")).toContain(look(words).color);
    await fireEvent.press(screen.getByRole("button", { name: "Try again" }));
    await flush();
    expect(requests(rig, "changes")).toHaveLength(2);
    expect(screen.queryByText("Git is busy; try again")).toBeNull();
    await answer(rig);
    expect(row("src/app.ts")).toBeTruthy();
  });

  it("a folder that is not a repository is the screen's empty state", async () => {
    const rig = await open();
    await refuse(rig, "changes", "This folder isn't a git repository", "unavailable");
    expect(screen.getByText("This folder isn't a git repository")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(screen.queryByText("Branch")).toBeNull();
    expect(screen.queryByText("Loading changes…")).toBeNull();
  });
});

describe("offline", () => {
  it("greys the screen and takes no tap or pull until the Mac is back, then lists again", async () => {
    const rig = await open();
    await answer(rig);
    await act(async () => rig.sockets.last.drop());
    expect(look(screen.getByTestId("changes-body")).opacity).toBe(0.6);
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ disabled: true });
    await fireEvent.press(row("src/app.ts"));
    await flush();
    expect(list().props.refreshControl).toBeUndefined();
    // The listing on the old link is gone with it: back, the screen asks again before any diff.
    await act(async () => {
      rig.clock.advance(60_000);
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    await flush();
    expect(look(screen.getByTestId("changes-body")).opacity).toBeUndefined();
    expect(requests(rig, "changes")).toHaveLength(1); // the new socket's own
    expect(requests(rig, "diff")).toHaveLength(0);
    await tap("src/app.ts");
    expect(requests(rig, "diff")).toHaveLength(0); // the listing is not in yet
    await answer(rig);
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts" })]);
  });
});

describe("the Checks card", () => {
  it("says no tests ran", async () => {
    const rig = await open();
    await answer(rig);
    expect(within(screen.getByTestId("checks")).getByText(NO_TESTS)).toBeTruthy();
  });

  it("passes with the last test command done, and fails with one that failed", async () => {
    const rig = await open();
    await answer(rig);
    await feed(tool("t1", "Bash", "cargo test -p octet-hub", "done"));
    const checks = () => within(screen.getByTestId("checks"));
    expect(checks().getByText("✓")).toBeTruthy();
    expect(tones("accent")).toContain(look(checks().getByText("✓")).color);
    expect(checks().getByText(/cargo test -p octet-hub/)).toBeTruthy();
    await feed(tool("t2", "Bash", "npm test", "failed"));
    expect(checks().getByText("✗")).toBeTruthy();
    expect(tones("amber")).toContain(look(checks().getByText("✗")).color);
    expect(checks().getByText(/npm test/)).toBeTruthy();
  });

  it("does not count a command that is not a test", async () => {
    const rig = await open();
    await answer(rig);
    await feed(tool("t1", "Bash", "cargo build", "done"));
    expect(within(screen.getByTestId("checks")).getByText(NO_TESTS)).toBeTruthy();
  });
});

describe("the Branch card", () => {
  it("is the branch and how far ahead it is", async () => {
    const rig = await open();
    await answer(rig, FILES, { branch: "feat/x", upstream: "origin/feat/x", ahead: 2 });
    const branch = within(screen.getByTestId("branch"));
    expect(branch.getByText("feat/x")).toBeTruthy();
    expect(branch.getByText("2 ahead of origin/feat/x")).toBeTruthy();
  });

  it("is uncommitted changes, clean, or detached", async () => {
    const rig = await open();
    await answer(rig, FILES);
    expect(within(screen.getByTestId("branch")).getByText("uncommitted changes")).toBeTruthy();
    await pull();
    await answer(rig, [], { branch: null, upstream: null });
    expect(within(screen.getByTestId("branch")).getByText("detached")).toBeTruthy();
    expect(within(screen.getByTestId("branch")).getByText("clean")).toBeTruthy();
  });
});

describe("the ways there", () => {
  it("opens with the file in the route expanded", async () => {
    const rig = await open({ file: "/Users/me/Projects/octet/src/app.ts" });
    await answer(rig);
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ expanded: true });
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts" })]);
  });

  it("opens with nothing expanded when the file is not listed", async () => {
    const rig = await open({ file: "/Users/me/Projects/octet/gone.ts" });
    await answer(rig);
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ expanded: false });
    expect(requests(rig, "diff")).toHaveLength(0);
  });

  it("a file-edit tool entry ends in view ›, which opens Changes with that file expanded", async () => {
    seq = 0;
    const rig = await launch(`/session/${SESSION}`, { "session/[id]": Session, changes: Changes, usage: Usage }, [A()]);
    await flush();
    await feed(tool("e1", "Read", "/Users/me/Projects/octet/README.md", "done"), tool("e2", "Edit", "/Users/me/Projects/octet/src/app.ts", "done"));
    // A tools entry that edited nothing has no link.
    expect(screen.getAllByRole("button", { name: /^View changes/ })).toHaveLength(1);
    const view = screen.getByRole("button", { name: "View changes to app.ts" });
    expect(screen.getByText("view ›")).toBeTruthy();
    expect(tones("accent")).toContain(look(screen.getByText("view ›")).color);
    const style = StyleSheet.flatten(view.props.style) as { minHeight?: number; minWidth?: number };
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
    expect(style.minWidth).toBeGreaterThanOrEqual(44);
    await fireEvent.press(view);
    await flush();
    expect(rig.view.getPathname()).toBe("/changes");
    await answer(rig);
    expect(row("src/app.ts").props.accessibilityState).toMatchObject({ expanded: true });
    expect(requests(rig, "diff")).toEqual([expect.objectContaining({ path: "src/app.ts", session: SESSION })]);
  });

  it("Usage has a Changes › row", async () => {
    const rig = await launch("/", { changes: Changes, usage: Usage }, [A()]);
    await act(async () => router.push({ pathname: "/usage", params: { id: SESSION } }));
    await flush();
    expect(screen.getByText("Changes ›")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Changes" }));
    await flush();
    expect(rig.view.getPathname()).toBe("/changes");
    expect(requests(rig, "changes").at(-1)).toMatchObject({ session: SESSION });
  });

  it("Session info has a Changes › row", async () => {
    const rig = await launch(`/session/${SESSION}`, { "session/[id]": Session, changes: Changes, usage: Usage }, [A()]);
    await flush();
    await fireEvent.press(screen.getByTestId("convo-title"));
    await fireEvent.press(screen.getByRole("button", { name: "Session info, a1b2c3" }));
    await act(async () => void jest.advanceTimersByTime(MENU_DISMISS_MS + 50));
    expect(screen.getByText("Changes ›")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button", { name: "Changes" }));
    await flush();
    expect(rig.view.getPathname()).toBe("/changes");
    expect(requests(rig, "changes").at(-1)).toMatchObject({ session: SESSION });
  });
});
