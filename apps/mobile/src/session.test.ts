import type { SessionState } from "./model/session";
import type { Entry } from "./model/transcript";
import type { SessionInfo } from "./protocol";
import { goalTurns, nowLine, queueWords, rowRole } from "./session";

const state = (extra: Partial<SessionState> = {}): SessionState => ({ phase: "running", queue: [], queued: 0, mode: "ask", ...extra });
const info = (extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id: "a",
  engine: "claude",
  workspace: "/w/a",
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

describe("rowRole", () => {
  it.each<[Entry, string]>([
    [{ kind: "user", key: "1", text: "x" }, "YOU"],
    [{ kind: "reply", key: "1", text: "x", done: true }, "OCTET"],
    [{ kind: "note", key: "1", text: "x" }, "NOTE"],
    [{ kind: "error", key: "1", text: "x" }, "ERROR"],
    [{ kind: "tools", key: "1", calls: [], over: false }, "TOOL"],
  ])("labels %p as the terminal UI does", (entry, label) => {
    expect(rowRole(entry).label).toBe(label);
  });
  it("errors are amber, prompts accent, replies fg, tools and notes muted", () => {
    expect(rowRole({ kind: "error", key: "1", text: "x" }).tone).toBe("amber");
    expect(rowRole({ kind: "user", key: "1", text: "x" }).tone).toBe("accent");
    expect(rowRole({ kind: "reply", key: "1", text: "x", done: true }).tone).toBe("fg");
    expect(rowRole({ kind: "tools", key: "1", calls: [], over: false }).tone).toBe("muted");
    expect(rowRole({ kind: "note", key: "1", text: "x" }).tone).toBe("muted");
  });
});

describe("the Task strip's words", () => {
  it("goal turns", () => {
    expect(goalTurns({ objective: "o", status: "active", turns: 3, max: 20 })).toBe("active · 3/20 turns");
  });
  it("the queue: first lines of the first three, the rest counted, the count the larger of the two", () => {
    expect(queueWords(state({ queue: ["a\nb", "c", "d", "e"], queued: 6 }))).toEqual({ lines: ["a", "c", "d"], more: 3, count: 6 });
    expect(queueWords(state({ queue: ["a"], queued: 0 }))).toEqual({ lines: ["a"], more: 0, count: 1 });
    expect(queueWords(null)).toEqual({ lines: [], more: 0, count: 0 });
  });
  it("NOW: the state's, else the list's, else the phase", () => {
    expect(nowLine(state({ now: "Thinking" }), info({ now: "Old" }))).toBe("Thinking");
    expect(nowLine(state(), info({ now: "Writing reply" }))).toBe("Writing reply");
    expect(nowLine(state({ phase: "stopped" }), undefined)).toBe("Stopped");
    expect(nowLine(null, undefined)).toBe("Idle");
  });
});
