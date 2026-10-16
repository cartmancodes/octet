// The app's store, and the hooks over it: each selects the smallest part a screen needs.
import { useMemo } from "react";
import { useShallow } from "zustand/react/shallow";
import { useStore } from "zustand";

import type { HubState } from "../client/HubClient";
import { EMPTY_VIEW, type PendingApproval, type SessionState, type SessionView, type TurnEnd } from "../model/session";
import type { Entry } from "../model/transcript";
import type { ApprovalInfo, CatalogInfo } from "../protocol";
import type { SessionHead } from "../session";
import { NO_SENT, bestHub, createOctetStore, type OctetStore } from "./core";
import type { ListedSession } from "./rows";

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
    /** Whether Mac `mac`'s link has ended an attempt of its run (see `OctetState.tried`). */
    useTried: (mac: string): boolean => useStore(store, (s) => s.tried.has(mac)),
    /** Some Mac's link is connecting (or saying hello). */
    useConnecting: (): boolean =>
      useStore(store, (s) => [...s.hubs.values()].some((hub) => hub === "connecting" || hub === "authenticating")),
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
  useTried,
  useConnecting,
  useReadyMacs,
  useSessionTurn,
  useTurns,
  useSessionUsage,
  useJournals,
  useUsageRows,
} = bindHooks(octetStore);
