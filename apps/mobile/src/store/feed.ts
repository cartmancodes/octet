// Feeding the store from one Mac's client: its frames keyed by that Mac, its link, and the
// release of the sessions that end.
import type { BatchItem, HubClient, HubState } from "../client/HubClient";
import { createCoalescer, type Coalescer, type Sink } from "../coalesce";
import { keyOf, rawOf, type Links } from "../macs";
import type { SessionInfo } from "../protocol";
import { ofMac, type OctetStore } from "./core";
import { isObj } from "./rows";
import { requested, shown, stream, timersOf } from "./views";

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
  // Whether an attempt of the client's run has ended: each run (a `start()`) begins untried; a
  // stop (the app going to the background) is not the Mac failing to answer, and ends the run.
  let run = client.run;
  let attempting = false;
  let tried = false;
  const follow = (state: HubState) => {
    if (client.run !== run) {
      run = client.run;
      attempting = false;
      tried = false;
    }
    if (state === "connecting" || state === "authenticating") attempting = true;
    else if (run !== null && (attempting || state === "ready")) tried = true;
    else if (state === "unpaired") tried = true;
    store.getState().setHub(mac, state, tried);
  };
  const offState = client.onState(follow);
  const offFailed = client.onSubscribeFailed((raw, refusal) => {
    const session = keyOf(mac, raw);
    forget(session);
    store.getState().subscribeFailed(session, refusal.message);
  });
  // As the feed begins, nothing of the run is heard yet.
  store.getState().setHub(mac, client.state, false);
  return () => {
    offBatch();
    offState();
    offFailed();
    coalescer.drop(mac, sink); // a newer feed of the same Mac keeps what it has waiting
    for (const session of [...timers.keys()]) cancel(session);
  };
}
