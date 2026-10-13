// The TUI's words for tool calls (crates/octet-tui/src/view/transcript.rs): `label`, `summary`
// and the rows of a tools entry. Plain TypeScript; the screens only draw what these return.
import type { ToolData } from "../protocol";

export type ToolCall = ToolData;

/** Rows of a failed call kept when collapsed. */
export const FAILED_ROWS = 8;

/** A tool's name as the transcript says it. */
export function label(name: string): string {
  switch (name) {
    case "commandExecution":
      return "Bash";
    case "fileChange":
      return "Edit";
    case "mcpToolCall":
      return "MCP";
    default:
      return name;
  }
}

/** The name calls that read alike in words are counted under. */
function group(name: string): string {
  const named = label(name);
  if (named === "MultiEdit" || named === "NotebookEdit") return "Edit";
  if (named === "Glob") return "Grep";
  return named;
}

const plural = (n: number, one: string, many: string): string => (n === 1 ? `1 ${one}` : `${n} ${many}`);

/** The calls in words, by name in order of first appearance: "Read 3 files · Ran 1 command". */
export function summary(calls: readonly ToolCall[]): string {
  const counts = new Map<string, number>();
  for (const call of calls) {
    const name = group(call.name);
    counts.set(name, (counts.get(name) ?? 0) + 1);
  }
  const words: string[] = [];
  for (const [name, n] of counts) {
    switch (name) {
      case "Read":
        words.push(`Read ${plural(n, "file", "files")}`);
        break;
      case "Edit":
        words.push(`Edited ${plural(n, "file", "files")}`);
        break;
      case "Write":
        words.push(`Wrote ${plural(n, "file", "files")}`);
        break;
      case "Bash":
        words.push(`Ran ${plural(n, "command", "commands")}`);
        break;
      case "Grep":
        words.push(n === 1 ? "Searched once" : `Searched ${n} times`);
        break;
      default:
        words.push(`${name} ×${n}`);
    }
  }
  return words.join(" · ");
}

/** Rust's `str::lines`: split on newlines, drop a final empty line and each line's `\r`. */
export function lines(text: string): string[] {
  if (text === "") return [];
  const all = text.split("\n");
  if (all[all.length - 1] === "") all.pop();
  return all.map((line) => (line.endsWith("\r") ? line.slice(0, -1) : line));
}

/** One row group of a tools entry: a head line, then indented detail lines. */
export interface ToolRow {
  kind: "summary" | "done" | "running" | "failed";
  head: string;
  /** Detail lines. */
  lines: string[];
  /** Detail lines left out before `lines`, shown as "+N more lines". */
  more: number;
}

/** A call's first row: its mark, name and target, and "stopped" when its turn ended mid-call. */
function head(call: ToolCall, mark: string, over: boolean): string {
  const stopped = call.phase === "running" && over ? " · stopped" : "";
  return `${mark} ${label(call.name)} ${call.target}${stopped}`.trimEnd();
}

/** A call's detail lines; with `keep`, only the last that many and a count of the rest. */
function detail(text: string, keep?: number): { lines: string[]; more: number } {
  const all = lines(text);
  const skip = keep === undefined ? 0 : Math.max(0, all.length - keep);
  return { lines: all.slice(skip), more: skip };
}

/** The text of a count of hidden lines. */
export const moreLines = (n: number): string => `+${n} more lines`;

/**
 * A tools entry collapsed: a summary of the done calls, a row per running call, and each failure
 * open, cut to its last `FAILED_ROWS` lines.
 */
export function toolRows(calls: readonly ToolCall[], over: boolean): ToolRow[] {
  const rows: ToolRow[] = [];
  const done = calls.filter((c) => c.phase === "done");
  if (done.length > 0) rows.push({ kind: "summary", head: summary(done), lines: [], more: 0 });
  for (const call of calls) {
    if (call.phase === "running") rows.push({ kind: "running", head: head(call, "…", over), lines: [], more: 0 });
  }
  for (const call of calls) {
    if (call.phase === "failed") {
      rows.push({ kind: "failed", head: head(call, "✗", over), ...detail(call.detail, FAILED_ROWS) });
    }
  }
  return rows;
}

/** A tools entry expanded: every call with its mark and whole detail. */
export function callRows(calls: readonly ToolCall[], over: boolean): ToolRow[] {
  return calls.map((call) => {
    const mark = call.phase === "done" ? "✓" : call.phase === "failed" ? "✗" : "…";
    return { kind: call.phase, head: head(call, mark, over), ...detail(call.detail) };
  });
}
