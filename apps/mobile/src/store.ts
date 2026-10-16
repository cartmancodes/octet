// The app's state: the sessions each paired Mac's hub lists and, for each, what the phone knows of
// it. Every session is keyed `"<mac>/<session>"` (src/macs.ts): the hub's raw id is only on the
// wire. A zustand store without React in it; the hooks at the bottom select parts of it so a
// screen re-renders only for what it shows. The batches of every Mac's client go through one
// coalescer (src/coalesce.ts) and are applied in one `set`, and every part that a batch did not
// change keeps its identity.
//
// The parts are in src/store/: the rows (`rows.ts`), the store (`core.ts`), the feed from each
// client (`feed.ts`), the streamed views (`views.ts`) and the hooks (`hooks.ts`). This module names
// them all, as it always has.
export type { ListedSession } from "./store/rows";
export { MAX_SENT, bestHub, createOctetStore, hasEnded, type OctetState, type OctetStore } from "./store/core";
export { ENDED_KEPT_MS, connectStore, type ConnectOptions } from "./store/feed";
export { MAX_VIEWS, releaseSession, unwatch, watch } from "./store/views";
export {
  bindHooks,
  entriesNow,
  octetStore,
  useApprovals,
  useCatalog,
  useCatalogs,
  useConnecting,
  useDraft,
  useEntries,
  useFrontApproval,
  useHub,
  useJournals,
  useListed,
  useListedMacs,
  useReadyMacs,
  useReplying,
  useSent,
  useSession,
  useSessionHead,
  useSessionInfo,
  useSessionState,
  useSessionStatus,
  useSessionTurn,
  useSessionUsage,
  useSessions,
  useTaskOpen,
  useTried,
  useTurns,
  useUsageRows,
  type SessionStatus,
} from "./store/hooks";
