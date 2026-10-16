// The Usage screen (⋯ in a conversation's header): this session's engine, usage, turns and
// journal, every session's usage line for today, and Export journal and Fresh context.
import { act, fireEvent, screen, within } from "@testing-library/react-native";
import { router } from "expo-router";
import { StyleSheet } from "react-native";

import Usage from "../app/usage";
import { FRESH_QUESTION } from "./components/CommandSheet";
import { hear, info, launch, pairedStorage, type Rig } from "./screenRig";
import type { BatchItem } from "./client/HubClient";
import type { JournalEvent, SessionInfo } from "./protocol";
import { octetStore } from "./store";
import { applyM, k } from "./testKeys";
import { hideToast, toastStore } from "./toast";

const SESSION = "a1b2c3d4";

beforeEach(async () => {
  await pairedStorage();
  hideToast();
});
afterEach(() => {
  hideToast();
  jest.restoreAllMocks();
});

const flush = () => act(async () => void jest.advanceTimersByTime(60));
let seq = 0;
const ev = (event: JournalEvent, session = SESSION, journal = "a-2.jsonl") => ({ type: "event", session, journal, seq: ++seq, event });
/** Frames for the session, as the hub would deliver them (the conversation behind this screen is
 * not streaming, so they go straight to the store). */
async function feed(_rig: Rig, ...frames: object[]) {
  await act(async () => void applyM(octetStore, frames.map((f) => ({ v: 1, ...f })) as BatchItem[]));
}
/** Frames over the socket, as the Mac sends them: the client must be subscribed to deliver them. */
async function socketFeed({ sockets, clock }: Rig, ...frames: object[]) {
  await act(async () => {
    for (const frame of frames) sockets.last.receive(frame);
    clock.advance(50);
  });
}
const subscribed = (journals: string[] = ["a-1.jsonl", "a-2.jsonl"], session = SESSION) => ({ type: "subscribed", session, from: "start", journals });

const A = (extra: Partial<SessionInfo> = {}) => info("a1b2c3d4", { workspace: "/Users/me/Projects/octet", last_activity: 1_000, ...extra });

/** The app on the Inbox, then Usage on top of it for `id`. */
async function open(list: SessionInfo[] = [A()], id = SESSION) {
  seq = 0;
  const rig = await launch("/", { usage: Usage }, list);
  await act(async () => router.push({ pathname: "/usage", params: { id: k(id) } }));
  await flush();
  return rig;
}
const button = (name: string | RegExp) => screen.queryByRole("button", { name });
const press = async (name: string | RegExp) => {
  await fireEvent.press(screen.getByRole("button", { name }));
  await flush();
};
const requests = (rig: Rig, type: string) => rig.sockets.last.commandsOf(type);
const lastReq = (rig: Rig, type: string) => Number(requests(rig, type).at(-1)?.req);
async function ack(rig: Rig, message: string) {
  await act(async () => {
    rig.sockets.last.receive({ type: "ack", req: lastReq(rig, "command"), message });
  });
  await flush();
}
async function refuse(rig: Rig, reason: string, code = "turn_open") {
  await act(async () => {
    rig.sockets.last.receive({ type: "refused", req: lastReq(rig, "command"), reason, code });
  });
  await flush();
}
/** The words a node draws, nested text included. */
const textOf = (node: unknown): string =>
  typeof node === "string" ? node : ((node as { children?: unknown[] }).children ?? []).map(textOf).join("");

describe("the header", () => {
  it("is ‹, Usage, and the session's name and short id under it", async () => {
    await open();
    expect(screen.getByText("Usage")).toBeTruthy();
    expect(screen.getByText("octet · a1b2c3")).toBeTruthy();
    const back = screen.getByRole("button", { name: "Back" });
    const style = StyleSheet.flatten(back.props.style) as { minHeight?: number; height?: number; minWidth?: number; width?: number };
    expect(Math.max(style.minHeight ?? 0, style.height ?? 0)).toBeGreaterThanOrEqual(44);
    expect(Math.max(style.minWidth ?? 0, style.width ?? 0)).toBeGreaterThanOrEqual(44);
  });

  it("‹ goes back to the conversation's place", async () => {
    const rig = await open();
    expect(rig.view.getPathname()).toBe("/usage");
    await press("Back");
    expect(rig.view.getPathname()).toBe("/");
  });
});

describe("This session", () => {
  it("shows the engine, the view's latest usage, the turns and the newest journal", async () => {
    const rig = await open([A({ usage: "row usage" })]);
    await feed(
      rig,
      subscribed(),
      ev({ type: "usage", data: "5h 12% · week 3%" }),
      ev({ type: "finished", data: "completed" }),
      ev({ type: "finished", data: "completed" }),
      ev({ type: "usage", data: "5h 20% · week 4%" }),
    );
    expect(screen.getByText("This session")).toBeTruthy();
    for (const label of ["ENGINE", "USAGE", "TURNS", "JOURNAL"]) expect(screen.getByText(label)).toBeTruthy();
    expect(screen.getByText("claude")).toBeTruthy();
    expect(within(screen.getByTestId("this-session")).getByText("5h 20% · week 4%")).toBeTruthy();
    expect(screen.queryByText("5h 12% · week 3%")).toBeNull();
    expect(screen.getByText("2")).toBeTruthy();
    expect(screen.getByText("a-2.jsonl")).toBeTruthy();
    expect(screen.queryByText("a-1.jsonl")).toBeNull();
  });

  it("falls back to the row's usage, and then to 'No usage reported yet'", async () => {
    const rig = await open([A({ usage: "row usage" })]);
    // Today lists the row's usage too: the session's own line is the one under USAGE.
    expect(within(screen.getByTestId("this-session")).getByText("row usage")).toBeTruthy();
    await hear(rig, [A({ usage: null })]);
    expect(within(screen.getByTestId("this-session")).getByText("No usage reported yet")).toBeTruthy();
  });

  it("shows the engine / model when the catalog names one", async () => {
    const rig = await open([A({ model: "claude-opus-5" })]);
    expect(within(screen.getByTestId("this-session")).getByText("claude / claude-opus-5")).toBeTruthy();
    await feed(rig, {
      type: "catalog",
      session: SESSION,
      catalog: { modes: ["ask"], current_mode: "ask", models: [{ engine: "claude", selection: "opus", name: "Opus", id: "claude-opus-5" }], current: { engine: "claude", selection: "opus" } },
    });
    expect(within(screen.getByTestId("this-session")).getByText("claude / Opus")).toBeTruthy();
  });

  it("shows 0 turns and no journal before the session was streamed", async () => {
    await open();
    expect(within(screen.getByTestId("this-session")).getByText("0")).toBeTruthy();
    expect(within(screen.getByTestId("this-session")).getByText("None yet")).toBeTruthy();
  });

  it("has no context bar", async () => {
    await open();
    expect(screen.queryByText("Context")).toBeNull();
    expect(screen.queryByText(/%$/)).toBeNull();
  });

  it("draws hub text as plain text only", async () => {
    const rig = await open([A()]);
    await feed(rig, subscribed(["<b>x</b>.jsonl"]), ev({ type: "usage", data: "\u001b[31m5h\u001b[0m <i>12%</i> [a](http://evil.example)\u0007" }, SESSION, "<b>x</b>.jsonl"));
    expect(within(screen.getByTestId("this-session")).getByText("5h <i>12%</i> [a](http://evil.example)")).toBeTruthy();
    expect(within(screen.getByTestId("today")).getByText("octet 5h <i>12%</i> [a](http://evil.example)")).toBeTruthy();
    expect(screen.getByText("<b>x</b>.jsonl")).toBeTruthy();
    expect(screen.queryByRole("link")).toBeNull();
  });
});

describe("This session: who", () => {
  it("shows the full session id (selectable) and the workspace", async () => {
    await open([A({ workspace: "/Users/me/Projects/octet" })]);
    const card = within(screen.getByTestId("this-session"));
    for (const label of ["SESSION", "WORKSPACE"]) expect(card.getByText(label)).toBeTruthy();
    const id = card.getByText(SESSION);
    expect(id.props.selectable).toBe(true);
    expect(card.getByText("/Users/me/Projects/octet").props.selectable).toBe(true);
  });

  it("a session that is not in the list shows its id and says the rest is unknown", async () => {
    await open([info("other")]);
    const card = within(screen.getByTestId("this-session"));
    expect(card.getByText(SESSION)).toBeTruthy();
    expect(card.getByText("Not listed")).toBeTruthy();
    expect(screen.getByText("Session · a1b2c3")).toBeTruthy();
    for (const name of ["Export journal /export", "Fresh context /new"]) {
      expect(screen.getByRole("button", { name }).props.accessibilityState?.disabled).toBe(true);
    }
  });

  it("an ended session keeps the workspace it had, from the list or from the route", async () => {
    const rig = await open([A({ workspace: "/Users/me/Projects/octet" })]);
    await hear(rig, [info("other")]);
    expect(within(screen.getByTestId("this-session")).getByText("/Users/me/Projects/octet")).toBeTruthy();
    expect(screen.getByText("octet · a1b2c3")).toBeTruthy();
  });

  it("takes the workspace from the route when the row is already gone, as plain text", async () => {
    seq = 0;
    await launch("/", { usage: Usage }, [info("other")]);
    await act(async () => router.push({ pathname: "/usage", params: { id: k(SESSION), workspace: "/w/<b>octet</b>\u001b[2J" } }));
    await flush();
    expect(within(screen.getByTestId("this-session")).getByText("/w/<b>octet</b>")).toBeTruthy();
  });
});

describe("the workspace param", () => {
  it("is clipped to 512 characters", async () => {
    seq = 0;
    await launch("/", { usage: Usage }, [info("other")]);
    await act(async () => router.push({ pathname: "/usage", params: { id: k(SESSION), workspace: `/w/${"x".repeat(2_000)}` } }));
    await flush();
    const shown = within(screen.getByTestId("this-session")).getByText(/^\/w\/x+$/);
    expect(String(shown.props.children).length).toBe(512);
  });
});

describe("live", () => {
  it("watches the session while it is in front: subscribe on focus, unsubscribe on blur", async () => {
    const rig = await open();
    expect(rig.sockets.last.commandsOf("subscribe")).toEqual([expect.objectContaining({ session: SESSION })]);
    expect(rig.sockets.last.commandsOf("unsubscribe")).toEqual([]);
    await press("Back");
    expect(rig.sockets.last.commandsOf("unsubscribe")).toEqual([expect.objectContaining({ session: SESSION })]);
  });

  it("USAGE and TURNS follow the frames the Mac sends while it is open", async () => {
    const rig = await open();
    const frame = (event: JournalEvent) => ({ type: "event", session: SESSION, journal: "a-1.jsonl", seq: ++seq, event });
    await socketFeed(rig,
      { type: "subscribed", session: SESSION, from: "start", journals: ["a-1.jsonl"] },
      frame({ type: "usage", data: "5h 12%" }),
      frame({ type: "finished", data: "completed" }),
    );
    const card = within(screen.getByTestId("this-session"));
    expect(card.getByText("5h 12%")).toBeTruthy();
    expect(card.getByText("1")).toBeTruthy();
    await socketFeed(rig, frame({ type: "usage", data: "5h 15%" }), frame({ type: "finished", data: "completed" }));
    expect(card.getByText("5h 15%")).toBeTruthy();
    expect(card.getByText("2")).toBeTruthy();
    expect(card.getByText("a-1.jsonl")).toBeTruthy();
  });

  it("does not subscribe a session the list no longer holds", async () => {
    const rig = await open([info("other")]);
    expect(rig.sockets.last.commandsOf("subscribe")).toEqual([]);
  });
});

describe("Today", () => {
  it("uses the view's usage for this session, and lists it though its row reports none", async () => {
    const rig = await open([A({ usage: "old row usage", last_activity: 500 }), info("other", { workspace: "/w/other", usage: "5h 1%", last_activity: 100 })]);
    await feed(rig, subscribed(), ev({ type: "usage", data: "fresh view usage" }));
    const lines = within(screen.getByTestId("today")).getAllByTestId("today-line").map(textOf);
    expect(lines).toEqual(["octet fresh view usage", "other 5h 1%"]);
    await hear(rig, [A({ usage: null, last_activity: 500 }), info("other", { workspace: "/w/other", usage: "5h 1%", last_activity: 100 })]);
    expect(within(screen.getByTestId("today")).getAllByTestId("today-line").map(textOf)).toEqual(["octet fresh view usage", "other 5h 1%"]);
  });

  it("lists each session's usage line by most recent activity, '<name> <usage>'", async () => {
    await open([
      info("old", { workspace: "/w/old", usage: "5h 1%", last_activity: 100 }),
      info("quiet", { workspace: "/w/quiet" }),
      info("newest", { workspace: "/w/newest", usage: "5h 50% · week 9%", last_activity: 900 }),
      info("none", { workspace: "/w/none", usage: "5h 7%", last_activity: null }),
      info("a1b2c3d4", { workspace: "/Users/me/Projects/octet", last_activity: 500, usage: "5h 12%" }),
    ]);
    expect(screen.getByText("Today")).toBeTruthy();
    const lines = within(screen.getByTestId("today")).getAllByTestId("today-line");
    expect(lines.map(textOf)).toEqual([
      "newest 5h 50% · week 9%",
      "octet 5h 12%",
      "old 5h 1%",
      "none 5h 7%",
    ]);
    expect(within(screen.getByTestId("today")).queryByText(/quiet/)).toBeNull();
  });

  it("says so when no session reports usage", async () => {
    await open([A()]);
    expect(within(screen.getByTestId("today")).getByText("No usage reported yet")).toBeTruthy();
  });

  it("draws a row's usage as plain text", async () => {
    await open([A({ usage: "<b>x</b> \u001b[2Jok" })]);
    expect(within(screen.getByTestId("today")).getByText("octet <b>x</b> ok")).toBeTruthy();
  });
});

describe("Export journal /export", () => {
  it("sends the export command and toasts the ack", async () => {
    const rig = await open();
    expect(requests(rig, "command")).toEqual([]);
    await press("Export journal /export");
    expect(requests(rig, "command")).toEqual([expect.objectContaining({ session: SESSION, name: "export" })]);
    expect(toastStore.getState().toast).toBeNull();
    await ack(rig, "Exported the journal to /w/a-2.jsonl");
    expect(toastStore.getState().toast?.text).toBe("Exported the journal to /w/a-2.jsonl");
  });

  it("a refusal shows its words on the screen", async () => {
    const rig = await open();
    await press("Export journal /export");
    await refuse(rig, "Wait for: a shell command", "busy");
    expect(screen.getByText("Wait for: a shell command")).toBeTruthy();
    expect(toastStore.getState().toast).toBeNull();
  });
});

describe("Fresh context /new", () => {
  it("confirms, sends only on Start fresh, and returns to the conversation on the ack", async () => {
    const rig = await open();
    await press("Fresh context /new");
    expect(screen.getByText(FRESH_QUESTION)).toBeTruthy();
    expect(requests(rig, "command")).toEqual([]);
    await press("Cancel");
    expect(screen.queryByText(FRESH_QUESTION)).toBeNull();
    expect(requests(rig, "command")).toEqual([]);
    await press("Fresh context /new");
    await press("Start fresh");
    expect(requests(rig, "command")).toEqual([expect.objectContaining({ session: SESSION, name: "new" })]);
    expect(rig.view.getPathname()).toBe("/usage");
    await ack(rig, "Starting a fresh conversation");
    expect(rig.view.getPathname()).toBe("/");
  });

  it("a refusal shows the desk's words and stays", async () => {
    const rig = await open();
    await press("Fresh context /new");
    await press("Start fresh");
    await refuse(rig, "A turn is running; wait or cancel it first");
    expect(screen.getByText("A turn is running; wait or cancel it first")).toBeTruthy();
    expect(rig.view.getPathname()).toBe("/usage");
  });
});

describe("the buttons are off", () => {
  it("while a command is out", async () => {
    const rig = await open();
    await press("Export journal /export");
    for (const name of ["Export journal /export", "Fresh context /new"]) {
      expect(screen.getByRole("button", { name }).props.accessibilityState?.disabled).toBe(true);
    }
    await fireEvent.press(screen.getByRole("button", { name: "Fresh context /new" }));
    expect(screen.queryByText(FRESH_QUESTION)).toBeNull();
    await ack(rig, "Exported the journal to /w/j");
    expect(screen.getByRole("button", { name: "Export journal /export" }).props.accessibilityState?.disabled).toBe(false);
  });

  it("offline", async () => {
    const rig = await open();
    await act(async () => {
      rig.sockets.last.drop();
    });
    for (const name of ["Export journal /export", "Fresh context /new"]) {
      expect(screen.getByRole("button", { name }).props.accessibilityState?.disabled).toBe(true);
    }
    expect(requests(rig, "command")).toEqual([]);
  });

  it("when the session is no longer listed", async () => {
    const rig = await open([A(), info("other")]);
    await hear(rig, [info("other")]);
    for (const name of ["Export journal /export", "Fresh context /new"]) {
      expect(screen.getByRole("button", { name }).props.accessibilityState?.disabled).toBe(true);
    }
  });

  it("each is a 44 pt button", async () => {
    await open();
    for (const name of ["Export journal /export", "Fresh context /new"]) {
      const style = StyleSheet.flatten(screen.getByRole("button", { name }).props.style) as { minHeight?: number; height?: number };
      expect(Math.max(style.minHeight ?? 0, style.height ?? 0)).toBeGreaterThanOrEqual(44);
    }
  });
});

it("reads from the store only: no session in the route says so", async () => {
  await launch("/", { usage: Usage }, [A()]);
  await act(async () => router.push("/usage"));
  await flush();
  expect(screen.getByText("No session")).toBeTruthy();
  expect(octetStore.getState().bySession.size).toBe(0);
  expect(button("Export journal /export")).toBeNull();
});
