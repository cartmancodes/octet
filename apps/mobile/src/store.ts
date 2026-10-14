// The app's state: the sessions the hub lists and, for each, what the phone knows of it. A
// zustand store without React in it; the hooks at the bottom select parts of it so a screen
// re-renders only for what it shows. A client batch is applied in one `set`, and every part that
// a batch did not change keeps its identity.
import { useShallow } from "zustand/react/shallow";
import { createStore, useStore, type StoreApi } from "zustand";

import type { BatchItem, HubClient, HubState } from "./client/HubClient";
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
import type { Entry } from "./model/transcript";
import { FIELD_CHARS, field } from "./model/text";
import { isKnownEvent, type ApprovalInfo, type CatalogInfo, type SessionInfo } from "./protocol";
import type { SessionHead } from "./session";

export interface OctetState {
  /** The live sessions, as the hub listed them last. */
  sessions: SessionInfo[];
  /** What the phone knows of each session it has heard of. A Map: the keys are the hub's. */
  bySession: ReadonlyMap<string, SessionView>;
  hub: HubState;
  /** A sessions frame came since the store began or was reset: until then an empty `sessions`
   * means "not told yet", not "none". */
  listed: boolean;
  /** Applies the client's frames, in order, in one commit. Returns the sessions that just
   * ended (they were listed before and are not now): their views stay, marked `ended`, until
   * the caller releases them. */
  applyBatch(frames: readonly BatchItem[]): string[];
  /** Drops the view of an ended session, or of a failed one the hub does not list. False for
   * any other. */
  release(session: string): boolean;
  /** The Task strip is open: one choice for every session's screen, for the app run. */
  taskOpen: boolean;
  setTaskOpen(open: boolean): void;
  setHub(state: HubState): void;
  /** The first prompt of a session that the Mac did not take, keyed by session: the user's words,
   * kept for that session's prompt box. At most `MAX_DRAFTS`, the oldest dropped. */
  drafts: ReadonlyMap<string, string>;
  setDraft(session: string, text: string): void;
  /** The session's draft, which is then forgotten; "" when it has none. */
  takeDraft(session: string): string;
  /** Forgets everything the hub sent, for a phone unpaired from its Mac or paired with another. */
  reset(): void;
  /** The hub refused to stream `session` for good. */
  subscribeFailed(session: string, reason: string): void;
  /** Lets go of a live session's transcript and approvals, keeping what other screens read of it
   * (its catalog and last state). False for a session with no view, or one that ended. */
  evict(session: string): boolean;
}

export type OctetStore = StoreApi<OctetState>;

/** Sessions listed, at most. */
const MAX_SESSIONS = 200;
/** Drafts kept, at most. */
const MAX_DRAFTS = 16;
const FIELD = FIELD_CHARS;

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
  return info;
}

/** Two cleaned approvals say the same (their summaries are small and bounded). */
const sameApproval = (a: ApprovalInfo | null | undefined, b: ApprovalInfo | null | undefined): boolean =>
  a === b || (!!a && !!b && a.id === b.id && JSON.stringify(a.summary) === JSON.stringify(b.summary));

const sameSession = (a: SessionInfo, b: SessionInfo): boolean => {
  const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
  for (const k of keys) {
    if (k === "approval") continue;
    if ((a as unknown as Record<string, unknown>)[k] !== (b as unknown as Record<string, unknown>)[k]) return false;
  }
  return sameApproval(a.approval, b.approval);
};

/** The listed sessions, reusing the previous object of each session that did not change, and the
 * previous array when none did. */
function reconcile(prev: SessionInfo[], raw: unknown[]): SessionInfo[] {
  const before = new Map(prev.map((s) => [s.id, s]));
  const next: SessionInfo[] = [];
  const seen = new Set<string>();
  for (const item of raw.slice(0, MAX_SESSIONS)) {
    const info = cleanSession(item);
    if (!info || seen.has(info.id)) continue;
    seen.add(info.id);
    const old = before.get(info.id);
    next.push(old && sameSession(old, info) ? old : info);
  }
  return next.length === prev.length && next.every((s, i) => s === prev[i]) ? prev : next;
}

export function createOctetStore(): OctetStore {
  return createStore<OctetState>()((set, get) => ({
    sessions: [],
    bySession: new Map(),
    hub: "offline",
    listed: false,
    taskOpen: false,
    drafts: new Map(),

    reset() {
      clearParsed(); // what the old Mac's replies parsed to
      set({ sessions: [], bySession: new Map(), hub: "offline", listed: false, taskOpen: false, drafts: new Map() });
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

    setTaskOpen(taskOpen) {
      if (get().taskOpen !== taskOpen) set({ taskOpen });
    },

    applyBatch(frames) {
      const { sessions: before, bySession, listed: wasListed } = get();
      let sessions = before;
      let listed = wasListed;
      const ops = new Map<string, Op[]>();
      const push = (session: string, op: Op) => {
        const list = ops.get(session);
        if (list) list.push(op);
        else ops.set(session, [op]);
      };
      for (const frame of frames) {
        switch (frame.type) {
          case "sessions":
            if (Array.isArray(frame.sessions)) {
              sessions = reconcile(sessions, frame.sessions);
              listed = true;
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

      // A listed session is live; one that was listed and is not now has ended, and keeps its view.
      // One refused for good that comes into the list is a session again (it started since a deep
      // link asked for it, or started over under its id): its failure is over.
      const ended: string[] = [];
      if (sessions !== before) {
        const now = new Set(sessions.map((s) => s.id));
        const was = new Set(before.map((s) => s.id));
        for (const s of before) {
          if (now.has(s.id)) continue;
          ended.push(s.id);
          if (bySession.has(s.id) || ops.has(s.id)) push(s.id, { op: "ended", ended: true });
        }
        for (const id of now) {
          const old = bySession.get(id);
          if (old?.ended) push(id, { op: "ended", ended: false });
          if (old?.failed != null && !was.has(id)) push(id, { op: "failed", reason: null });
        }
      }
      let views: Map<string, SessionView> | null = null;
      for (const [session, list] of ops) {
        const old = bySession.get(session);
        const next = reduceView(old ?? EMPTY_VIEW, list);
        if (next === old || (old === undefined && next === EMPTY_VIEW)) continue;
        (views ??= new Map(bySession)).set(session, next);
      }
      if (views === null && sessions === before && listed === wasListed) return ended;
      set({ sessions, ...(views ? { bySession: views } : {}), ...(listed !== wasListed ? { listed } : {}) });
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

    setHub(hub) {
      if (get().hub !== hub) set({ hub });
    },

    evict(session) {
      const v = get().bySession.get(session);
      if (!v || v.ended) return false;
      const kept: SessionView = { ...EMPTY_VIEW, state: v.state, catalog: v.catalog, mode: v.mode, model: v.model, usage: v.usage, epoch: v.epoch + 1 };
      set((st) => ({ bySession: new Map(st.bySession).set(session, kept) }));
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
  /** Runs `fn` after `ms`; returns the function that cancels it. */
  schedule?: (fn: () => void, ms: number) => () => void;
}

const defaultSchedule = (fn: () => void, ms: number): (() => void) => {
  const id = setTimeout(fn, ms);
  return () => clearTimeout(id);
};

/** The timers that will release ended sessions, per client. */
const releases = new WeakMap<HubClient, Map<string, () => void>>();
const timersOf = (client: HubClient): Map<string, () => void> => {
  let map = releases.get(client);
  if (!map) releases.set(client, (map = new Map()));
  return map;
};

/** Feeds the store from `client`: its batches, its connection state, and the sessions the hub
 * refused for good (forgotten, and their views marked failed). A session that leaves the hub's
 * list keeps its view for `ENDED_KEPT_MS`, then is released and forgotten (one with no view is
 * forgotten at once). Returns the function
 * that stops all of it. */
export function connectStore(store: OctetStore, client: HubClient, options: ConnectOptions = {}): () => void {
  const schedule = options.schedule ?? defaultSchedule;
  const timers = timersOf(client);
  const cancel = (session: string) => {
    timers.get(session)?.();
    timers.delete(session);
  };
  const forget = (session: string) => {
    cancel(session);
    requested(client).delete(session);
    client.forget(session);
  };
  const offBatch = client.onBatch((frames) => {
    const state = store.getState();
    // Only a list can end a failure: look for failed views only when one came.
    const failed = frames.some((f) => f.type === "sessions")
      ? [...state.bySession].flatMap(([id, v]) => (v.failed !== null ? [id] : []))
      : [];
    for (const ended of state.applyBatch(frames)) {
      cancel(ended);
      // Nothing to hold for a screen: the client lets it go now rather than never.
      if (!store.getState().bySession.has(ended)) {
        forget(ended);
        continue;
      }
      timers.set(
        ended,
        schedule(() => {
          timers.delete(ended);
          if (store.getState().release(ended)) forget(ended);
        }, ENDED_KEPT_MS),
      );
    }
    // A session listed again is live: its timer goes.
    for (const [session] of timers) if (store.getState().bySession.get(session)?.ended === false) cancel(session);
    // A failure the list ended: the screen showing the session streams it again.
    for (const session of failed) {
      if (shown(client).has(session) && store.getState().bySession.get(session)?.failed === null) {
        watch(store, client, session);
      }
    }
  });
  const offState = client.onState((state) => store.getState().setHub(state));
  const offFailed = client.onSubscribeFailed((session, refusal) => {
    forget(session);
    store.getState().subscribeFailed(session, refusal.message);
  });
  store.getState().setHub(client.state);
  return () => {
    offBatch();
    offState();
    offFailed();
    for (const session of [...timers.keys()]) cancel(session);
  };
}

/** The user left the screen of an ended session: its view is dropped and the client forgets it. */
export function releaseSession(store: OctetStore, client: HubClient, session: string): void {
  if (!store.getState().release(session)) return;
  timersOf(client).get(session)?.();
  timersOf(client).delete(session);
  requested(client).delete(session);
  client.forget(session);
}

/** The sessions each client was asked to stream and not since released. */
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

/** Live sessions whose transcripts the phone keeps, the ones seen last: each holds up to 1,000
 * entries and 4 MiB, and nothing else bounds the total. */
export const MAX_VIEWS = 4;

/** The sessions each client streamed for a screen, the one seen last at the end. */
const visited = new WeakMap<HubClient, string[]>();
const visitsOf = (client: HubClient): string[] => {
  let list = visited.get(client);
  if (!list) visited.set(client, (list = []));
  return list;
};

/** Lets go of the transcripts of live sessions past the `MAX_VIEWS` seen last, but never one on
 * a screen. Each is forgotten by the client too, so the next `watch` replays it from the start. */
function keepRecent(store: OctetStore, client: HubClient): void {
  const visits = visitsOf(client);
  const views = store.getState().bySession;
  const holding = (id: string) => {
    const v = views.get(id);
    return v !== undefined && !v.ended && (v.transcript.entries.length > 0 || v.held !== null || v.approvals.length > 0);
  };
  // Newest first: a session not seen yet (a view a refusal made) counts as the oldest.
  const order = [...visits].reverse();
  for (const id of views.keys()) if (!visits.includes(id)) order.push(id);
  let kept = 0;
  for (const id of order) {
    // One on a screen is kept, and counts, whether or not its replay has brought anything yet.
    if (shown(client).has(id)) {
      kept += 1;
      continue;
    }
    if (!holding(id)) continue;
    if (kept < MAX_VIEWS) {
      kept += 1;
      continue;
    }
    if (store.getState().evict(id)) {
      requested(client).delete(id);
      client.forget(id);
    }
  }
  // Forget visits to sessions with no view and no screen, so the list stays as small as the store.
  visited.set(client, visits.filter((id) => views.has(id) || shown(client).has(id)));
}

/** Streams `session`. One the store holds nothing of, and that was not asked for yet, subscribes
 * fresh, from the start of its journal: cursors are only in memory, so a cold start has no
 * transcript to resume. Asking again changes nothing. Seeing it makes it the newest of the
 * `MAX_VIEWS` whose transcripts are kept. */
export function watch(store: OctetStore, client: HubClient, session: string): void {
  const fresh = !store.getState().bySession.has(session) && !requested(client).has(session);
  requested(client).add(session);
  shown(client).add(session);
  const visits = visitsOf(client).filter((id) => id !== session);
  visits.push(session);
  visited.set(client, visits);
  keepRecent(store, client);
  client.subscribe(session, fresh);
}

/** Stops streaming `session`; the store keeps what it holds, and a later `watch` resumes. */
export function unwatch(client: HubClient, session: string): void {
  requested(client).delete(session);
  shown(client).delete(session);
  client.unsubscribe(session);
}

const NO_APPROVALS: PendingApproval[] = [];

export interface SessionStatus {
  /** The store holds a view of it. */
  known: boolean;
  ended: boolean;
  /** Why the hub refused to stream it, when it did for good. */
  failed: string | null;
  /** Moves on at each replay from the start. */
  epoch: number;
}

/** Hooks over `store`: each selects the smallest part a screen needs. */
export function bindHooks(store: OctetStore) {
  return {
    /** Everything the phone knows of a session; the same object until it changes. */
    useSession: (id: string): SessionView => useStore(store, (s) => s.bySession.get(id) ?? EMPTY_VIEW),
    /** The listed sessions; the same array until the list changes. */
    useSessions: (): SessionInfo[] => useStore(store, useShallow((s) => s.sessions)),
    useSessionInfo: (id: string): SessionInfo | undefined => useStore(store, (s) => s.sessions.find((x) => x.id === id)),
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
    useEntries: (id: string): Entry[] =>
      useStore(store, (s) => {
        const v = s.bySession.get(id) ?? EMPTY_VIEW;
        return v.replaying && v.held ? v.held : v.transcript.entries;
      }),
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
    /** A sessions frame came since the store began or was reset. */
    useListed: (): boolean => useStore(store, (s) => s.listed),
    /** A session's draft (its first prompt, not taken by the Mac), or undefined. */
    useDraft: (id: string): string | undefined => useStore(store, (s) => s.drafts.get(id)),
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
    /** The catalogs of the live sessions that have sent one; each the same object until it changes. */
    useCatalogs: (): CatalogInfo[] =>
      useStore(
        store,
        useShallow((s) => {
          const catalogs: CatalogInfo[] = [];
          for (const { id } of s.sessions) {
            const catalog = s.bySession.get(id)?.catalog;
            if (catalog) catalogs.push(catalog);
          }
          return catalogs;
        }),
      ),
    useHub: (): HubState => useStore(store, (s) => s.hub),
    /** How the session's last turn ended, for its mascot; null before one ends and while one runs. */
    useSessionTurn: (id: string): TurnEnd | null => useStore(store, (s) => s.bySession.get(id)?.turn ?? null),
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
  useTaskOpen,
  useDraft,
  useFrontApproval,
  useApprovals,
  useCatalog,
  useCatalogs,
  useHub,
  useSessionTurn,
} = bindHooks(octetStore);
