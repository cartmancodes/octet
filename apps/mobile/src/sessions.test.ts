import type { SessionInfo } from "./protocol";
import { engineLine, folderName, headerPose, nowText, sessionPose, sortSessions } from "./sessions";

const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/work/${id}`,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

describe("folderName", () => {
  it.each([
    ["/Users/me/Projects/Octet", "Octet"],
    ["/Users/me/Projects/Octet/", "Octet"],
    ["/Users/me/Projects/Octet///", "Octet"],
    ["Octet", "Octet"],
    ["C:\\work\\octet", "octet"],
    ["/", "/"],
    ["", ""],
  ])("%s -> %s", (path, name) => {
    expect(folderName(path)).toBe(name);
  });
});

describe("engineLine", () => {
  it("names the engine and the model", () => {
    expect(engineLine(info("a", { engine: "claude", model: "sonnet-5" }))).toBe("Claude · sonnet-5");
    expect(engineLine(info("a", { engine: "codex" }))).toBe("Codex");
    expect(engineLine(info("a", { engine: "demo", model: "" }))).toBe("Demo");
  });
});

describe("nowText", () => {
  it("is the NOW line, else the phase in words", () => {
    expect(nowText(info("a", { now: "Running Bash cargo test", phase: "running" }))).toBe("Running Bash cargo test");
    expect(nowText(info("a", { now: "", phase: "idle" }))).toBe("Idle");
    expect(nowText(info("a", { phase: "running" }))).toBe("Running");
    expect(nowText(info("a", { now: null, phase: "stopped" }))).toBe("Stopped");
    expect(nowText(info("a", { phase: "connecting" }))).toBe("Connecting");
  });
});

describe("sortSessions", () => {
  it("puts approvals waiting first, then the newest activity", () => {
    const list = [
      info("old", { last_activity: 100 }),
      info("new", { last_activity: 300 }),
      info("wait-old", { approvals: 1, last_activity: 50 }),
      info("wait-new", { approvals: 2, last_activity: 200 }),
      info("none"),
    ];
    expect(sortSessions(list).map((s) => s.id)).toEqual(["wait-new", "wait-old", "new", "old", "none"]);
  });

  it("is stable by id on a tie and does not change its input", () => {
    const list = [info("b", { last_activity: 1 }), info("a", { last_activity: 1 })];
    expect(sortSessions(list).map((s) => s.id)).toEqual(["a", "b"]);
    expect(list.map((s) => s.id)).toEqual(["b", "a"]);
  });
});

describe("sessionPose", () => {
  it("is approval when one waits, whatever else is going on", () => {
    expect(sessionPose(info("a", { approvals: 1, phase: "running", now: "Running Bash ls" }))).toBe("approval");
  });
  it("follows the NOW line while running", () => {
    const running = (now: string | null) => info("a", { phase: "running", now });
    expect(sessionPose(running("Running Edit src/main.rs"))).toBe("coding");
    expect(sessionPose(running("Running Bash cargo test"))).toBe("coding");
    expect(sessionPose(running("Running Bash rg foo"))).toBe("searching");
    expect(sessionPose(running("Running Grep needle"))).toBe("searching");
    expect(sessionPose(running("Running Read a.txt"))).toBe("searching");
    expect(sessionPose(running("Running Task review it"))).toBe("delegating");
    expect(sessionPose(running("Running Bash"))).toBe("coding");
    expect(sessionPose(running("Thinking"))).toBe("thinking");
    expect(sessionPose(running("Writing reply"))).toBe("thinking");
    expect(sessionPose(running(null))).toBe("thinking");
  });
  it("follows the phase otherwise", () => {
    expect(sessionPose(info("a", { phase: "idle" }))).toBe("idle");
    expect(sessionPose(info("a", { phase: "connecting" }))).toBe("idle");
    expect(sessionPose(info("a", { phase: "stopped" }))).toBe("sleeping");
    // a stale NOW line does not make a stopped session busy
    expect(sessionPose(info("a", { phase: "idle", now: "Running Bash ls" }))).toBe("idle");
  });
});

describe("headerPose: the worst state across the sessions", () => {
  it("is sleeping while the hub cannot be reached, even with a list", () => {
    expect(headerPose([info("a", { approvals: 1 })], "offline")).toBe("sleeping");
  });
  it("is idle with nothing running", () => {
    expect(headerPose([], "ready")).toBe("idle");
    expect(headerPose([info("a"), info("b")], "ready")).toBe("idle");
  });
  it("is the running session's pose", () => {
    const list = [info("a"), info("b", { phase: "running", now: "Running Bash rg x", last_activity: 5 })];
    expect(headerPose(list, "ready")).toBe("searching");
  });
  it("takes the newest of several running sessions", () => {
    const list = [
      info("a", { phase: "running", now: "Running Edit x", last_activity: 1 }),
      info("b", { phase: "running", now: "Thinking", last_activity: 9 }),
    ];
    expect(headerPose(list, "ready")).toBe("thinking");
  });
  it("is approval when any session waits, over a running one", () => {
    const list = [info("a", { phase: "running", now: "Running Edit x" }), info("b", { approvals: 3 })];
    expect(headerPose(list, "ready")).toBe("approval");
  });
  it("does not call a reconnecting hub asleep", () => {
    expect(headerPose([info("a")], "connecting")).toBe("idle");
  });
});
