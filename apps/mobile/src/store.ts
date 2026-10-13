// The app's state: the sessions the hub lists and, for each, what the phone knows of it. A
// zustand store without React in it; the hooks at the bottom select parts of it so a screen
// re-renders only for what it shows. A client batch is applied in one `set`, and every part that
// a batch did not change keeps its identity.
import { useShallow } from "zustand/react/shallow";
import { createStore, useStore, type StoreApi } from "zustand";

import type { HubClient, HubState } from "./client/HubClient";
import { EMPTY_VIEW, reduceView, type Op, type PendingApproval, type SessionView } from "./model/session";
import { field } from "./model/text";
import { isKnownEvent, type SessionInfo, type ToPhone } from "./protocol";

export interface OctetState {
  /** The live sessions, as the hub listed them last. */
  sessions: SessionInfo[];
  /** What the phone knows of each session it has heard of. */
  bySession: Record<string, SessionView>;
  hub: HubState;
  /** Applies the client's frames, in order, in one commit. Returns the sessions that ended (they
   * were listed before and are not now): their views are gone, and the caller forgets them. */
  applyBatch(frames: readonly ToPhone[]): string[];
  setHub(state: HubState): void;
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
  const info: SessionInfo = {
    id: field(raw.id, FIELD),
    engine: raw.engine as SessionInfo["engine"],
    workspace: field(raw.workspace, FIELD),
    mode: raw.mode as SessionInfo["mode"],
    phase: raw.phase as SessionInfo["phase"],
    approvals: typeof raw.approvals === "number" ? raw.approvals : 0,
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
    bySession: {},
    hub: "offline",

    applyBatch(frames) {
      const { sessions: before, bySession } = get();
      let sessions = before;
      const ops = new Map<string, Op[]>();
      const push = (session: string, op: Op) => {
        const list = ops.get(session);
        if (list) list.push(op);
        else ops.set(session, [op]);
      };
      for (const frame of frames) {
        switch (frame.type) {
          case "sessions":
            if (Array.isArray(frame.sessions)) sessions = reconcile(sessions, frame.sessions);
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
          default:
            break; // replies to requests are the client's
        }
      }

      const ended: string[] = [];
      if (sessions !== before) {
        const now = new Set(sessions.map((s) => s.id));
        for (const s of before) if (!now.has(s.id)) ended.push(s.id);
      }
      let views: Record<string, SessionView> | null = null;
      for (const [session, list] of ops) {
        if (ended.includes(session)) continue;
        const old = bySession[session];
        const next = reduceView(old ?? EMPTY_VIEW, list);
        if (next !== old && !(old === undefined && next === EMPTY_VIEW)) (views ??= { ...bySession })[session] = next;
      }
      for (const session of ended) {
        if (session in bySession) {
          views ??= { ...bySession };
          delete views[session];
        }
      }
      if (views === null && sessions === before) return ended;
      set({ sessions, ...(views ? { bySession: views } : {}) });
      return ended;
    },

    setHub(hub) {
      if (get().hub !== hub) set({ hub });
    },

    subscribeFailed(session, reason) {
      const old = get().bySession[session] ?? EMPTY_VIEW;
      const next = reduceView(old, [{ op: "failed", reason: field(reason, FIELD) }]);
      if (next !== old) set((s) => ({ bySession: { ...s.bySession, [session]: next } }));
    },
  }));
}

/** Feeds the store from `client`: its batches, its connection state, and the sessions the hub
 * refused for good (forgotten, and their views marked failed). Returns the function that stops. */
export function connectStore(store: OctetStore, client: HubClient): () => void {
  const forget = (session: string) => {
    requested(client).delete(session);
    client.forget(session);
  };
  const offBatch = client.onBatch((frames) => {
    for (const ended of store.getState().applyBatch(frames)) forget(ended);
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
  };
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
  const fresh = !(session in store.getState().bySession) && !requested(client).has(session);
  requested(client).add(session);
  client.subscribe(session, fresh);
}

/** Stops streaming `session`; the store keeps what it holds, and a later `watch` resumes. */
export function unwatch(client: HubClient, session: string): void {
  requested(client).delete(session);
  client.unsubscribe(session);
}

const NO_APPROVALS: PendingApproval[] = [];

/** Hooks over `store`: each selects the smallest part a screen needs. */
export function bindHooks(store: OctetStore) {
  return {
    /** Everything the phone knows of a session; the same object until it changes. */
    useSession: (id: string): SessionView => useStore(store, (s) => s.bySession[id] ?? EMPTY_VIEW),
    /** The listed sessions; the same array until the list changes. */
    useSessions: (): SessionInfo[] => useStore(store, useShallow((s) => s.sessions)),
    useSessionInfo: (id: string): SessionInfo | undefined => useStore(store, (s) => s.sessions.find((x) => x.id === id)),
    /** A session's transcript entries: new only when an entry was added or changed. */
    useEntries: (id: string) => useStore(store, (s) => (s.bySession[id] ?? EMPTY_VIEW).transcript.entries),
    useApprovals: (id: string): PendingApproval[] =>
      useStore(store, (s) => s.bySession[id]?.approvals ?? NO_APPROVALS),
    useHub: (): HubState => useStore(store, (s) => s.hub),
  };
}

/** The app's store, and its hooks. */
export const octetStore = createOctetStore();
export const { useSession, useSessions, useSessionInfo, useEntries, useApprovals, useHub } = bindHooks(octetStore);
