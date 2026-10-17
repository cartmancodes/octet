// The hub's answers to `changes` and `diff`, cleaned before a request resolves with them: every
// string is the hub's (git's bytes, a branch anyone could have named), so each is cleaned of
// escapes and control characters and clipped the way a listed session's fields are, and every
// number is a whole one, at least 0. parseFrame (frames.ts) has checked the shape the client needs.
// A listed file keeps the hub's own path too, never drawn: the hub gives a diff only of a path
// exactly as it listed it.
import type { ChangesFrame, DiffFrame, FileChange, FileStatus } from "../protocol";
import { FIELD_CHARS, SHORT_CHARS, TEXT, field, sanitize } from "../model/text";

/** The most files a `changes` answer holds (the hub's own limit); the rest are counted. */
export const MAX_FILES = 500;
/** The longest path the hub lists, in characters (its own limit); a longer one is not a file. */
export const MAX_PATH_CHARS = FIELD_CHARS;

/** A listed file: `path` cleaned, for showing and matching; `raw` the hub's own, exactly, for
 * asking for its diff and for telling files apart. Never draw `raw`. */
export type ListedFile = FileChange & { raw: string };

/** A `changes` answer cleaned, its files with their raw paths. */
export type ChangesAnswer = Omit<ChangesFrame, "files"> & { files: ListedFile[] };

const STATUSES: readonly string[] = ["M", "A", "D", "R", "?", "U"] satisfies FileStatus[];

/** `n` as a whole number, at least 0; anything that is not a finite number is 0. */
export function count(n: unknown): number {
  if (typeof n !== "number" || !Number.isFinite(n)) return 0;
  return Math.min(Number.MAX_SAFE_INTEGER, Math.max(0, Math.floor(n)));
}

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);

/** A listed file cleaned, or null for one without a path or with one longer than the hub lists.
 * A status the phone does not know is shown as `?`. */
function cleanFile(item: unknown): ListedFile | null {
  if (!isObj(item) || typeof item.path !== "string" || item.path.length > MAX_PATH_CHARS) return null;
  const status = typeof item.status === "string" && STATUSES.includes(item.status) ? (item.status as FileStatus) : "?";
  return {
    path: field(item.path, FIELD_CHARS),
    raw: item.path,
    add: count(item.add),
    del: count(item.del),
    binary: item.binary === true,
    status,
  };
}

/** A branch or upstream name cleaned, or null for none. */
const name = (raw: unknown): string | null => (typeof raw === "string" ? field(raw, SHORT_CHARS) : null);

/** A `changes` answer cleaned. */
export function cleanChanges(frame: ChangesFrame): ChangesAnswer {
  const raw = frame.files as unknown[];
  const files: ListedFile[] = [];
  let dropped = 0;
  for (const item of raw) {
    const f = cleanFile(item);
    if (f === null) continue;
    if (files.length < MAX_FILES) files.push(f);
    else dropped += 1;
  }
  return {
    v: frame.v,
    type: "changes",
    req: frame.req,
    branch: name(frame.branch),
    upstream: name(frame.upstream),
    ahead: count(frame.ahead),
    behind: count(frame.behind),
    files,
    more: count(frame.more) + dropped,
  };
}

/** A `diff` answer cleaned: its text keeps its newlines (the hub bounds it at 64 KiB). */
export function cleanDiff(frame: DiffFrame): DiffFrame {
  return {
    v: frame.v,
    type: "diff",
    req: frame.req,
    path: field(frame.path, FIELD_CHARS),
    text: sanitize(frame.text, TEXT).out,
    clipped: frame.clipped === true,
    binary: frame.binary === true,
  };
}
