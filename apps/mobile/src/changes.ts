// What the Changes screen derives from the hub's `changes` and `diff` answers and from the
// session's view: a file's bar, the header's summary, the Checks and Branch cards, a diff's lines,
// and which file a tool entry edited. Pure; the screen draws what these return.
import { label } from "./model/summary";
import type { Entry } from "./model/transcript";
import type { FileChange, FileStatus } from "./protocol";

/** Cells in a file's bar. */
export const BAR_CELLS = 5;

/** Accent cells in a file's bar: one per 8 lines changed, at least one and at most five. */
export function barCells(add: number, del: number): number {
  return Math.min(BAR_CELLS, Math.max(1, Math.ceil((add + del) / 8)));
}

/** The header's "N files · +a −d", or "no changes". Files past the listing count, but their
 * lines are only the Mac's. */
export function changeSummary(files: readonly FileChange[], more: number): string {
  const n = files.length + more;
  if (n === 0) return "no changes";
  let add = 0;
  let del = 0;
  for (const f of files) {
    add += f.add;
    del += f.del;
  }
  return `${n === 1 ? "1 file" : `${n} files`} · +${add} −${del}`;
}

/** What a status letter means, for a screen reader. */
export const STATUS_WORDS: Record<FileStatus, string> = {
  M: "modified",
  A: "added",
  D: "deleted",
  R: "renamed",
  "?": "untracked",
  U: "unmerged",
};

/** A path's last part and the directory before it ("" at the top). */
export function splitPath(path: string): { name: string; dir: string } {
  const cut = path.lastIndexOf("/", path.length - 2);
  return cut < 0 ? { name: path, dir: "" } : { name: path.slice(cut + 1), dir: path.slice(0, cut) };
}

/** Said in Checks when no call looks like a test run. */
export const NO_TESTS = "No tests run in this session";

/**
 * A HEURISTIC, not a test report: a shell command that starts with a known test runner is taken
 * for a test run. A runner behind `cd x &&`, a script, or another tool is not seen, and a run's
 * pass is only its exit status.
 */
export const TEST_COMMAND = /^(cargo (nextest run|test)|npm (run )?test|pnpm (run )?test|yarn (run )?test|pytest|go test|npx (jest|vitest)|jest|vitest)\b/;

/** The Checks card: the last test run's command and whether it passed. */
export interface Checks {
  passed: boolean;
  command: string;
}

/** The newest finished shell call in the session's view whose command matches `TEST_COMMAND`
 * (the heuristic above), or null. One still running is passed over for the run before it. */
export function lastChecks(entries: readonly Entry[]): Checks | null {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (entry?.kind !== "tools") continue;
    for (let j = entry.calls.length - 1; j >= 0; j--) {
      const call = entry.calls[j];
      if (!call || call.phase === "running" || label(call.name) !== "Bash") continue;
      const command = call.target.trim();
      if (TEST_COMMAND.test(command)) return { passed: call.phase === "done", command };
    }
  }
  return null;
}

/** The Branch card's two lines: the branch ("detached" with none), and how far ahead of its
 * upstream it is, else whether anything is uncommitted. */
export function branchLines(c: { branch?: string | null; upstream?: string | null; ahead: number }, files: number): { name: string; detail: string } {
  const name = c.branch ?? "detached";
  if (c.ahead > 0 && c.upstream) return { name, detail: `${c.ahead} ahead of ${c.upstream}` };
  return { name, detail: files > 0 ? "uncommitted changes" : "clean" };
}

/** The end of a clipped diff. */
export const CLIPPED_NOTE = "The rest of this diff is on the Mac.";
/** A binary file's diff. */
export const BINARY_NOTE = "binary file";
/** The most lines of a diff drawn: what the hub sends at most. */
export const MAX_DIFF_LINES = 2_000;

/** One drawn line of a diff: `add` and `del` lines have their sign apart from their code. */
export interface DiffLine {
  kind: "add" | "del" | "hunk" | "plain";
  sign: string;
  code: string;
}

/** A diff's lines from its first hunk on (git's header lines say nothing the row does not), or
 * every line, plain, when it has no hunk (a mode change). At most `MAX_DIFF_LINES`. */
export function diffLines(text: string): DiffLine[] {
  if (text === "") return [];
  const all = text.split("\n");
  if (all[all.length - 1] === "") all.pop();
  const first = all.findIndex((line) => line.startsWith("@@"));
  const lines = first < 0 ? all : all.slice(first);
  const out: DiffLine[] = [];
  for (const line of lines) {
    if (out.length === MAX_DIFF_LINES) break;
    if (first < 0) out.push({ kind: "plain", sign: "", code: line });
    else if (line.startsWith("@@")) out.push({ kind: "hunk", sign: "", code: line });
    else if (line.startsWith("+")) out.push({ kind: "add", sign: "+", code: line.slice(1) });
    else if (line.startsWith("-")) out.push({ kind: "del", sign: "−", code: line.slice(1) });
    else if (line.startsWith(" ")) out.push({ kind: "plain", sign: " ", code: line.slice(1) });
    else out.push({ kind: "plain", sign: "", code: line });
  }
  return out;
}

/** Tools whose target is a file they wrote. */
const EDITS: readonly string[] = ["Edit", "MultiEdit", "NotebookEdit", "Write"];

/** The file a tools entry edited, for its "view ›": the first done edit's target, or null. */
export function editedFile(entry: Entry): string | null {
  if (entry.kind !== "tools") return null;
  for (const call of entry.calls) {
    if (call.phase === "done" && call.target !== "" && EDITS.includes(label(call.name))) return call.target;
  }
  return null;
}

/**
 * The listed path that `file` (a tool's target: absolute, or relative to the workspace) names,
 * or null. The listing's paths are from the repository's top, which may be the workspace or a
 * folder above it, so the longest path that ends the target, or that the target ends, wins.
 */
export function matchFile(paths: readonly string[], file: string): string | null {
  if (file === "") return null;
  if (paths.includes(file)) return file;
  let best: string | null = null;
  for (const path of paths) {
    if (path === "") continue;
    const fits = file.endsWith(`/${path}`) || path.endsWith(`/${file}`);
    if (fits && (best === null || path.length > best.length)) best = path;
  }
  return best;
}
