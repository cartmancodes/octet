// The sessions streamed for screens: what each client was asked for, by key, and the budget of
// transcripts kept across every Mac.
import type { HubClient } from "../client/HubClient";
import { macOfKey, rawOf, splitKey, type Links } from "../macs";
import type { OctetStore } from "./core";

/** The timers that will release ended sessions, per client, by key. */
const releases = new WeakMap<HubClient, Map<string, () => void>>();
export const timersOf = (client: HubClient): Map<string, () => void> => {
  let map = releases.get(client);
  if (!map) releases.set(client, (map = new Map()));
  return map;
};

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
export const requested = (client: HubClient): Set<string> => {
  let set = asked.get(client);
  if (!set) asked.set(client, (set = new Set()));
  return set;
};

/** The sessions each client streams for a screen (watched and not since unwatched). Unlike
 * `requested`, a refusal does not take a session out: its screen still shows it. */
const screens = new WeakMap<HubClient, Set<string>>();
export const shown = (client: HubClient): Set<string> => {
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
export function stream(store: OctetStore, links: Links, client: HubClient, session: string): void {
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
