// For tests that feed the store by hand: the one Mac's id and keys, and frames keyed as `connectStore`
// hands them to the store. Not imported by the app.
import type { BatchItem } from "./client/HubClient";
import { keyOf } from "./macs";
import type { OctetStore } from "./store";

/** The one Mac of the tests that pair one: the id its pairing is given (the rigs pass
 * `newId: newM`, so the move of a saved one-Mac pairing gives it this id too). */
export const M = "m00000001";

/** Makes `M`, for a Fleet's `newId`. */
export const newM = (): string => M;

/** The store's key of hub id `id` on the one Mac. */
export const k = (id: string): string => keyOf(M, id);

/** The frames with every session they name keyed by the one Mac, as `connectStore` does. */
export function keyed(frames: readonly BatchItem[]): BatchItem[] {
  return frames.map((f) => {
    if (f.type === "sessions" && Array.isArray(f.sessions)) {
      return { ...f, sessions: f.sessions.map((x) => (typeof x?.id === "string" && x.id !== "" ? { ...x, id: k(x.id) } : x)) };
    }
    return "session" in f && typeof f.session === "string" ? { ...f, session: k(f.session) } : f;
  });
}

/** The one Mac's frames, with hub ids, applied as its feed applies them. */
export const applyM = (store: OctetStore, frames: readonly BatchItem[]): string[] => store.getState().applyBatch(M, keyed(frames));

/** The Conversation's path for hub id `id` on the one Mac, the key encoded in one segment. */
export const sessionPath = (id: string): string => `/session/${encodeURIComponent(k(id))}`;
