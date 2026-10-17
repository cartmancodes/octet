// A Mac's listed sessions: each row cleaned and keyed by its Mac, and reconciled with the last list.
import { MAX_SESSION_ROWS, type ApprovalInfo, type SessionInfo } from "../protocol";
import { splitKey } from "../macs";
import { ENGINES, MODES, PHASES, count, member } from "../model/enums";
import { cleanFrontApproval } from "../model/session";
import { FIELD_CHARS, field } from "../model/text";

/** A listed session. Its `id` is the store's key, `"<mac>/<session>"`; `raw` is the hub's own id,
 * which goes only on the wire, and `mac` the phone's id for the Mac that listed it. */
export type ListedSession = SessionInfo & { mac: string; raw: string };

export const FIELD = FIELD_CHARS;
/** The longest usage line kept: the hub clips it to this too. */
const USAGE_CHARS = 200;

export const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);

function cleanSession(raw: unknown): SessionInfo | null {
  if (!isObj(raw) || typeof raw.id !== "string" || raw.id === "") return null;
  const optional = (x: unknown): string | null | undefined =>
    x === undefined ? undefined : typeof x === "string" ? field(x, FIELD) : null;
  const engine = member(ENGINES, raw.engine);
  if (engine === null) return null; // a session this app cannot drive
  const info: SessionInfo = {
    id: field(raw.id, FIELD),
    engine,
    workspace: field(raw.workspace, FIELD),
    mode: member(MODES, raw.mode) ?? "ask",
    phase: member(PHASES, raw.phase) ?? "idle",
    approvals: count(raw.approvals),
    from_phone: raw.from_phone === true,
  };
  for (const key of ["model", "goal", "now", "tmux"] as const) {
    const value = optional(raw[key]);
    if (value !== undefined) info[key] = value;
  }
  if (typeof raw.last_activity === "number" || raw.last_activity === null) info.last_activity = raw.last_activity;
  const approval = cleanFrontApproval(raw.approval);
  if (approval) info.approval = approval;
  if (raw.usage !== undefined) info.usage = typeof raw.usage === "string" ? field(raw.usage, USAGE_CHARS) : null;
  // A heuristic flag: only `true` says anything, so nothing else is kept.
  if (raw.question === true) info.question = true;
  return info;
}

/** A row of Mac `mac`, its id a key of that Mac; null for one that is not a session. */
function cleanRow(mac: string, raw: unknown): ListedSession | null {
  const info = cleanSession(raw);
  const split = info ? splitKey(info.id) : null;
  if (!info || split?.mac !== mac) return null;
  return { ...info, mac, raw: split.raw };
}

/** Two cleaned approvals say the same (their summaries are small and bounded). */
const sameApproval = (a: ApprovalInfo | null | undefined, b: ApprovalInfo | null | undefined): boolean =>
  a === b || (!!a && !!b && a.id === b.id && JSON.stringify(a.summary) === JSON.stringify(b.summary));

const sameSession = (a: ListedSession, b: ListedSession): boolean => {
  const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
  for (const k of keys) {
    if (k === "approval") continue;
    if ((a as unknown as Record<string, unknown>)[k] !== (b as unknown as Record<string, unknown>)[k]) return false;
  }
  return sameApproval(a.approval, b.approval);
};

/** `answered` once kept to what is still asking: other Macs' marks, and those of this Mac that
 * `asking` (a session listed still asking) holds. The same set when nothing left it. */
function settle(mac: string, answered: ReadonlySet<string>, asking: (id: string) => boolean): ReadonlySet<string> {
  const prefix = `${mac}/`;
  let kept: Set<string> | null = null;
  for (const id of answered) {
    if (!id.startsWith(prefix) || asking(id)) continue;
    kept ??= new Set(answered);
    kept.delete(id);
  }
  return kept ?? answered;
}

/** `answered` after a sessions frame of Mac `mac` that a later one in the same batch replaces: its
 * rows are not cleaned or kept, only read for the marks of this Mac, which leave the set as
 * `reconcile` would let them go (the row no longer asks, or is not listed), so that a question
 * after the Mac caught up still shows. O(rows) only while one of this Mac's marks is out. */
export function answeredAfter(mac: string, raw: unknown[], answered: ReadonlySet<string>): ReadonlySet<string> {
  const prefix = `${mac}/`;
  if (![...answered].some((id) => id.startsWith(prefix))) return answered;
  const asking = new Set<string>();
  for (const item of raw.slice(0, MAX_SESSION_ROWS)) {
    if (isObj(item) && typeof item.id === "string" && item.question === true) asking.add(field(item.id, FIELD));
  }
  return settle(mac, answered, (id) => asking.has(id));
}

/** The front approval `old` showed, for its row `info` that came without one. Under pressure the hub
 * leaves a row's `approval` out and keeps its count (`SessionsFrame`, protocol.ts): what is not sent
 * is not cleared. The count decides: none waiting, no front; fewer than before, the front was likely
 * answered, so none is kept. */
function keptFront(old: ListedSession | undefined, info: SessionInfo, raw: Record<string, unknown>): ApprovalInfo | undefined {
  if (raw.approval !== undefined || !old?.approval || info.approvals <= 0 || info.approvals < old.approvals) return undefined;
  return old.approval;
}

/** Mac `mac`'s listed sessions, reusing the previous object of each session that did not change,
 * and the previous array when none did. A row's field the hub left out is not sent, which for most
 * fields is the same as none (a Mac sends no null for a NOW line or goal that went), but a front
 * approval's gist it knew is kept while the count says it still waits (`keptFront`). A session in
 * `answered` that still says question is shown as not asking, and stays in the set; one of this Mac
 * whose row no longer asks (the Mac caught up), or that is not listed, leaves it. Other Macs' marks
 * are kept. At most `MAX_SESSION_ROWS`, the hub's own bound. */
export function reconcile(
  mac: string,
  prev: ListedSession[],
  raw: unknown[],
  answered: ReadonlySet<string>,
): { sessions: ListedSession[]; answered: ReadonlySet<string> } {
  const before = new Map(prev.map((s) => [s.id, s]));
  const next: ListedSession[] = [];
  const seen = new Set<string>();
  const asking = new Set<string>();
  for (const item of raw.slice(0, MAX_SESSION_ROWS)) {
    const info = cleanRow(mac, item);
    if (!info || seen.has(info.id)) continue;
    seen.add(info.id);
    if (info.question && answered.has(info.id)) {
      delete info.question;
      asking.add(info.id);
    }
    const old = before.get(info.id);
    const front = keptFront(old, info, item as Record<string, unknown>);
    if (front) info.approval = front;
    next.push(old && sameSession(old, info) ? old : info);
  }
  return {
    sessions: next.length === prev.length && next.every((s, i) => s === prev[i]) ? prev : next,
    answered: settle(mac, answered, (id) => asking.has(id)),
  };
}

/** Every Mac's rows, Mac after Mac. */
export const merged = (slices: ReadonlyMap<string, ListedSession[]>): ListedSession[] => {
  const all: ListedSession[] = [];
  for (const rows of slices.values()) for (const row of rows) all.push(row);
  return all;
};
