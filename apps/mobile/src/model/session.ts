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
import { EMPTY_TRANSCRIPT, TranscriptDraft, type Transcript } from "./transcript";
import { field } from "./text";

/** An approval waiting, with the request as the journal recorded it (empty when only the
 * terminal UI's state frame told of it). */
export interface PendingApproval extends ApprovalInfo {
  detail: string;
}

/** The terminal UI's state without its approvals, which the view keeps (and updates from the
 * journal) in `approvals`. */
export type SessionState = Omit<StateInfo, "approvals">;

export interface SessionView {
  transcript: Transcript;
  state: SessionState | null;
  catalog: CatalogInfo | null;
  approvals: PendingApproval[];
  mode: string | null;
  model: string | null;
  usage: string | null;
  /** Why the hub refused to stream this session, when it did for good. */
  failed: string | null;
}

export const EMPTY_VIEW: SessionView = {
  transcript: EMPTY_TRANSCRIPT,
  state: null,
  catalog: null,
  approvals: [],
  mode: null,
  model: null,
  usage: null,
  failed: null,
};

/** One change to a session's view, in the order the hub sent it. */
export type Op =
  | { op: "event"; ev: KnownJournalEvent }
  /** A replay from the start begins: the journal will bring everything again. */
  | { op: "reset" }
  | { op: "state"; state: StateInfo }
  | { op: "catalog"; catalog: CatalogInfo }
  | { op: "failed"; reason: string | null };

/** Approvals waiting, at most. */
export const MAX_APPROVALS = 32;
/** Bounds on what a hub can make the phone hold. */
const TEXT_CHARS = 8_000;
const SHORT_CHARS = 512;
const DETAIL_CHARS = 16_000;
const MAX_FILES = 200;
const MAX_QUEUE = 8;
const MAX_MODELS = 500;
const MAX_MODES = 16;

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const str = (x: unknown, limit: number): string => field(x, limit);
const optStr = (x: unknown, limit: number): string | null | undefined =>
  x === undefined ? undefined : x === null ? null : typeof x === "string" ? str(x, limit) : null;

function cleanSummary(raw: unknown): ApprovalSummary {
  const s = isObj(raw) ? raw : {};
  const files = Array.isArray(s.files) ? s.files.slice(0, MAX_FILES).map((f) => str(f, SHORT_CHARS * 4)) : [];
  const out: ApprovalSummary = { action: str(s.action, TEXT_CHARS), files, scope: str(s.scope, SHORT_CHARS) };
  const command = optStr(s.command, DETAIL_CHARS);
  if (command !== undefined) out.command = command;
  const cwd = optStr(s.cwd, SHORT_CHARS * 4);
  if (cwd !== undefined) out.cwd = cwd;
  return out;
}

function cleanApprovals(raw: unknown, known: readonly PendingApproval[]): PendingApproval[] {
  if (!Array.isArray(raw)) return [];
  const out: PendingApproval[] = [];
  for (const a of raw.slice(0, MAX_APPROVALS)) {
    if (!isObj(a) || typeof a.id !== "number" || !Number.isSafeInteger(a.id)) continue;
    const id = a.id;
    if (out.some((x) => x.id === id)) continue;
    out.push({ id, summary: cleanSummary(a.summary), detail: known.find((k) => k.id === id)?.detail ?? "" });
  }
  return out;
}

function cleanGoal(raw: unknown): GoalInfo | null {
  if (!isObj(raw)) return null;
  const n = (x: unknown): number => (typeof x === "number" && Number.isFinite(x) ? x : 0);
  return { objective: str(raw.objective, TEXT_CHARS), status: str(raw.status, SHORT_CHARS), turns: n(raw.turns), max: n(raw.max) };
}

/** A state frame's payload, cleaned and bounded: it is the hub's, so untrusted. */
export function cleanState(raw: StateInfo, known: readonly PendingApproval[] = []): StateInfo & { approvals: PendingApproval[] } {
  const s: Record<string, unknown> = isObj(raw) ? raw : {};
  const queue = Array.isArray(s.queue) ? s.queue.slice(0, MAX_QUEUE).map((q) => str(q, TEXT_CHARS)) : [];
  const state: StateInfo = {
    phase: s.phase as StateInfo["phase"],
    queue,
    queued: typeof s.queued === "number" && Number.isFinite(s.queued) ? s.queued : queue.length,
    mode: s.mode as StateInfo["mode"],
    approvals: [],
  };
  const goal = cleanGoal(s.goal);
  if (s.goal !== undefined) state.goal = goal;
  const now = optStr(s.now, TEXT_CHARS);
  if (now !== undefined) state.now = now;
  return { ...state, approvals: cleanApprovals(s.approvals, known) };
}

function cleanCatalog(raw: CatalogInfo): CatalogInfo {
  const c: Record<string, unknown> = isObj(raw) ? raw : {};
  const models: ModelInfo[] = [];
  if (Array.isArray(c.models)) {
    for (const m of c.models.slice(0, MAX_MODELS)) {
      if (!isObj(m)) continue;
      models.push({
        engine: m.engine as ModelInfo["engine"],
        selection: str(m.selection, SHORT_CHARS),
        name: str(m.name, SHORT_CHARS),
        id: str(m.id, SHORT_CHARS),
      });
    }
  }
  const out: CatalogInfo = {
    modes: Array.isArray(c.modes) ? (c.modes.slice(0, MAX_MODES).filter((m) => typeof m === "string") as CatalogInfo["modes"]) : [],
    current_mode: c.current_mode as CatalogInfo["current_mode"],
    models,
  };
  if (isObj(c.current)) {
    out.current = { engine: c.current.engine as ModelInfo["engine"], selection: str(c.current.selection, SHORT_CHARS) };
  } else if (c.current === null) out.current = null;
  return out;
}

/** The same value when `a` and `b` are alike (state and catalog are small and plain data). */
const alike = (a: unknown, b: unknown): boolean => a === b || JSON.stringify(a) === JSON.stringify(b);

function approve(approvals: PendingApproval[], data: unknown): PendingApproval[] {
  if (!isObj(data) || typeof data.id !== "number" || !Number.isSafeInteger(data.id)) return approvals;
  const id = data.id;
  if (approvals.some((a) => a.id === id) || approvals.length >= MAX_APPROVALS) return approvals;
  const detail = typeof data.detail === "string" ? str(data.detail, DETAIL_CHARS) : "";
  return [...approvals, { id, summary: cleanSummary(data.summary), detail }];
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
        draft = new TranscriptDraft(EMPTY_TRANSCRIPT);
        if (v.approvals.length > 0) v.approvals = [];
        v.failed = null;
        break;
      case "failed":
        v.failed = op.reason;
        break;
      case "state": {
        const state = cleanState(op.state, v.approvals);
        const { approvals, ...rest } = state;
        if (!alike(v.state, rest)) v.state = rest;
        if (!alike(v.approvals, approvals)) v.approvals = approvals;
        break;
      }
      case "catalog": {
        const catalog = cleanCatalog(op.catalog);
        if (!alike(v.catalog, catalog)) v.catalog = catalog;
        break;
      }
      case "event": {
        const ev = op.ev;
        switch (ev.type) {
          case "approval":
            v.approvals = approve(v.approvals, ev.data);
            break;
          case "approval_closed":
            if (v.approvals.some((a) => a.id === ev.data)) v.approvals = v.approvals.filter((a) => a.id !== ev.data);
            break;
          case "stopped":
            if (v.approvals.length > 0) v.approvals = []; // nothing would answer them
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
    v.mode === view.mode &&
    v.model === view.model &&
    v.usage === view.usage &&
    v.failed === view.failed;
  return same ? view : v;
}
