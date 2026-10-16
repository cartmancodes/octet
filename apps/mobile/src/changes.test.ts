// What the Changes screen derives from the hub's answers and the session's view: the bar, the
// summary, the Checks heuristic, the Branch card, the diff's lines and the file a tool edited.
import {
  BINARY_NOTE,
  CLIPPED_NOTE,
  MAX_DIFF_LINES,
  NO_TESTS,
  PREFILL_CHARS,
  barCells,
  branchLines,
  changeSummary,
  commitScope,
  committedText,
  diffLines,
  editedFile,
  firstPrompt,
  lastChecks,
  matchFile,
  rowKey,
  splitPath,
} from "./changes";
import type { Entry, ToolCall } from "./model/transcript";
import type { FileChange } from "./protocol";

const file = (path: string, add: number, del: number, extra: Partial<FileChange> = {}): FileChange => ({
  path,
  add,
  del,
  binary: false,
  status: "M",
  ...extra,
});
const call = (name: string, target: string, phase: ToolCall["phase"] = "done"): ToolCall => ({ id: target, name, target, phase, detail: "" });
let key = 0;
const tools = (...calls: ToolCall[]): Entry => ({ kind: "tools", key: String(++key), calls, over: false });

describe("the bar", () => {
  it.each([
    [0, 0, 1],
    [1, 0, 1],
    [8, 0, 1],
    [5, 4, 2],
    [16, 0, 2],
    [17, 0, 3],
    [32, 0, 4],
    [33, 0, 5],
    [400, 400, 5],
  ])("+%i −%i is %i accent cells of 5", (add, del, cells) => {
    expect(barCells(add, del)).toBe(cells);
  });
});

describe("the summary", () => {
  it("counts the files and sums their lines", () => {
    expect(changeSummary([file("a", 3, 1), file("b/c", 2, 0)], 0)).toBe("2 files · +5 −1");
    expect(changeSummary([file("a", 3, 1)], 0)).toBe("1 file · +3 −1");
  });

  it("counts the files past the listing too", () => {
    expect(changeSummary([file("a", 3, 1)], 4)).toBe("5 files · +3 −1");
  });

  it("is 'no changes' for none", () => {
    expect(changeSummary([], 0)).toBe("no changes");
  });
});

describe("the Checks heuristic", () => {
  it("passes on the last test command that is done", () => {
    const entries = [tools(call("Bash", "cargo test -p octet-hub")), { kind: "reply", key: "x", text: "ok", done: true } as Entry];
    expect(lastChecks(entries)).toEqual({ passed: true, command: "cargo test -p octet-hub" });
  });

  it("fails on the last test command that failed", () => {
    const entries = [tools(call("Bash", "npm test")), tools(call("commandExecution", "npx vitest run", "failed"))];
    expect(lastChecks(entries)).toEqual({ passed: false, command: "npx vitest run" });
  });

  it("is nothing with no test command", () => {
    expect(lastChecks([])).toBeNull();
    expect(lastChecks([{ kind: "user", key: "u", text: "cargo test" }])).toBeNull();
  });

  it("does not take a command that is not a test", () => {
    for (const command of ["cargo build", "npm run testing", "cd x && cargo test", "pytesting", "echo cargo test", "cargo testx"]) {
      expect(lastChecks([tools(call("Bash", command))])).toBeNull();
    }
    // A test command's words in another tool's target are not a run.
    expect(lastChecks([tools(call("Grep", "cargo test"))])).toBeNull();
  });

  it("knows the runners", () => {
    for (const command of [
      "cargo nextest run",
      "cargo test",
      "npm test",
      "npm run test",
      "pnpm test",
      "pnpm run test",
      "yarn test",
      "yarn run test -- x",
      "pytest -q",
      "go test ./...",
      "npx jest",
      "npx vitest",
      "jest",
      "vitest run",
    ]) {
      expect(lastChecks([tools(call("Bash", command))])).toEqual({ passed: true, command });
    }
  });

  it("takes the newest finished run, not one still running", () => {
    const entries = [tools(call("Bash", "cargo test", "failed")), tools(call("Bash", "cargo test -q"), call("Bash", "cargo test --all", "running"))];
    expect(lastChecks(entries)).toEqual({ passed: true, command: "cargo test -q" });
  });

  it("says so when none ran", () => {
    expect(NO_TESTS).toBe("No tests run in this session");
  });
});

describe("the Branch card", () => {
  it("is the branch and how far ahead of its upstream it is", () => {
    expect(branchLines({ branch: "main", upstream: "origin/main", ahead: 2 }, 3)).toEqual({ name: "main", detail: "2 ahead of origin/main" });
  });

  it("is uncommitted changes when files are listed, else clean", () => {
    expect(branchLines({ branch: "feat/x", upstream: "origin/feat/x", ahead: 0 }, 1)).toEqual({ name: "feat/x", detail: "uncommitted changes" });
    expect(branchLines({ branch: "feat/x", upstream: null, ahead: 0 }, 0)).toEqual({ name: "feat/x", detail: "clean" });
  });

  it("is detached with no branch", () => {
    expect(branchLines({ branch: null, upstream: null, ahead: 0 }, 0)).toEqual({ name: "detached", detail: "clean" });
    expect(branchLines({ ahead: 0 }, 2)).toEqual({ name: "detached", detail: "uncommitted changes" });
  });
});

describe("a path", () => {
  it("is its name and its directory", () => {
    expect(splitPath("crates/octet-hub/src/git.rs")).toEqual({ name: "git.rs", dir: "crates/octet-hub/src" });
    expect(splitPath("README.md")).toEqual({ name: "README.md", dir: "" });
    expect(splitPath("dir/")).toEqual({ name: "dir/", dir: "" });
  });
});

describe("a diff's lines", () => {
  const text = [
    "diff --git a/src/a.ts b/src/a.ts",
    "index 1111111..2222222 100644",
    "--- a/src/a.ts",
    "+++ b/src/a.ts",
    "@@ -1,3 +1,3 @@ export",
    " const a = 1;",
    "-const b = 2;",
    "+const b = 3;",
    "\\ No newline at end of file",
    "",
  ].join("\n");

  it("starts at the first hunk, with a sign and a kind per line", () => {
    expect(diffLines(text).truncated).toBe(false);
    expect(diffLines(text).lines).toEqual([
      { kind: "hunk", sign: "", code: "@@ -1,3 +1,3 @@ export" },
      { kind: "plain", sign: " ", code: "const a = 1;" },
      { kind: "del", sign: "−", code: "const b = 2;" },
      { kind: "add", sign: "+", code: "const b = 3;" },
      { kind: "plain", sign: "", code: "\\ No newline at end of file" },
    ]);
  });

  it("is every line, plain, with no hunk", () => {
    expect(diffLines("diff --git a/x b/x\nold mode 100644\nnew mode 100755\n").lines).toEqual([
      { kind: "plain", sign: "", code: "diff --git a/x b/x" },
      { kind: "plain", sign: "", code: "old mode 100644" },
      { kind: "plain", sign: "", code: "new mode 100755" },
    ]);
    expect(diffLines("")).toEqual({ lines: [], truncated: false });
  });

  it("stops at the most the hub sends, and says so", () => {
    const long = `@@ -1 +1 @@\n${"+x\n".repeat(MAX_DIFF_LINES + 50)}`;
    expect(MAX_DIFF_LINES).toBe(2_000);
    expect(diffLines(long).lines).toHaveLength(MAX_DIFF_LINES);
    expect(diffLines(long).truncated).toBe(true);
    // Exactly the most is not cut.
    const full = `@@ -1 +1 @@\n${"+x\n".repeat(MAX_DIFF_LINES - 1)}`;
    expect(diffLines(full)).toMatchObject({ truncated: false });
    expect(diffLines(full).lines).toHaveLength(MAX_DIFF_LINES);
  });

  it("has its notes", () => {
    expect(CLIPPED_NOTE).toBe("The rest of this diff is on the Mac.");
    expect(BINARY_NOTE).toBe("binary file");
  });
});

describe("the file a tool entry edited", () => {
  it("is the first edit's target", () => {
    expect(editedFile(tools(call("Read", "a.ts"), call("Edit", "/w/src/b.ts"), call("Write", "/w/c.ts")))).toBe("/w/src/b.ts");
    expect(editedFile(tools(call("fileChange", "/w/d.ts")))).toBe("/w/d.ts");
    expect(editedFile(tools(call("MultiEdit", "e.ts")))).toBe("e.ts");
    expect(editedFile(tools(call("Write", "f.ts")))).toBe("f.ts");
  });

  it("is nothing for an entry that edited nothing", () => {
    expect(editedFile(tools(call("Read", "a.ts"), call("Bash", "ls")))).toBeNull();
    expect(editedFile(tools(call("Edit", "a.ts", "failed"), call("Edit", "b.ts", "running")))).toBeNull();
    expect(editedFile(tools(call("Edit", "")))).toBeNull();
    expect(editedFile({ kind: "user", key: "u", text: "Edit a.ts" })).toBeNull();
  });
});

describe("matching an edited file to the listing", () => {
  const paths = ["src/app.ts", "app.ts", "crates/a/src/lib.rs"];

  it("takes the path itself", () => {
    expect(matchFile(paths, "app.ts")).toBe("app.ts");
    expect(matchFile(paths, "src/app.ts")).toBe("src/app.ts");
  });

  it("takes an absolute path that ends in a listed one, the longest first", () => {
    expect(matchFile(paths, "/Users/me/octet/src/app.ts")).toBe("src/app.ts");
    expect(matchFile(paths, "/Users/me/octet/crates/a/src/lib.rs")).toBe("crates/a/src/lib.rs");
  });

  it("takes a path under the workspace when the repository is above it", () => {
    expect(matchFile(["apps/mobile/src/x.ts"], "src/x.ts")).toBe("apps/mobile/src/x.ts");
  });

  it("is nothing when a short target fits several paths equally", () => {
    expect(matchFile(["a/x.ts", "b/x.ts"], "x.ts")).toBeNull();
    expect(matchFile(["a/x.ts", "b/x.ts", "src/c/x.ts"], "x.ts")).toBeNull();
    expect(matchFile(["a/src/x.ts", "b/src/x.ts"], "src/x.ts")).toBeNull();
    // An absolute target names one: the longest listed path that ends it.
    expect(matchFile(["x.ts", "c/x.ts"], "/w/c/x.ts")).toBe("c/x.ts");
    // An exact path listed twice (two raw paths that clean alike) is no one file either.
    expect(matchFile(["x.ts", "x.ts"], "x.ts")).toBeNull();
  });

  it("is nothing for a file not listed, or a name that only ends alike", () => {
    expect(matchFile(paths, "/Users/me/octet/README.md")).toBeNull();
    expect(matchFile(paths, "/Users/me/octet/myapp.ts")).toBeNull();
    expect(matchFile(paths, "")).toBeNull();
  });
});

describe("firstPrompt", () => {
  const user = (text: string): Entry => ({ kind: "user", key: String(++key), text });
  const reply = (text: string): Entry => ({ kind: "reply", key: String(++key), text, done: true });

  it("is the earliest user entry, as one line: whitespace collapsed and trimmed", () => {
    expect(firstPrompt([reply("Hi"), user("  Fix   the\nlogin\t bug \r\n"), user("Then the tests")])).toBe("Fix the login bug");
  });

  it("is clipped at 72 characters, a surrogate pair never cut in two", () => {
    expect(PREFILL_CHARS).toBe(72);
    expect(firstPrompt([user("a".repeat(100))])).toBe("a".repeat(72));
    expect(firstPrompt([user(`${"b".repeat(71)}😀😀`)])).toBe(`${"b".repeat(71)}😀`);
    // A space the clip leaves at the end goes too.
    expect(firstPrompt([user(`${"c".repeat(71)} dee`)])).toBe("c".repeat(71));
  });

  it("is empty with no user entry, or one that is only space", () => {
    expect(firstPrompt([])).toBe("");
    expect(firstPrompt([reply("Hello"), tools(call("Bash", "ls"))])).toBe("");
    expect(firstPrompt([user(" \n\t ")])).toBe("");
  });
});

describe("committedText", () => {
  const ack = (message: unknown) => ({ type: "ack", req: 1, message });

  it("names the short hash and the branch", () => {
    expect(committedText(ack("Committed 1a2b3c4"), "main")).toBe("Committed 1a2b3c4 on main");
  });

  it("names only the hash on a detached HEAD", () => {
    expect(committedText(ack("Committed 1a2b3c4"), null)).toBe("Committed 1a2b3c4");
  });

  it("says Committed when the Mac gave no hash, or something that is not one", () => {
    expect(committedText(ack("Committed"), "main")).toBe("Committed on main");
    expect(committedText(ack("Committed \u001b[2Jzz"), null)).toBe("Committed");
    expect(committedText(ack(undefined), "main")).toBe("Committed on main");
  });
});

describe("commitScope", () => {
  const untracked = (path: string) => file(path, 1, 0, { status: "?" });

  it("counts the listed files, saying how many are new (untracked) when any are", () => {
    expect(commitScope([file("a", 1, 0), file("b", 2, 1, { status: "A" })], 0)).toBe("Commits 2 files");
    expect(commitScope([file("a", 1, 0)], 0)).toBe("Commits 1 file");
    expect(commitScope([file("a", 1, 0), untracked("b"), untracked("c")], 0)).toBe("Commits 3 files, including 2 new");
    expect(commitScope([untracked("b")], 0)).toBe("Commits 1 file, including 1 new");
  });

  it("is about the count when the Mac had more than it listed", () => {
    expect(commitScope([file("a", 1, 0)], 4)).toBe("Commits about 5 files");
    expect(commitScope([untracked("a"), file("b", 1, 0)], 498)).toBe("Commits about 500 files, including 1 new");
    expect(commitScope([], 3)).toBe("Commits about 3 files");
  });

  it("is nothing with nothing listed", () => {
    expect(commitScope([], 0)).toBeNull();
  });
});

describe("rowKey", () => {
  it("never makes one key of two rows, whatever a path holds", () => {
    const rows: [string, ...(string | number)[]][] = [
      ["a"],
      ["a", 0],
      ["a\n0"],
      ["a", "loading"],
      ["a\nloading"],
      ["a\u0000", 0],
      ["a", "\u00000"],
      ["a\",0"],
      ["a", '",0'],
      ["a", "0"],
    ];
    const keys = rows.map(([raw, ...part]) => rowKey(raw, ...part));
    expect(new Set(keys).size).toBe(rows.length);
    expect(rowKey("a", 3)).toBe(rowKey("a", 3));
  });
});
