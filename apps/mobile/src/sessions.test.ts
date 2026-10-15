import type { SessionInfo } from "./protocol";
import { clipLabel, folderName, poseOf, sessionName, sessionPose } from "./sessions";

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

describe("sessionName", () => {
  it.each([
    ["/Users/me/Octet", "Octet"],
    ["", "Session"],
    ["/", "Session"],
    ["///", "Session"],
    ["/Users/me/   /", "Session"],
    [undefined, "Session"],
  ])("%p is %p", (workspace, name) => {
    expect(sessionName(workspace)).toBe(name);
  });
});

describe("clipLabel", () => {
  it("leaves a short text alone and cuts a long one to the limit with an ellipsis", () => {
    expect(clipLabel("short")).toBe("short");
    expect(clipLabel("x".repeat(120))).toBe("x".repeat(120));
    const cut = clipLabel("x".repeat(121));
    expect(cut).toHaveLength(120);
    expect(cut.endsWith("…")).toBe(true);
  });
  it("never splits a surrogate pair", () => {
    const cut = clipLabel("x".repeat(118) + "😀".repeat(10));
    expect(cut).toBe("x".repeat(118) + "…");
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

describe("poseOf: one session's pose, with how its last turn ended", () => {
  it("shows success after a completed turn and error after a failed one, while idle", () => {
    expect(poseOf({ approvals: 0, phase: "idle", turn: "finished" })).toBe("success");
    expect(poseOf({ approvals: 0, phase: "idle", turn: "failed" })).toBe("error");
    expect(poseOf({ approvals: 0, phase: "idle", turn: "stopped" })).toBe("sleeping");
    expect(poseOf({ approvals: 0, phase: "idle", turn: null })).toBe("idle");
  });
  it("lets an approval, a running turn and a stopped connection come first", () => {
    expect(poseOf({ approvals: 1, phase: "idle", turn: "failed" })).toBe("approval");
    expect(poseOf({ approvals: 0, phase: "running", now: "Running Edit a.rs", turn: "finished" })).toBe("coding");
    expect(poseOf({ approvals: 0, phase: "running", now: null, turn: "failed" })).toBe("thinking");
    expect(poseOf({ approvals: 0, phase: "stopped", turn: "finished" })).toBe("sleeping");
  });
  it("keeps the error pose when a session stops after an error", () => {
    expect(poseOf({ approvals: 0, phase: "stopped", turn: "failed" })).toBe("error");
    expect(poseOf({ approvals: 1, phase: "stopped", turn: "failed" })).toBe("approval");
    expect(poseOf({ approvals: 0, phase: "running", now: "Thinking", turn: "failed" })).toBe("thinking");
  });
});
