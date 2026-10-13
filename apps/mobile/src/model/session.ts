// What the phone knows of one session: its transcript, the terminal UI's state and catalog, the
// approvals waiting, and the few journal records that are not transcript (mode, model, usage).
import type {
  ApprovalInfo,
  ApprovalSummary,
  CatalogInfo,
  GoalInfo,
  KnownJournalEvent,
  ModelInfo,
  StateInfo,
} from "../protocol";
import { EMPTY_TRANSCRIPT, TranscriptDraft, type Entry, type Transcript } from "./transcript";
import { ENGINES, MODES, PHASES, count, member } from "./enums";
import { SHORT_CHARS, field, fieldCut } from "./text";

/** An approval waiting, with the request as the journal recorded it (empty when only the
 * terminal UI's state frame told of it). */
export interface PendingApproval extends ApprovalInfo {
  detail: string;
  /** The command or the detail is not whole: the phone cut it to fit, or the Mac sent it
   * shortened. The sheet says so, since Allow runs what the Mac has. */
  cut?: boolean;
  /** The journal's record of it, whole, came: a state frame's shorter summary does not replace it. */
  journaled?: boolean;
}

/** The terminal UI's state without its approvals, which the view keeps (and updates from the
 * journal) in `approvals`. */
export type SessionState = Omit<StateInfo, "approvals">;

/** How the last turn ended, as the terminal UI's mascot tells it (crates/octet-tui/src/app.rs):
 * a completed turn finished, an interrupted one stopped, anything else (or an error) failed. */
export type TurnEnd = "finished" | "failed" | "stopped";

export interface SessionView {
  transcript: Transcript;
  state: SessionState | null;
  catalog: CatalogInfo | null;
  approvals: PendingApproval[];
  /** The approvals the latest state frame listed, less those closed since. A replay from the
   * start keeps them on screen: the state frame and the replay's start reach the phone in
   * either order, and the approval's own record can be far down a long journal. */
  stated: PendingApproval[];
  mode: string | null;
  model: string | null;
  usage: string | null;
  /** Why the hub refused to stream this session, when it did for good. */
  failed: string | null;
  /** The session left the hub's list; this is what it left behind, held until released. */
  ended: boolean;
  /** The last `MAX_CLOSED` approvals closed, so that a stale state frame cannot bring one back. */
  closed: number[];
  /** The journals the hub listed for this session (newest last), plus any a record came from.
   * A new vendor process starts a new journal, and reuses approval ids: when this list changes,
   * `closed` is forgotten. */
  journals: string[];
  /** A replay from the start began and has brought no record yet. */
  replaying: boolean;
  /** While `replaying`, the entries shown before it began, so the screen does not flash empty
   * between the reset and the replay's first page; null otherwise. */
  held: Entry[] | null;
  /** Moves on at each replay from the start: what the screen keeps by entry key (the groups
   * opened) is forgotten, since keys start over. */
  epoch: number;
  /** How the last turn ended; null before one ends and while the next runs. */
  turn: TurnEnd | null;
}

export const EMPTY_VIEW: SessionView = {
  transcript: EMPTY_TRANSCRIPT,
  state: null,
  catalog: null,
  approvals: [],
  stated: [],
  mode: null,
  model: null,
  usage: null,
  failed: null,
  ended: false,
  closed: [],
  journals: [],
  replaying: false,
  held: null,
  epoch: 0,
  turn: null,
};

/** One change to a session's view, in the order the hub sent it. */
export type Op =
  | { op: "event"; ev: KnownJournalEvent; journal?: string }
  /** The hub's `subscribed` frame listed these journals (oldest first). */
  | { op: "journals"; journals: readonly string[] }
  /** A replay from the start begins: the journal will bring everything again. */
  | { op: "reset" }
  /** The replay is over (the hub acked the subscribe): a hold still up ends, empty or not. */
  | { op: "replayed" }
  | { op: "state"; state: StateInfo }
  | { op: "catalog"; catalog: CatalogInfo }
  | { op: "failed"; reason: string | null }
  | { op: "ended"; ended: boolean };

/** Approvals waiting, at most. */
export const MAX_APPROVALS = 32;
/** Closed approval IDs remembered. */
export const MAX_CLOSED = 256;
/** Journal ids remembered per session. */
export const MAX_JOURNALS = 64;
/** Bounds on what a hub can make the phone hold. */
const TEXT_CHARS = 8_000;
const DETAIL_CHARS = 16_000;
const MAX_FILES = 200;
const MAX_QUEUE = 8;
const MAX_MODELS = 500;
const MAX_MODES = 16;

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const str = (x: unknown, limit: number): string => field(x, limit);
const optStr = (x: unknown, limit: number): string | null | undefined =>
  x === undefined ? undefined : x === null ? null : typeof x === "string" ? str(x, limit) : null;

/** How the Mac marks a string it shortened: the terminal UI's state frame cuts an approval's
 * command to 4 KiB and ends it with this (`clip`, crates/octet-tui/src/bridge.rs). */
const HUB_CLIP_MARK = "…";

/** A summary cleaned and bounded, and whether its command is not whole. */
function cleanSummary(raw: unknown): { summary: ApprovalSummary; cut: boolean } {
  const s = isObj(raw) ? raw : {};
  const files = Array.isArray(s.files) ? s.files.slice(0, MAX_FILES).map((f) => str(f, SHORT_CHARS * 4)) : [];
  const out: ApprovalSummary = { action: str(s.action, TEXT_CHARS), files, scope: str(s.scope, SHORT_CHARS) };
  let cut = false;
  if (typeof s.command === "string") {
    const command = fieldCut(s.command, DETAIL_CHARS);
    out.command = command.text;
    cut = command.cut || command.text.endsWith(HUB_CLIP_MARK);
  } else if (s.command !== undefined) out.command = null;
  const cwd = optStr(s.cwd, SHORT_CHARS * 4);
  if (cwd !== undefined) out.cwd = cwd;
  return { summary: out, cut };
}

function cleanApprovals(raw: unknown, known: readonly PendingApproval[], closed: readonly number[]): PendingApproval[] {
  if (!Array.isArray(raw)) return [];
  const out: PendingApproval[] = [];
  for (const a of raw.slice(0, MAX_APPROVALS)) {
    if (!isObj(a) || typeof a.id !== "number" || !Number.isSafeInteger(a.id)) continue;
    const id = a.id;
    if (out.some((x) => x.id === id) || closed.includes(id)) continue;
    const had = known.find((k) => k.id === id);
    if (had?.journaled) {
      out.push(had); // the journal's whole request, not the state frame's shortened summary
      continue;
    }
    const { summary, cut } = cleanSummary(a.summary);
    out.push({ id, summary, detail: had?.detail ?? "", cut });
  }
  return out;
}

function cleanGoal(raw: unknown): GoalInfo | null {
  if (!isObj(raw)) return null;
  return { objective: str(raw.objective, TEXT_CHARS), status: str(raw.status, SHORT_CHARS), turns: count(raw.turns), max: count(raw.max) };
}

/** A state frame's payload, cleaned and bounded: it is the hub's, so untrusted. */
export function cleanState(
  raw: StateInfo,
  known: readonly PendingApproval[] = [],
  closed: readonly number[] = [],
): StateInfo & { approvals: PendingApproval[] } {
  const s: Record<string, unknown> = isObj(raw) ? raw : {};
  const queue = Array.isArray(s.queue) ? s.queue.slice(0, MAX_QUEUE).map((q) => str(q, TEXT_CHARS)) : [];
  const state: StateInfo = {
    phase: member(PHASES, s.phase) ?? "idle",
    queue,
    queued: count(s.queued),
    mode: member(MODES, s.mode) ?? "ask",
    approvals: [],
  };
  const goal = cleanGoal(s.goal);
  if (s.goal !== undefined) state.goal = goal;
  const now = optStr(s.now, TEXT_CHARS);
  if (now !== undefined) state.now = now;
  return { ...state, approvals: cleanApprovals(s.approvals, known, closed) };
}

function cleanCatalog(raw: CatalogInfo): CatalogInfo {
  const c: Record<string, unknown> = isObj(raw) ? raw : {};
  const models: ModelInfo[] = [];
  if (Array.isArray(c.models)) {
    for (const m of c.models.slice(0, MAX_MODELS)) {
      const engine = isObj(m) ? member(ENGINES, m.engine) : null;
      if (!isObj(m) || engine === null) continue;
      models.push({
        engine,
        selection: str(m.selection, SHORT_CHARS),
        name: str(m.name, SHORT_CHARS),
        id: str(m.id, SHORT_CHARS),
      });
    }
  }
  const out: CatalogInfo = {
    modes: Array.isArray(c.modes) ? c.modes.slice(0, MAX_MODES).flatMap((m) => member(MODES, m) ?? []) : [],
    current_mode: member(MODES, c.current_mode) ?? "ask",
    models,
  };
  const current = isObj(c.current) ? member(ENGINES, c.current.engine) : null;
  if (isObj(c.current) && current !== null) out.current = { engine: current, selection: str(c.current.selection, SHORT_CHARS) };
  else if (c.current !== undefined) out.current = null;
  return out;
}

/** The same value when `a` and `b` are alike (state and catalog are small and plain data). */
const alike = (a: unknown, b: unknown): boolean => a === b || JSON.stringify(a) === JSON.stringify(b);

/** `closed` with `ids` added, the oldest forgotten past `MAX_CLOSED`. */
function remember(closed: number[], ids: number[]): number[] {
  const next = closed.filter((id) => !ids.includes(id)).concat(ids);
  return next.length > MAX_CLOSED ? next.slice(next.length - MAX_CLOSED) : next;
}

/** A journal list change means a new vendor process, which may reuse approval ids the old one
 * closed: forget `closed`, and take `journals`. */
function setJournals(v: SessionView, journals: string[]): void {
  if (journals.length === v.journals.length && journals.every((j, i) => j === v.journals[i])) return;
  v.journals = journals.slice(-MAX_JOURNALS);
  if (v.closed.length > 0) v.closed = [];
}

function approve(approvals: PendingApproval[], data: unknown): PendingApproval[] {
  if (!isObj(data) || typeof data.id !== "number" || !Number.isSafeInteger(data.id)) return approvals;
  const id = data.id;
  const detail = fieldCut(typeof data.detail === "string" ? data.detail : "", DETAIL_CHARS);
  const { summary, cut } = cleanSummary(data.summary);
  const made: PendingApproval = { id, summary, detail: detail.text, cut: cut || detail.cut, journaled: true };
  const at = approvals.findIndex((a) => a.id === id);
  if (at >= 0) {
    // Listed already, from a state frame that told only the summary: the journal's record fills it in.
    const next = approvals.slice();
    next[at] = made;
    return alike(approvals[at], made) ? approvals : next;
  }
  return approvals.length >= MAX_APPROVALS ? approvals : [...approvals, made];
}

/**
 * The view after `ops`, in order. Returns `view` itself when nothing changed, and keeps the
 * identity of every part that did not: the transcript's entries, an unchanged state or catalog.
 */
export function reduceView(view: SessionView, ops: readonly Op[]): SessionView {
  const v: SessionView = { ...view };
  let draft: TranscriptDraft | null = null;
  const own = (): TranscriptDraft => (draft ??= new TranscriptDraft(v.transcript));

  for (const op of ops) {
    switch (op.op) {
      case "reset":
        if (!v.replaying) {
          // What the screen showed (this batch's earlier records included) stays up until the replay brings something.
          const shown = draft ? (draft as TranscriptDraft).finish() : v.transcript;
          v.held = shown.entries.length > 0 ? shown.entries : null;
          v.replaying = true;
        }
        draft = new TranscriptDraft(EMPTY_TRANSCRIPT);
        v.epoch += 1;
        v.turn = null;
        // What the latest state frame listed is still waiting; the replay fills it in or closes it.
        if (!alike(v.approvals, v.stated)) v.approvals = v.stated;
        if (v.closed.length > 0) v.closed = [];
        v.failed = null;
        break;
      case "journals":
        setJournals(
          v,
          op.journals.filter((j): j is string => typeof j === "string").map((j) => str(j, SHORT_CHARS)),
        );
        break;
      case "replayed":
        if (v.replaying) {
          v.replaying = false;
          v.held = null;
        }
        break;
      case "ended":
        v.ended = op.ended;
        break;
      case "failed":
        v.failed = op.reason;
        break;
      case "state": {
        const state = cleanState(op.state, v.approvals, v.closed);
        const { approvals, ...rest } = state;
        if (!alike(v.state, rest)) v.state = rest;
        if (!alike(v.approvals, approvals)) v.approvals = approvals;
        if (!alike(v.stated, approvals)) v.stated = approvals;
        break;
      }
      case "catalog": {
        const catalog = cleanCatalog(op.catalog);
        if (!alike(v.catalog, catalog)) v.catalog = catalog;
        break;
      }
      case "event": {
        const ev = op.ev;
        if (op.journal !== undefined) {
          const journal = str(op.journal, SHORT_CHARS);
          if (!v.journals.includes(journal)) setJournals(v, [...v.journals, journal]); // one the hub did not list yet
        }
        if (v.replaying) {
          v.replaying = false;
          v.held = null;
        }
        switch (ev.type) {
          case "approval":
            v.approvals = approve(v.approvals, ev.data);
            if (isObj(ev.data) && v.closed.includes(ev.data.id as number)) v.closed = v.closed.filter((id) => id !== ev.data.id);
            break;
          case "approval_closed":
            if (typeof ev.data !== "number" || !Number.isSafeInteger(ev.data)) break;
            v.closed = remember(v.closed, [ev.data]);
            if (v.approvals.some((a) => a.id === ev.data)) v.approvals = v.approvals.filter((a) => a.id !== ev.data);
            if (v.stated.some((a) => a.id === ev.data)) v.stated = v.stated.filter((a) => a.id !== ev.data);
            break;
          case "stopped":
            if (v.turn !== "failed") v.turn = "stopped";
            if (v.approvals.length > 0) {
              v.closed = remember(v.closed, v.approvals.map((a) => a.id)); // nothing would answer them
              v.approvals = [];
            }
            if (v.stated.length > 0) v.stated = [];
            own().apply(ev);
            break;
          case "mode":
            if (typeof ev.data === "string") v.mode = str(ev.data, SHORT_CHARS);
            break;
          case "model_selected":
            if (typeof ev.data === "string") v.model = str(ev.data, SHORT_CHARS);
            break;
          case "usage":
            if (typeof ev.data === "string") v.usage = str(ev.data, SHORT_CHARS);
            break;
          case "started":
          case "ready":
            v.turn = null;
            own().apply(ev);
            break;
          case "finished":
            v.turn = ev.data === "completed" ? "finished" : ev.data === "interrupted" ? "stopped" : "failed";
            own().apply(ev);
            break;
          case "error":
            v.turn = "failed";
            own().apply(ev);
            break;
          default:
            own().apply(ev);
        }
        break;
      }
    }
  }
  if (draft) v.transcript = (draft as TranscriptDraft).finish();

  const same =
    v.transcript === view.transcript &&
    v.state === view.state &&
    v.catalog === view.catalog &&
    v.approvals === view.approvals &&
    v.stated === view.stated &&
    v.mode === view.mode &&
    v.model === view.model &&
    v.usage === view.usage &&
    v.failed === view.failed &&
    v.ended === view.ended &&
    v.closed === view.closed &&
    v.journals === view.journals &&
    v.replaying === view.replaying &&
    v.held === view.held &&
    v.epoch === view.epoch &&
    v.turn === view.turn;
  return same ? view : v;
}
