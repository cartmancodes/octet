import type { ToolData, ToolPhase } from "../protocol";
import { FAILED_ROWS, callRows, label, lines, summary, toolRows } from "./summary";

const call = (id: string, name: string, target: string, phase: ToolPhase, detail = ""): ToolData => ({
  id,
  name,
  target,
  phase,
  detail,
});

describe("label", () => {
  it("names the vendors' tools the way the transcript does", () => {
    expect(label("commandExecution")).toBe("Bash");
    expect(label("fileChange")).toBe("Edit");
    expect(label("mcpToolCall")).toBe("MCP");
    expect(label("Read")).toBe("Read");
    expect(label("WebFetch")).toBe("WebFetch");
  });
});

describe("summary", () => {
  it("counts by name in order of first appearance", () => {
    const calls = [
      call("1", "Read", "a", "done"),
      call("2", "Read", "b", "done"),
      call("3", "Read", "c", "done"),
      call("4", "commandExecution", "cargo build", "done"),
    ];
    expect(summary(calls)).toBe("Read 3 files · Ran 1 command");
  });

  it("is singular for one", () => {
    expect(summary([call("1", "Read", "a", "done")])).toBe("Read 1 file");
    expect(summary([call("1", "Write", "a", "done")])).toBe("Wrote 1 file");
    expect(summary([call("1", "Edit", "a", "done")])).toBe("Edited 1 file");
    expect(summary([call("1", "Bash", "a", "done")])).toBe("Ran 1 command");
    const grep = call("1", "Grep", "a", "done");
    const fetch = call("2", "WebFetch", "u", "done");
    expect(summary([grep, fetch, fetch])).toBe("Searched once · WebFetch ×2");
  });

  it("is plural for many", () => {
    const many = (name: string) => [1, 2].map((n) => call(String(n), name, "x", "done"));
    expect(summary(many("Write"))).toBe("Wrote 2 files");
    expect(summary(many("Bash"))).toBe("Ran 2 commands");
    expect(summary(many("Grep"))).toBe("Searched 2 times");
    expect(summary(many("Edit"))).toBe("Edited 2 files");
    expect(summary(many("WebSearch"))).toBe("WebSearch ×2");
  });

  it("counts Edit, MultiEdit, NotebookEdit and fileChange together as Edited", () => {
    const calls = ["Edit", "MultiEdit", "NotebookEdit", "fileChange"].map((n, i) =>
      call(String(i), n, "f", "done"),
    );
    expect(summary(calls)).toBe("Edited 4 files");
  });

  it("counts Grep and Glob together as Searched", () => {
    const calls = [call("1", "Grep", "a", "done"), call("2", "Glob", "*.rs", "done")];
    expect(summary(calls)).toBe("Searched 2 times");
    expect(summary([call("1", "Glob", "*.rs", "done")])).toBe("Searched once");
  });

  it("counts MCP tools by the MCP label and is empty for no calls", () => {
    expect(summary([call("1", "mcpToolCall", "x", "done"), call("2", "mcpToolCall", "y", "done")])).toBe("MCP ×2");
    expect(summary([])).toBe("");
  });
});

describe("lines", () => {
  it("splits like Rust's str::lines", () => {
    expect(lines("")).toEqual([]);
    expect(lines("a")).toEqual(["a"]);
    expect(lines("a\nb\n")).toEqual(["a", "b"]);
    expect(lines("a\n\nb")).toEqual(["a", "", "b"]);
    expect(lines("a\r\nb")).toEqual(["a", "b"]);
  });
});

describe("toolRows", () => {
  const twelve = Array.from({ length: 12 }, (_, i) => `out ${i + 1}`).join("\n");

  it("summarises the done calls, lists the running ones and opens the failures", () => {
    const calls = [
      call("1", "Read", "a.rs", "done", "x"),
      call("2", "Read", "b.rs", "done", "x"),
      call("3", "Read", "c.rs", "done", "x"),
      call("4", "commandExecution", "cargo build", "done", "x"),
      call("5", "Grep", "fn main", "running", "x"),
      call("6", "Bash", "cargo test", "failed", twelve),
    ];
    const rows = toolRows(calls, false);
    expect(rows).toHaveLength(3);
    expect(rows[0]).toEqual({ kind: "summary", head: "Read 3 files · Ran 1 command", lines: [], more: 0 });
    expect(rows[1]).toEqual({ kind: "running", head: "… Grep fn main", lines: [], more: 0 });
    expect(rows[2]?.kind).toBe("failed");
    expect(rows[2]?.head).toBe("✗ Bash cargo test");
    expect(FAILED_ROWS).toBe(8);
    expect(rows[2]?.lines).toEqual(Array.from({ length: 8 }, (_, i) => `out ${i + 5}`));
    expect(rows[2]?.more).toBe(4);
  });

  it("says how many lines a failure hides", () => {
    const rows = toolRows([call("1", "Bash", "x", "failed", twelve)], false);
    expect(rows[0]).toMatchObject({ more: 4 });
    const short = toolRows([call("1", "Bash", "x", "failed", "a\nb")], false);
    expect(short[0]).toMatchObject({ lines: ["a", "b"], more: 0 });
  });

  it("says stopped on a running call whose turn is over", () => {
    const rows = toolRows([call("1", "Bash", "sleep 9", "running")], true);
    expect(rows[0]?.head).toBe("… Bash sleep 9 · stopped");
    expect(toolRows([call("1", "Bash", "sleep 9", "failed", "x")], true)[0]?.head).toBe("✗ Bash sleep 9");
  });

  it("trims a head with no target", () => {
    expect(toolRows([call("1", "Bash", "", "running")], false)[0]?.head).toBe("… Bash");
  });

  it("draws nothing for no calls", () => {
    expect(toolRows([], false)).toEqual([]);
  });
});

describe("callRows", () => {
  it("lists every call with its marks and whole detail", () => {
    const rows = callRows(
      [call("1", "Read", "a.rs", "done", "line one\nline two"), call("2", "Bash", "ls", "failed", "boom")],
      false,
    );
    expect(rows).toEqual([
      { kind: "done", head: "✓ Read a.rs", lines: ["line one", "line two"], more: 0 },
      { kind: "failed", head: "✗ Bash ls", lines: ["boom"], more: 0 },
    ]);
  });
});
