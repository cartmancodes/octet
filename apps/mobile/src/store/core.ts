// The store itself: what it holds of every Mac, and how one Mac's frames change only its own part.
import { createStore, type StoreApi } from "zustand";

import type { BatchItem, HubState } from "../client/HubClient";
import type { Batch } from "../coalesce";
import { macOfKey } from "../macs";
import { EMPTY_VIEW, reduceView, type Op, type SessionView } from "../model/session";
import { clearParsed } from "../markdown/blocks";
import { hideToastOf } from "../toast";
import { field } from "../model/text";
import { isKnownEvent } from "../protocol";
import { FIELD, answeredAfter, isObj, merged, reconcile, type ListedSession } from "./rows";

export interface OctetState {
  /** The live sessions of every Mac, each Mac's as it listed them last, Mac after Mac. */
  sessions: ListedSession[];
  /** The same rows by Mac: a Mac's sessions frame replaces only its own. */
  slices: ReadonlyMap<string, ListedSession[]>;
  /** What the phone knows of each session it has heard of, by key. A Map: the keys hold the
   * hub's ids. */
  bySession: ReadonlyMap<string, SessionView>;
  /** The link to each Mac. `bestHub` gives the app's. The one place a link's state is kept:
   * the Fleet's `MacInfo` holds only a Mac's name and host. */
  hubs: ReadonlyMap<string, HubState>;
  /** The Macs whose link has ended an attempt (ready, refused, or failed) of its run, the run
   * since its client last started (the app came to the foreground, or it was paired): until then
   * a Mac that is not ready is coming, not asleep. */
  tried: ReadonlySet<string>;
  /** The Macs whose sessions frame came since the store began or was reset: until a Mac's comes,
   * its having no rows means "not told yet", not "none". */
  listed: ReadonlySet<string>;
  /** Applies one Mac's frames, their sessions already keyed by it, in order, in one commit. Each
   * sessions frame is the Mac's whole list, so only the last in the batch is reconciled. Returns the sessions that just ended (they were listed before and are not now): their views
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
  /** Mac `mac`'s link is `state`, and with `tried` given, has (or has not) ended an attempt of
   * its run. */
  setHub(mac: string, state: HubState, tried?: boolean): void;
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
/** Drafts kept, at most. */
const MAX_DRAFTS = 16;
/** Texts sent kept for ↑, at most, per session; and the sessions they are kept for. */
export const MAX_SENT = 20;
const MAX_SENT_SESSIONS = 16;
export const NO_SENT: readonly string[] = [];
const NO_ANSWERED: ReadonlySet<string> = new Set();
const NO_LISTED: ReadonlySet<string> = new Set();
const NO_TRIED: ReadonlySet<string> = new Set();
const NO_ROWS: ListedSession[] = [];

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
export const ofMac = (mac: string) => {
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
    tried: NO_TRIED,
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
          tried: NO_TRIED,
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
      const tried = st.tried.has(mac) ? new Set([...st.tried].filter((m) => m !== mac)) : st.tried;
      set({
        sessions: merged(slices),
        slices,
        bySession: without(st.bySession, mine),
        hubs,
        tried,
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
        const own = ownFrames(mac, frames);
        // Each list is the whole of the Mac's sessions, so only the batch's last one is reconciled;
        // one before it only lets go of the answered marks it would have.
        let lastList = -1;
        own.forEach((frame, i) => {
          if (frame.type === "sessions" && Array.isArray(frame.sessions)) lastList = i;
        });
        for (const [i, frame] of own.entries()) {
          switch (frame.type) {
            case "sessions":
              if (Array.isArray(frame.sessions)) {
                if (i === lastList) ({ sessions: rows, answered } = reconcile(mac, rows, frame.sessions, answered));
                else answered = answeredAfter(mac, frame.sessions, answered);
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

    setHub(mac, hub, attempted) {
      const st = get();
      const was = st.tried.has(mac);
      const tried =
        attempted === undefined || attempted === was
          ? st.tried
          : attempted
            ? new Set(st.tried).add(mac)
            : new Set([...st.tried].filter((m) => m !== mac));
      if (st.hubs.get(mac) === hub) {
        if (tried !== st.tried) set({ tried });
        return;
      }
      const hubs = new Map(st.hubs).set(mac, hub);
      // A link ready again brings a fresh list, which is the truth: an answer the Mac never heard
      // (the link went down first) must not hide its question.
      if (hub !== "ready") {
        set({ hubs, tried });
        return;
      }
      const mine = ofMac(mac);
      const answered = [...st.answered].some(mine) ? new Set([...st.answered].filter((id) => !mine(id))) : st.answered;
      set({ hubs, tried, answered: answered.size === 0 ? NO_ANSWERED : answered });
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

/** Whether session `key` is over, as far as the phone knows: its view is kept as ended, or its
 * Mac has listed its sessions and `key` is not one of them. False before the Mac has listed (not
 * told yet is not over) and for a string that is no key. */
export function hasEnded(state: Pick<OctetState, "bySession" | "listed" | "slices">, key: string): boolean {
  if (state.bySession.get(key)?.ended) return true;
  const mac = macOfKey(key);
  if (mac === null || !state.listed.has(mac)) return false;
  return !(state.slices.get(mac) ?? []).some((s) => s.id === key);
}
