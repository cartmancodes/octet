// The app's state: the sessions the hub lists and, for each, what the phone knows of it. A
// zustand store without React in it; the hooks at the bottom select parts of it so a screen
// re-renders only for what it shows. A client batch is applied in one `set`, and every part that
// a batch did not change keeps its identity.
import { useShallow } from "zustand/react/shallow";
import { createStore, useStore, type StoreApi } from "zustand";

import type { BatchItem, HubClient, HubState } from "./client/HubClient";
import { ENGINES, MODES, PHASES, count, member } from "./model/enums";
import { EMPTY_VIEW, reduceView, type Op, type PendingApproval, type SessionState, type SessionView } from "./model/session";
import type { Entry } from "./model/transcript";
import { field } from "./model/text";
import { isKnownEvent, type CatalogInfo, type SessionInfo } from "./protocol";

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
  /** Forgets everything the hub sent, for a phone unpaired from its Mac or paired with another. */
  reset(): void;
  /** The hub refused to stream `session` for good. */
  subscribeFailed(session: string, reason: string): void;
}

export type OctetStore = StoreApi<OctetState>;

/** Sessions listed, at most. */
const MAX_SESSIONS = 200;
const FIELD = 4_096;

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
  return info;
}

const sameSession = (a: SessionInfo, b: SessionInfo): boolean => {
  const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
  for (const k of keys) if ((a as unknown as Record<string, unknown>)[k] !== (b as unknown as Record<string, unknown>)[k]) return false;
  return true;
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

    reset() {
      set({ sessions: [], bySession: new Map(), hub: "offline", listed: false, taskOpen: false });
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
            break;
          case "event":
            if (isKnownEvent(frame.event)) push(frame.session, { op: "event", ev: frame.event });
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
      const ended: string[] = [];
      if (sessions !== before) {
        const now = new Set(sessions.map((s) => s.id));
        for (const s of before) {
          if (now.has(s.id)) continue;
          ended.push(s.id);
          if (bySession.has(s.id) || ops.has(s.id)) push(s.id, { op: "ended", ended: true });
        }
        for (const id of now) if (bySession.get(id)?.ended) push(id, { op: "ended", ended: false });
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

/** Streams `session`. One the store holds nothing of, and that was not asked for yet, subscribes
 * fresh, from the start of its journal: cursors are only in memory, so a cold start has no
 * transcript to resume. Asking again changes nothing. */
export function watch(store: OctetStore, client: HubClient, session: string): void {
  const fresh = !store.getState().bySession.has(session) && !requested(client).has(session);
  requested(client).add(session);
  client.subscribe(session, fresh);
}

/** Stops streaming `session`; the store keeps what it holds, and a later `watch` resumes. */
export function unwatch(client: HubClient, session: string): void {
  requested(client).delete(session);
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
    /** The Task strip's open state, shared by every session's screen. */
    useTaskOpen: (): boolean => useStore(store, (s) => s.taskOpen),
    useApprovals: (id: string): PendingApproval[] =>
      useStore(store, (s) => s.bySession.get(id)?.approvals ?? NO_APPROVALS),
    /** The modes and models a session offers, with the current ones marked; null until the Mac
     * sends them. The same object until it changes. */
    useCatalog: (id: string): CatalogInfo | null => useStore(store, (s) => s.bySession.get(id)?.catalog ?? null),
    useHub: (): HubState => useStore(store, (s) => s.hub),
  };
}

/** The app's store, and its hooks. */
export const octetStore = createOctetStore();
export const {
  useSession,
  useSessions,
  useSessionInfo,
  useEntries,
  useSessionState,
  useSessionStatus,
  useListed,
  useTaskOpen,
  useApprovals,
  useCatalog,
  useHub,
} = bindHooks(octetStore);
