// The app's state: the sessions each paired Mac's hub lists and, for each, what the phone knows of
// it. Every session is keyed `"<mac>/<session>"` (src/macs.ts): the hub's raw id is only on the
// wire. A zustand store without React in it; the hooks at the bottom select parts of it so a
// screen re-renders only for what it shows. The batches of every Mac's client go through one
// coalescer (src/coalesce.ts) and are applied in one `set`, and every part that a batch did not
// change keeps its identity.
import { useMemo } from "react";
import { useShallow } from "zustand/react/shallow";
import { createStore, useStore, type StoreApi } from "zustand";

import type { BatchItem, HubClient, HubState } from "./client/HubClient";
import { createCoalescer, type Batch, type Coalescer, type Sink } from "./coalesce";
import { keyOf, macOfKey, rawOf, splitKey, type Links } from "./macs";
import { ENGINES, MODES, PHASES, count, member } from "./model/enums";
import {
  EMPTY_VIEW,
  cleanFrontApproval,
  reduceView,
  type Op,
  type PendingApproval,
  type SessionState,
  type SessionView,
  type TurnEnd,
} from "./model/session";
import { clearParsed } from "./markdown/blocks";
import { hideToastOf } from "./toast";
import type { Entry } from "./model/transcript";
import { FIELD_CHARS, field } from "./model/text";
import { isKnownEvent, type ApprovalInfo, type CatalogInfo, type SessionInfo } from "./protocol";
import type { SessionHead } from "./session";

/** A listed session. Its `id` is the store's key, `"<mac>/<session>"`; `raw` is the hub's own id,
 * which goes only on the wire, and `mac` the phone's id for the Mac that listed it. */
export type ListedSession = SessionInfo & { mac: string; raw: string };

export interface OctetState {
  /** The live sessions of every Mac, each Mac's as it listed them last, Mac after Mac. */
  sessions: ListedSession[];
  /** The same rows by Mac: a Mac's sessions frame replaces only its own. */
  slices: ReadonlyMap<string, ListedSession[]>;
  /** What the phone knows of each session it has heard of, by key. A Map: the keys hold the
   * hub's ids. */
  bySession: ReadonlyMap<string, SessionView>;
  /** The link to each Mac. `bestHub` gives the app's. */
  hubs: ReadonlyMap<string, HubState>;
  /** The Macs whose sessions frame came since the store began or was reset: until a Mac's comes,
   * its having no rows means "not told yet", not "none". */
  listed: ReadonlySet<string>;
  /** Applies one Mac's frames, their sessions already keyed by it, in order, in one commit.
   * Returns the sessions that just ended (they were listed before and are not now): their views
   * stay, marked `ended`, until the caller releases them. A frame naming another Mac's session
   * is left out, and so is a listed row of another Mac's: a frame of one Mac never makes or
   * changes another's keys. */
  applyBatch(mac: string, frames: readonly BatchItem[]): string[];
  /** `applyBatch` for several Macs at once, in one commit; the ended sessions of each, in order.
   * A batch that cannot be applied (a fault in the store) is reported and left out whole, and
   * is null here; the others still apply. */
  applyBatches(batches: readonly Batch[]): (string[] | null)[];
  /** Drops the view of an ended session, or of a failed one the hub does not list. False for
   * any other. */
  release(session: string): boolean;
  /** The Task strip is open: one choice for every session's screen, for the app run. */
  taskOpen: boolean;
  setTaskOpen(open: boolean): void;
  setHub(mac: string, state: HubState): void;
  /** The first prompt of a session that the Mac did not take, keyed by session: the user's words,
   * kept for that session's prompt box. At most `MAX_DRAFTS`, the oldest dropped. */
  drafts: ReadonlyMap<string, string>;
  setDraft(session: string, text: string): void;
  /** The session's draft, which is then forgotten; "" when it has none. */
  takeDraft(session: string): string;
  /** The texts the Mac took from this phone, by session, newest last, for the key bar's ↑. Kept
   * in memory only: at most `MAX_SENT` a session, for at most `MAX_SENT_SESSIONS` sessions. */
  sent: ReadonlyMap<string, readonly string[]>;
  remember(session: string, text: string): void;
  /** Sessions whose question the user answered from this phone, whose rows the Mac may not have
   * caught up with: such a row's `question` is not kept. A session leaves this when a row of
   * it comes without a question (the Mac caught up), when it leaves the list, when the link to
   * its Mac is ready again, and on reset. */
  answered: ReadonlySet<string>;
  /** The user sent something to `session`: its row stops asking, at once. Nothing for a session
   * that is not asking now (a question that comes later must show). */
  markAnswered(session: string): void;
  /** The session whose prompt the Inbox's Reply asked to focus; null for none. */
  replyTo: string | null;
  setReplyTo(session: string | null): void;
  /** Forgets everything one Mac sent (a Mac unpaired, or paired again), or with no argument
   * everything every Mac sent. */
  reset(mac?: string): void;
  /** The hub refused to stream `session` for good. */
  subscribeFailed(session: string, reason: string): void;
  /** Marks a fresh start the Mac took: its divider goes before the first record of the
   * session's next journal, and again there on a replay from the start. False, and nothing made,
   * for a session the store holds no view of: a view made here would stop a later `watch` from
   * starting fresh. */
  addNote(session: string, text: string): boolean;
  /** Lets go of a live session's transcript and approvals, keeping what other screens read of it
   * (its catalog and last state). False for a session with no view, or one that ended. */
  evict(session: string): boolean;
}

export type OctetStore = StoreApi<OctetState>;

/** Sessions listed, at most, per Mac. */
const MAX_SESSIONS = 200;
/** Drafts kept, at most. */
const MAX_DRAFTS = 16;
/** Texts sent kept for ↑, at most, per session; and the sessions they are kept for. */
export const MAX_SENT = 20;
const MAX_SENT_SESSIONS = 16;
const NO_SENT: readonly string[] = [];
const NO_ANSWERED: ReadonlySet<string> = new Set();
const NO_LISTED: ReadonlySet<string> = new Set();
const NO_ROWS: ListedSession[] = [];
const FIELD = FIELD_CHARS;
/** The longest usage line kept: the hub clips it to this too. */
const USAGE_CHARS = 200;

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);

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

/** Mac `mac`'s listed sessions, reusing the previous object of each session that did not change,
 * and the previous array when none did. A session in `answered` that still says question is
 * shown as not asking, and stays in the set; one of this Mac whose row no longer asks (the Mac
 * caught up), or that is not listed, leaves it. Other Macs' marks are kept. */
function reconcile(
  mac: string,
  prev: ListedSession[],
  raw: unknown[],
  answered: ReadonlySet<string>,
): { sessions: ListedSession[]; answered: ReadonlySet<string> } {
  const prefix = `${mac}/`;
  const before = new Map(prev.map((s) => [s.id, s]));
  const next: ListedSession[] = [];
  const seen = new Set<string>();
  const kept = new Set<string>();
  for (const id of answered) if (!id.startsWith(prefix)) kept.add(id);
  for (const item of raw.slice(0, MAX_SESSIONS)) {
    const info = cleanRow(mac, item);
    if (!info || seen.has(info.id)) continue;
    seen.add(info.id);
    if (info.question && answered.has(info.id)) {
      delete info.question;
      kept.add(info.id);
    }
    const old = before.get(info.id);
    next.push(old && sameSession(old, info) ? old : info);
  }
  return {
    sessions: next.length === prev.length && next.every((s, i) => s === prev[i]) ? prev : next,
    answered: kept.size === answered.size && [...kept].every((id) => answered.has(id)) ? answered : kept,
  };
}

/** Every Mac's rows, Mac after Mac. */
const merged = (slices: ReadonlyMap<string, ListedSession[]>): ListedSession[] => {
  const all: ListedSession[] = [];
  for (const rows of slices.values()) for (const row of rows) all.push(row);
  return all;
};

/** Says so, in development, when a frame of one Mac names another's session: a fault upstream,
 * since `connectStore` keys every frame by its own Mac. */
function stray(mac: string, what: string): void {
  if (typeof __DEV__ !== "undefined" && __DEV__) console.warn(`octet: a frame of ${mac} names ${what}, which is not its own; it was left out`);
}

/** Mac `mac`'s frames without those that name another Mac's session; the same array when none
 * does. A sessions frame's rows of another Mac are left out by `cleanRow`. */
function ownFrames(mac: string, frames: readonly BatchItem[]): readonly BatchItem[] {
  const prefix = `${mac}/`;
  let kept: BatchItem[] | null = null;
  frames.forEach((frame, i) => {
    if (frame.type === "sessions") {
      if (Array.isArray(frame.sessions)) {
        for (const row of frame.sessions as unknown[]) {
          if (isObj(row) && typeof row.id === "string" && row.id !== "" && !row.id.startsWith(prefix)) stray(mac, `the row ${row.id}`);
        }
      }
    } else if ("session" in frame && !(typeof frame.session === "string" && frame.session.startsWith(prefix))) {
      stray(mac, `the session ${String(frame.session)}`);
      kept ??= frames.slice(0, i);
      return;
    }
    kept?.push(frame);
  });
  return kept ?? frames;
}

/** Whether `key` is one of Mac `mac`'s. */
const ofMac = (mac: string) => {
  const prefix = `${mac}/`;
  return (key: string): boolean => key.startsWith(prefix);
};

/** `map` without the keys `drop` picks; the same map when it has none of them. */
function without<V>(map: ReadonlyMap<string, V>, drop: (key: string) => boolean): ReadonlyMap<string, V> {
  let next: Map<string, V> | null = null;
  for (const key of map.keys()) if (drop(key)) (next ??= new Map(map)).delete(key);
  return next ?? map;
}

/** How good each link state is, for `bestHub`. */
const RANK: Record<HubState, number> = { ready: 4, authenticating: 3, connecting: 2, offline: 1, unpaired: 0 };

/** The best link of every Mac's: `ready` when any is, and `offline` with none. */
export function bestHub(state: Pick<OctetState, "hubs">): HubState {
  let best: HubState = "offline";
  let rank = -1;
  for (const hub of state.hubs.values()) {
    if (RANK[hub] > rank) {
      best = hub;
      rank = RANK[hub];
    }
  }
  return best;
}

export function createOctetStore(): OctetStore {
  return createStore<OctetState>()((set, get) => ({
    sessions: [],
    slices: new Map(),
    bySession: new Map(),
    hubs: new Map(),
    listed: NO_LISTED,
    taskOpen: false,
    drafts: new Map(),
    sent: new Map(),
    answered: NO_ANSWERED,
    replyTo: null,

    reset(mac) {
      // What the old Mac's replies parsed to. The cache is keyed by text, not by Mac, so it all
      // goes: the other Macs' replies parse again when next drawn.
      clearParsed();
      // An approval toast of the Mac (or of any Mac) points at a session that is going.
      hideToastOf(mac);
      if (mac === undefined) {
        set({
          sessions: [],
          slices: new Map(),
          bySession: new Map(),
          hubs: new Map(),
          listed: NO_LISTED,
          taskOpen: false,
          drafts: new Map(),
          sent: new Map(),
          answered: NO_ANSWERED,
          replyTo: null,
        });
        return;
      }
      const mine = ofMac(mac);
      const st = get();
      const slices = new Map(st.slices);
      slices.delete(mac);
      const listed = new Set(st.listed);
      listed.delete(mac);
      const hubs = new Map(st.hubs);
      hubs.delete(mac);
      set({
        sessions: merged(slices),
        slices,
        bySession: without(st.bySession, mine),
        hubs,
        listed,
        drafts: without(st.drafts, mine),
        sent: without(st.sent, mine),
        answered: new Set([...st.answered].filter((id) => !mine(id))),
        replyTo: st.replyTo !== null && mine(st.replyTo) ? null : st.replyTo,
      });
    },

    remember(session, text) {
      const old = get().sent.get(session) ?? NO_SENT;
      if (old[old.length - 1] === text) return; // the same again: one is enough to bring back
      const sent = new Map(get().sent);
      sent.delete(session); // the session sent to last goes last
      sent.set(session, [...old, text].slice(-MAX_SENT));
      for (const oldest of sent.keys()) {
        if (sent.size <= MAX_SENT_SESSIONS) break;
        sent.delete(oldest);
      }
      set({ sent });
    },

    setDraft(session, text) {
      const drafts = new Map(get().drafts);
      drafts.delete(session); // a fresh one goes last
      drafts.set(session, text);
      for (const oldest of drafts.keys()) {
        if (drafts.size <= MAX_DRAFTS) break;
        drafts.delete(oldest);
      }
      set({ drafts });
    },

    takeDraft(session) {
      const text = get().drafts.get(session);
      if (text === undefined) return "";
      const drafts = new Map(get().drafts);
      drafts.delete(session);
      set({ drafts });
      return text;
    },

    markAnswered(session) {
      const { slices, answered } = get();
      const mac = macOfKey(session);
      const rows = mac === null ? undefined : slices.get(mac);
      const at = rows?.findIndex((x) => x.id === session) ?? -1;
      const row = rows?.[at];
      if (mac === null || !rows || !row?.question) return;
      const { question: _asked, ...quiet } = row;
      const next = rows.slice();
      next[at] = quiet;
      const all = new Map(slices).set(mac, next);
      set({ sessions: merged(all), slices: all, answered: new Set(answered).add(session) });
    },

    setReplyTo(replyTo) {
      if (get().replyTo !== replyTo) set({ replyTo });
    },

    setTaskOpen(taskOpen) {
      if (get().taskOpen !== taskOpen) set({ taskOpen });
    },

    applyBatch(mac, frames) {
      return get().applyBatches([{ mac, frames }])[0] ?? [];
    },

    applyBatches(batches) {
      const st = get();
      let slices = st.slices;
      let answered = st.answered;
      let listed = st.listed;
      let views: Map<string, SessionView> | null = null;
      const ended: (string[] | null)[] = [];

      /** One Mac's batch over what the batches before it made. Its views go into `changed`, which
       * the caller merges only if the whole batch went through. */
      const step = (mac: string, frames: readonly BatchItem[], changed: Map<string, SessionView>): string[] => {
        const before = slices.get(mac) ?? NO_ROWS;
        let rows = before;
        let heard = false;
        const ops = new Map<string, Op[]>();
        const push = (session: string, op: Op) => {
          const list = ops.get(session);
          if (list) list.push(op);
          else ops.set(session, [op]);
        };
        for (const frame of ownFrames(mac, frames)) {
          switch (frame.type) {
            case "sessions":
              if (Array.isArray(frame.sessions)) {
                ({ sessions: rows, answered } = reconcile(mac, rows, frame.sessions, answered));
                heard = true;
              }
              break;
            case "subscribed":
              if (frame.from === "start") push(frame.session, { op: "reset" });
              push(frame.session, { op: "journals", journals: frame.journals });
              break;
            case "event":
              if (isKnownEvent(frame.event)) push(frame.session, { op: "event", ev: frame.event, journal: frame.journal });
              break;
            case "state":
              push(frame.session, { op: "state", state: frame.state });
              break;
            case "catalog":
              push(frame.session, { op: "catalog", catalog: frame.catalog });
              break;
            case "replayed":
              push(frame.session, { op: "replayed" });
              break;
            default:
              break; // replies to requests are the client's
          }
        }
        if (heard && !listed.has(mac)) listed = new Set(listed).add(mac);
        // The first list of a Mac places it among the others, rows or none.
        if (rows !== before || (heard && !slices.has(mac))) slices = new Map(slices).set(mac, rows);

        // A listed session is live; one that was listed and is not now has ended, and keeps its
        // view. One refused for good that comes into the list is a session again (it started
        // since a deep link asked for it, or started over under its id): its failure is over.
        const gone: string[] = [];
        const known = views ?? st.bySession;
        if (rows !== before) {
          const now = new Set(rows.map((s) => s.id));
          const was = new Set(before.map((s) => s.id));
          for (const s of before) {
            if (now.has(s.id)) continue;
            gone.push(s.id);
            if (known.has(s.id) || ops.has(s.id)) push(s.id, { op: "ended", ended: true });
          }
          for (const id of now) {
            const old = known.get(id);
            if (old?.ended) push(id, { op: "ended", ended: false });
            if (old?.failed != null && !was.has(id)) push(id, { op: "failed", reason: null });
          }
        }
        for (const [session, list] of ops) {
          const old = known.get(session);
          const next = reduceView(old ?? EMPTY_VIEW, list);
          if (next === old || (old === undefined && next === EMPTY_VIEW)) continue;
          changed.set(session, next);
        }
        return gone;
      };

      for (const { mac, frames } of batches) {
        const saved = { slices, answered, listed };
        const changed = new Map<string, SessionView>();
        try {
          ended.push(step(mac, frames, changed));
        } catch (error) {
          // A fault in the store, not the Mac: this batch costs only itself.
          ({ slices, answered, listed } = saved);
          console.error(`octet: a batch of ${mac} could not be applied; it was left out`, error);
          ended.push(null);
          continue;
        }
        if (changed.size > 0) {
          views ??= new Map(st.bySession);
          for (const [session, view] of changed) views.set(session, view);
        }
      }
      if (views === null && slices === st.slices && listed === st.listed && answered === st.answered) return ended;
      set({
        ...(slices !== st.slices ? { slices, sessions: merged(slices) } : {}),
        ...(views ? { bySession: views } : {}),
        ...(listed !== st.listed ? { listed } : {}),
        ...(answered !== st.answered ? { answered } : {}),
      });
      return ended;
    },

    release(session) {
      const { bySession, sessions } = get();
      const view = bySession.get(session);
      const failedUnlisted = view?.failed != null && !sessions.some((s) => s.id === session);
      if (!view?.ended && !failedUnlisted) return false;
      const views = new Map(bySession);
      views.delete(session);
      set({ bySession: views });
      return true;
    },

    setHub(mac, hub) {
      const st = get();
      if (st.hubs.get(mac) === hub) return;
      const hubs = new Map(st.hubs).set(mac, hub);
      // A link ready again brings a fresh list, which is the truth: an answer the Mac never heard
      // (the link went down first) must not hide its question.
      if (hub !== "ready") {
        set({ hubs });
        return;
      }
      const mine = ofMac(mac);
      const answered = [...st.answered].some(mine) ? new Set([...st.answered].filter((id) => !mine(id))) : st.answered;
      set({ hubs, answered: answered.size === 0 ? NO_ANSWERED : answered });
    },

    evict(session) {
      const v = get().bySession.get(session);
      if (!v || v.ended) return false;
      // A fresh start's divider is made again from its journal when the transcript comes back.
      const kept: SessionView = { ...EMPTY_VIEW, state: v.state, catalog: v.catalog, mode: v.mode, model: v.model, usage: v.usage, epoch: v.epoch + 1, fresh: v.fresh };
      set((st) => ({ bySession: new Map(st.bySession).set(session, kept) }));
      return true;
    },

    addNote(session, text) {
      const old = get().bySession.get(session);
      if (!old) return false;
      const next = reduceView(old, [{ op: "note", text: field(text, FIELD) }]);
      if (next !== old) set((s) => ({ bySession: new Map(s.bySession).set(session, next) }));
      return true;
    },

    subscribeFailed(session, reason) {
      const old = get().bySession.get(session) ?? EMPTY_VIEW;
      const next = reduceView(old, [{ op: "failed", reason: field(reason, FIELD) }]);
      if (next !== old) set((s) => ({ bySession: new Map(s.bySession).set(session, next) }));
    },
  }));
}

/** How long a session that left the hub's list is kept for its screen. */
export const ENDED_KEPT_MS = 10 * 60_000;

export interface ConnectOptions {
  /** The phone's id for the Mac the client reaches: every session it names is keyed by it. */
  mac: string;
  /** Where each Mac's client is (the Fleet's, src/hub.ts): the view budget asks it for the client
   * of each key it lets go. Read at each use, so it must follow the Macs as they change. */
  links: Links;
  /** Runs `fn` after `ms`; returns the function that cancels it. */
  schedule?: (fn: () => void, ms: number) => () => void;
  now?: () => number;
  /** The coalescer every Mac's batches go through; the store's own when none is given. */
  coalescer?: Coalescer;
}

const defaultSchedule = (fn: () => void, ms: number): (() => void) => {
  const id = setTimeout(fn, ms);
  return () => clearTimeout(id);
};

/** Each store's shared coalescer, for feeds not given one. */
const coalescers = new WeakMap<OctetStore, Coalescer>();
function coalescerOf(store: OctetStore, options: ConnectOptions): Coalescer {
  let c = coalescers.get(store);
  if (!c) {
    c = createCoalescer((batches) => store.getState().applyBatches(batches), { now: options.now, schedule: options.schedule });
    coalescers.set(store, c);
  }
  return c;
}

/** The timers that will release ended sessions, per client, by key. */
const releases = new WeakMap<HubClient, Map<string, () => void>>();
const timersOf = (client: HubClient): Map<string, () => void> => {
  let map = releases.get(client);
  if (!map) releases.set(client, (map = new Map()));
  return map;
};

const isRow = (x: unknown): x is Record<string, unknown> & { id: string } => isObj(x) && typeof x.id === "string" && x.id !== "";

/** A client's frames with every session they name keyed by `mac`. A frame whose session is not a
 * string is dropped. */
function keyFrames(mac: string, frames: readonly BatchItem[]): BatchItem[] {
  const out: BatchItem[] = [];
  for (const frame of frames) {
    if (frame.type === "sessions") {
      out.push(
        Array.isArray(frame.sessions)
          ? { ...frame, sessions: (frame.sessions as unknown[]).map((row) => (isRow(row) ? { ...row, id: keyOf(mac, row.id) } : row)) as SessionInfo[] }
          : frame,
      );
    } else if ("session" in frame) {
      if (typeof frame.session === "string") out.push({ ...frame, session: keyOf(mac, frame.session) });
    } else out.push(frame);
  }
  return out;
}

/** Feeds the store from `client`, the link to Mac `options.mac`: its batches (every session keyed
 * by that Mac, through the coalescer), its connection state, and the sessions the hub refused for
 * good (forgotten, and their views marked failed). A session that leaves the hub's list keeps its
 * view for `ENDED_KEPT_MS`, then is released and forgotten (one with no view is forgotten at
 * once). Returns the function that stops all of it. */
export function connectStore(store: OctetStore, client: HubClient, options: ConnectOptions): () => void {
  const { mac, links } = options;
  const schedule = options.schedule ?? defaultSchedule;
  const coalescer = options.coalescer ?? coalescerOf(store, options);
  const mine = ofMac(mac);
  const timers = timersOf(client);
  const cancel = (session: string) => {
    timers.get(session)?.();
    timers.delete(session);
  };
  const forget = (session: string) => {
    cancel(session);
    requested(client).delete(session);
    const raw = rawOf(session);
    if (raw !== null) client.forget(raw);
  };
  // The failed views of this Mac when a list came: only a list can end a failure.
  let failed: string[] = [];
  const sink: Sink = {
    before(frames) {
      failed = frames.some((f) => f.type === "sessions")
        ? [...store.getState().bySession].flatMap(([id, v]) => (v.failed !== null && mine(id) ? [id] : []))
        : [];
    },
    after(ended) {
      for (const session of ended) {
        cancel(session);
        // Nothing to hold for a screen: the client lets it go now rather than never.
        if (!store.getState().bySession.has(session)) {
          forget(session);
          continue;
        }
        timers.set(
          session,
          schedule(() => {
            timers.delete(session);
            if (store.getState().release(session)) forget(session);
          }, ENDED_KEPT_MS),
        );
      }
      // A session listed again is live: its timer goes.
      for (const [session] of timers) if (store.getState().bySession.get(session)?.ended === false) cancel(session);
      // A failure the list ended: the screen showing the session streams it again.
      for (const session of failed) {
        if (shown(client).has(session) && store.getState().bySession.get(session)?.failed === null) {
          // The screen already holds it: streaming again adds no hold of its own.
          stream(store, links, client, session);
        }
      }
      failed = [];
    },
  };
  const offBatch = client.onBatch((frames) => coalescer.push(mac, keyFrames(mac, frames), sink));
  const offState = client.onState((state) => store.getState().setHub(mac, state));
  const offFailed = client.onSubscribeFailed((raw, refusal) => {
    const session = keyOf(mac, raw);
    forget(session);
    store.getState().subscribeFailed(session, refusal.message);
  });
  store.getState().setHub(mac, client.state);
  return () => {
    offBatch();
    offState();
    offFailed();
    coalescer.drop(mac, sink); // a newer feed of the same Mac keeps what it has waiting
    for (const session of [...timers.keys()]) cancel(session);
  };
}

/** The client of `session`'s Mac, and its raw id; null for a key on no Mac connected. */
function linkOf(links: Links, session: string): { client: HubClient; raw: string } | null {
  const split = splitKey(session);
  const client = split ? links.client(split.mac) : null;
  return split && client ? { client, raw: split.raw } : null;
}

/** The user left the screen of an ended session: its view is dropped and its Mac's client
 * forgets it. */
export function releaseSession(store: OctetStore, links: Links, session: string): void {
  if (!store.getState().release(session)) return;
  const link = linkOf(links, session);
  if (!link) return;
  timersOf(link.client).get(session)?.();
  timersOf(link.client).delete(session);
  requested(link.client).delete(session);
  link.client.forget(link.raw);
}

/** The sessions each client was asked to stream and not since released, by key. */
const asked = new WeakMap<HubClient, Set<string>>();
const requested = (client: HubClient): Set<string> => {
  let set = asked.get(client);
  if (!set) asked.set(client, (set = new Set()));
  return set;
};

/** The sessions each client streams for a screen (watched and not since unwatched). Unlike
 * `requested`, a refusal does not take a session out: its screen still shows it. */
const screens = new WeakMap<HubClient, Set<string>>();
const shown = (client: HubClient): Set<string> => {
  let set = screens.get(client);
  if (!set) screens.set(client, (set = new Set()));
  return set;
};

/** How many screens watch each session, per client. */
const watchers = new WeakMap<HubClient, Map<string, number>>();
const holdsOf = (client: HubClient): Map<string, number> => {
  let map = watchers.get(client);
  if (!map) watchers.set(client, (map = new Map()));
  return map;
};

/** Live sessions whose transcripts the phone keeps, across every Mac, the ones seen last: each
 * holds up to 1,000 entries and 4 MiB, and nothing else bounds the total. */
export const MAX_VIEWS = 4;

/** The sessions streamed for a screen, of every Mac, the one seen last at the end. */
const visited = new WeakMap<OctetStore, string[]>();
const visitsOf = (store: OctetStore): string[] => {
  let list = visited.get(store);
  if (!list) visited.set(store, (list = []));
  return list;
};

/** Lets go of the transcripts of live sessions past the `MAX_VIEWS` seen last, of whichever Mac,
 * but never one on a screen. Each is forgotten by its Mac's client too (found through `links`),
 * so the next `watch` replays it from the start. */
function keepRecent(store: OctetStore, links: Links): void {
  const visits = visitsOf(store);
  const views = store.getState().bySession;
  const clientOf = (id: string): HubClient | undefined => {
    const mac = macOfKey(id);
    return (mac === null ? null : links.client(mac)) ?? undefined;
  };
  const onScreen = (id: string): boolean => {
    const client = clientOf(id);
    return client !== undefined && shown(client).has(id);
  };
  const holding = (id: string) => {
    const v = views.get(id);
    return v !== undefined && !v.ended && (v.transcript.entries.length > 0 || v.held !== null || v.approvals.length > 0);
  };
  // Newest first: a session not seen yet (a view a refusal made) counts as the oldest.
  const order = [...visits].reverse();
  const seen = new Set(visits);
  for (const id of views.keys()) if (!seen.has(id)) order.push(id);
  let kept = 0;
  for (const id of order) {
    // One on a screen is kept, and counts, whether or not its replay has brought anything yet.
    if (onScreen(id)) {
      kept += 1;
      continue;
    }
    if (!holding(id)) continue;
    if (kept < MAX_VIEWS) {
      kept += 1;
      continue;
    }
    if (store.getState().evict(id)) {
      const client = clientOf(id);
      const raw = rawOf(id);
      if (client && raw !== null) {
        requested(client).delete(id);
        client.forget(raw);
      }
    }
  }
  // Forget visits to sessions with no view and no screen, so the list stays as small as the store.
  visited.set(store, visits.filter((id) => views.has(id) || onScreen(id)));
}

/** Streams `session` from its Mac. One the store holds nothing of, and that was not asked for
 * yet, subscribes fresh, from the start of its journal: cursors are only in memory, so a cold
 * start has no transcript to resume. Asking again changes nothing. Seeing it makes it the newest
 * of the `MAX_VIEWS` whose transcripts are kept. Nothing for a key on no Mac connected. */
export function watch(store: OctetStore, links: Links, session: string): void {
  const link = linkOf(links, session);
  if (!link) return;
  const holds = holdsOf(link.client);
  holds.set(session, (holds.get(session) ?? 0) + 1);
  stream(store, links, link.client, session);
}

/** Streams `session` from `client` for the screens that hold it (`watch` counts them; this does
 * not). */
function stream(store: OctetStore, links: Links, client: HubClient, session: string): void {
  const raw = rawOf(session);
  if (raw === null) return;
  const fresh = !store.getState().bySession.has(session) && !requested(client).has(session);
  requested(client).add(session);
  shown(client).add(session);
  const visits = visitsOf(store).filter((id) => id !== session);
  visits.push(session);
  visited.set(store, visits);
  keepRecent(store, links);
  client.subscribe(raw, fresh);
}

/** Stops streaming `session`; the store keeps what it holds, and a later `watch` resumes. */
export function unwatch(links: Links, session: string): void {
  const link = linkOf(links, session);
  if (!link) return;
  const { client } = link;
  // Screens that stream one session (a Conversation, and the Usage over it) hold it together: it
  // stops when the last of them lets go, whichever order they blur in.
  const holds = holdsOf(client);
  const left = (holds.get(session) ?? 0) - 1;
  if (left > 0) {
    holds.set(session, left);
    return;
  }
  holds.delete(session);
  requested(client).delete(session);
  shown(client).delete(session);
  client.unsubscribe(link.raw);
}

const NO_APPROVALS: PendingApproval[] = [];
const NO_JOURNALS: readonly string[] = [];

export interface SessionStatus {
  /** The store holds a view of it. */
  known: boolean;
  ended: boolean;
  /** Why the hub refused to stream it, when it did for good. */
  failed: string | null;
  /** Moves on at each replay from the start. */
  epoch: number;
}

/** A view's transcript entries as `useEntries` gives them: during a replay from the start that
 * has brought nothing yet, the entries shown before it. */
const viewEntries = (v: SessionView | undefined): Entry[] => {
  const view = v ?? EMPTY_VIEW;
  return view.replaying && view.held ? view.held : view.transcript.entries;
};

/** A session's transcript entries now, as `useEntries` gives them, read once: for a value taken
 * when something opens, which a stream must not draw again. */
export const entriesNow = (store: OctetStore, id: string): Entry[] => viewEntries(store.getState().bySession.get(id));

/** The ready Macs of `hubs`, as one string (a Mac id holds no space). */
function readyKey(hubs: ReadonlyMap<string, HubState>): string {
  let key = "";
  for (const [mac, hub] of hubs) if (hub === "ready") key = key === "" ? mac : `${key} ${mac}`;
  return key;
}

/** Hooks over `store`: each selects the smallest part a screen needs. */
export function bindHooks(store: OctetStore) {
  return {
    /** Everything the phone knows of a session; the same object until it changes. */
    useSession: (id: string): SessionView => useStore(store, (s) => s.bySession.get(id) ?? EMPTY_VIEW),
    /** The listed sessions of every Mac; the same array until the list changes. */
    useSessions: (): ListedSession[] => useStore(store, useShallow((s) => s.sessions)),
    useSessionInfo: (id: string): ListedSession | undefined => useStore(store, (s) => s.sessions.find((x) => x.id === id)),
    /** What a session's header says of it from the list (workspace, engine, model, mode): new only
     * when one of those changes, not with its NOW line or phase. */
    useSessionHead: (id: string): SessionHead =>
      useStore(
        store,
        useShallow((s) => {
          const info = s.sessions.find((x) => x.id === id);
          return { workspace: info?.workspace, engine: info?.engine, model: info?.model ?? undefined, mode: info?.mode };
        }),
      ),
    /** A session's transcript entries: new only when an entry was added or changed. During a
     * replay from the start that has brought nothing yet, the entries shown before it. */
    useEntries: (id: string): Entry[] => useStore(store, (s) => viewEntries(s.bySession.get(id))),
    /** The terminal UI's state of a session (goal, NOW, queue); the same object until it changes. */
    useSessionState: (id: string): SessionState | null => useStore(store, (s) => s.bySession.get(id)?.state ?? null),
    /** Whether the phone holds a view of the session, and whether it ended or failed. */
    useSessionStatus: (id: string): SessionStatus =>
      useStore(
        store,
        useShallow((s) => {
          const v = s.bySession.get(id);
          return { known: v !== undefined, ended: v?.ended ?? false, failed: v?.failed ?? null, epoch: v?.epoch ?? 0 };
        }),
      ),
    /** A sessions frame came from `mac` (with none, from any Mac) since the store began or was
     * reset. */
    useListed: (mac?: string): boolean => useStore(store, (s) => (mac === undefined ? s.listed.size > 0 : s.listed.has(mac))),
    /** The Macs whose sessions frame came since the store began or was reset; the same set until
     * that changes. */
    useListedMacs: (): ReadonlySet<string> => useStore(store, (s) => s.listed),
    /** A session's draft (its first prompt, not taken by the Mac), or undefined. */
    useDraft: (id: string): string | undefined => useStore(store, (s) => s.drafts.get(id)),
    /** The texts the Mac took from this phone for a session, newest last; the same array until
     * one more is sent. */
    useSent: (id: string): readonly string[] => useStore(store, (s) => s.sent.get(id) ?? NO_SENT),
    /** The Inbox's Reply asked for this session's prompt to be focused. */
    useReplying: (id: string): boolean => useStore(store, (s) => s.replyTo === id),
    /** The Task strip's open state, shared by every session's screen. */
    useTaskOpen: (): boolean => useStore(store, (s) => s.taskOpen),
    /** The first approval waiting in a session, from the hub's sessions row, so a session that is
     * not open can still be answered; null when none waits. The same object until it changes. */
    useFrontApproval: (id: string): ApprovalInfo | null =>
      useStore(store, (s) => s.sessions.find((x) => x.id === id)?.approval ?? null),
    useApprovals: (id: string): PendingApproval[] =>
      useStore(store, (s) => s.bySession.get(id)?.approvals ?? NO_APPROVALS),
    /** The modes and models a session offers, with the current ones marked; null until the Mac
     * sends them. The same object until it changes. */
    useCatalog: (id: string): CatalogInfo | null => useStore(store, (s) => s.bySession.get(id)?.catalog ?? null),
    /** The catalogs of the live sessions that have sent one (of Mac `mac` only, when given); each
     * the same object until it changes. */
    useCatalogs: (mac?: string): CatalogInfo[] =>
      useStore(
        store,
        useShallow((s) => {
          const catalogs: CatalogInfo[] = [];
          for (const { id, mac: of } of s.sessions) {
            if (mac !== undefined && of !== mac) continue;
            const catalog = s.bySession.get(id)?.catalog;
            if (catalog) catalogs.push(catalog);
          }
          return catalogs;
        }),
      ),
    /** The link to `mac`; with none, the best link of every Mac's (`bestHub`). */
    useHub: (mac?: string): HubState => useStore(store, (s) => (mac === undefined ? bestHub(s) : (s.hubs.get(mac) ?? "offline"))),
    /** The Macs whose link is ready; the same set until one comes or goes. */
    useReadyMacs: (): ReadonlySet<string> => {
      const key = useStore(store, (s) => readyKey(s.hubs));
      return useMemo(() => new Set(key === "" ? [] : key.split(" ")), [key]);
    },
    /** How the session's last turn ended, for its mascot; null before one ends and while one runs. */
    useSessionTurn: (id: string): TurnEnd | null => useStore(store, (s) => s.bySession.get(id)?.turn ?? null),
    /** The turns a session has finished: the `finished` records counted by its view's reducer. */
    useTurns: (id: string): number => useStore(store, (s) => s.bySession.get(id)?.turns ?? 0),
    /** The latest usage text of a session: the journal's, else its row's; null when neither says. */
    useSessionUsage: (id: string): string | null =>
      useStore(store, (s) => s.bySession.get(id)?.usage || s.sessions.find((x) => x.id === id)?.usage || null),
    /** The journals the hub listed for a session, newest last; the same array until they change. */
    useJournals: (id: string): readonly string[] => useStore(store, (s) => s.bySession.get(id)?.journals ?? NO_JOURNALS),
    /** The listed sessions that report usage, in list order; each row the same object until it
     * changes. */
    useUsageRows: (): ListedSession[] => useStore(store, useShallow((s) => s.sessions.filter((x) => !!x.usage))),
  };
}

/** The app's store, and its hooks. */
export const octetStore = createOctetStore();
export const {
  useSession,
  useSessions,
  useSessionInfo,
  useSessionHead,
  useEntries,
  useSessionState,
  useSessionStatus,
  useListed,
  useListedMacs,
  useTaskOpen,
  useReplying,
  useDraft,
  useSent,
  useFrontApproval,
  useApprovals,
  useCatalog,
  useCatalogs,
  useHub,
  useReadyMacs,
  useSessionTurn,
  useTurns,
  useSessionUsage,
  useJournals,
  useUsageRows,
} = bindHooks(octetStore);
