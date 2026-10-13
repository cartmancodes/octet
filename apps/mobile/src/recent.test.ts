import type { CatalogInfo, RecentInfo, SessionInfo } from "./protocol";
import { modelsFor, recentItems, startedAgo, tmuxOfAck, workspaceChoices } from "./recent";

const NOW = 1_800_000_000_000; // ms

describe("startedAgo", () => {
  const ago = (seconds: number) => startedAgo(NOW / 1000 - seconds, NOW);
  it("says how long ago, in the largest whole unit", () => {
    expect(ago(0)).toBe("just now");
    expect(ago(59)).toBe("just now");
    expect(ago(60)).toBe("1 min ago");
    expect(ago(59 * 60)).toBe("59 min ago");
    expect(ago(3600)).toBe("1 h ago");
    expect(ago(23 * 3600)).toBe("23 h ago");
    expect(ago(24 * 3600)).toBe("1 d ago");
    expect(ago(6 * 24 * 3600)).toBe("6 d ago");
    expect(ago(14 * 24 * 3600)).toBe("2 w ago");
    expect(ago(60 * 24 * 3600)).toBe("2 mo ago");
    expect(ago(800 * 24 * 3600)).toBe("2 y ago");
  });
  it("treats a start in the future (clock skew) as just now, and nonsense as nothing", () => {
    expect(ago(-500)).toBe("just now");
    expect(startedAgo(Number.NaN, NOW)).toBe("");
    expect(startedAgo(0, NOW)).toBe("");
  });
});

const info = (id: string, workspace: string): SessionInfo => ({
  id,
  engine: "claude",
  workspace,
  mode: "ask",
  phase: "idle",
  approvals: 0,
  from_phone: false,
});
const recent = (workspace: string): RecentInfo => ({ engine: "claude", workspace, session: "s", title: "t", started: 1 });

describe("workspaceChoices", () => {
  it("is the live sessions' and the recent items' workspaces, deduped and sorted by folder name", () => {
    const got = workspaceChoices(
      [info("a", "/Users/me/zeta"), info("b", "/Users/me/alpha")],
      [recent("/Users/me/alpha"), recent("/work/Beta"), recent("/Users/me/zeta")],
    );
    expect(got).toEqual(["/Users/me/alpha", "/work/Beta", "/Users/me/zeta"]);
  });
  it("is empty when nothing is known, and drops blank paths", () => {
    expect(workspaceChoices([], [])).toEqual([]);
    expect(workspaceChoices([info("a", "")], [recent("")])).toEqual([]);
  });
  it("keeps two folders of the same name apart, in path order", () => {
    expect(workspaceChoices([info("a", "/b/app"), info("b", "/a/app")], [])).toEqual(["/a/app", "/b/app"]);
  });
});

describe("modelsFor", () => {
  const catalog = (models: CatalogInfo["models"]): CatalogInfo => ({ modes: ["ask"], current_mode: "ask", models });
  it("is the engine's models across the catalogs, the first of a selection winning", () => {
    const a = catalog([
      { engine: "claude", selection: "sonnet", name: "Sonnet", id: "s1" },
      { engine: "codex", selection: "gpt", name: "GPT", id: "g1" },
    ]);
    const b = catalog([
      { engine: "claude", selection: "sonnet", name: "Sonnet again", id: "s2" },
      { engine: "claude", selection: "opus", name: "Opus", id: "o1" },
    ]);
    expect(modelsFor([a, b], "claude").map((m) => m.name)).toEqual(["Sonnet", "Opus"]);
    expect(modelsFor([a, b], "codex").map((m) => m.name)).toEqual(["GPT"]);
    expect(modelsFor([a, b], "demo")).toEqual([]);
    expect(modelsFor([], "claude")).toEqual([]);
  });
});

describe("tmuxOfAck", () => {
  it("takes the tmux name from the hub's words", () => {
    expect(tmuxOfAck({ message: "Starting octet-a1b2c3 in /Users/me/octet" })).toBe("octet-a1b2c3");
  });
  it("is null for anything else", () => {
    expect(tmuxOfAck({ message: "ok" })).toBeNull();
    expect(tmuxOfAck({})).toBeNull();
    expect(tmuxOfAck(null)).toBeNull();
    expect(tmuxOfAck({ message: 5 })).toBeNull();
    expect(tmuxOfAck({ message: "Starting other-name in /w" })).toBeNull(); // not one of the hub's names
  });
});

describe("recentItems", () => {
  const item = (extra: Record<string, unknown> = {}) => ({
    engine: "claude",
    workspace: "/Users/me/octet",
    session: "abc",
    title: "Fix the hub",
    started: 1_700_000_000,
    ...extra,
  });

  it("keeps only items with the shape of a recent answer", () => {
    expect(recentItems({ type: "ack" })).toBeNull();
    expect(recentItems(null)).toBeNull();
    expect(recentItems({ type: "recent", items: "no" })).toBeNull();
    const items = recentItems({
      type: "recent",
      items: [item(), item({ engine: "vim" }), item({ title: 3 }), item({ started: "x" }), null],
    });
    expect(items).toEqual([item()]);
  });

  it("cleans the title and workspace the way sessions are, and clips them", () => {
    const [cleaned] =
      recentItems({
        type: "recent",
        items: [item({ title: "\u001b[31mred\u001b[0m \u202eevil\u0007", workspace: "/w\u001b]0;title\u0007/x" + "y".repeat(10_000) })],
      }) ?? [];
    expect(cleaned?.title).toBe("red evil");
    expect(cleaned?.workspace.startsWith("/w/xyyy")).toBe(true);
    expect(cleaned?.workspace.length).toBe(4_096);
    // The id the Mac resumes by is sent back exactly as it came.
    expect(cleaned?.session).toBe("abc");
  });
});
