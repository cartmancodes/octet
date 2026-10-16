// One store commit per 50 ms however many Macs stream: each Mac's client batches its own frames,
// and this sits between those batches and the store. A batch commits at once when no commit
// happened in the last 50 ms; otherwise it waits for the 50 ms mark, and every Mac that has one
// waiting goes into that one commit, in the order they first arrived. No React here.
import type { BatchItem } from "./client/HubClient";

/** The least time between two commits. */
export const COALESCE_MS = 50;

/** One Mac's frames, its sessions already keyed by it. */
export interface Batch {
  mac: string;
  frames: readonly BatchItem[];
}

/** What a Mac's feed does around the commit its frames go in: `before` sees the store as it was,
 * `after` the sessions of that Mac that just ended. */
export interface Sink {
  before?(frames: readonly BatchItem[]): void;
  after?(ended: string[]): void;
}

export interface Coalescer {
  /** Hands over a batch of `mac`'s from the feed `sink` stands for: a feed's batches wait
   * together, and apart from another feed's for the same Mac. */
  push(mac: string, frames: readonly BatchItem[], sink?: Sink): void;
  /** Forgets what `mac` has waiting from the feed `sink` (that feed stopped); with no `sink`,
   * from pushes that named none. A newer feed of the same Mac keeps its batches. */
  drop(mac: string, sink?: Sink): void;
}

export interface CoalescerOptions {
  now?: () => number;
  /** Runs `fn` after `ms`; returns the function that cancels it. */
  schedule?: (fn: () => void, ms: number) => () => void;
}

const defaultSchedule = (fn: () => void, ms: number): (() => void) => {
  const id = setTimeout(fn, ms);
  return () => clearTimeout(id);
};

interface Waiting {
  mac: string;
  frames: BatchItem[];
  sink: Sink | undefined;
}

/** Runs `fn`, reporting what it throws: one feed's fault stops no other's. */
function guarded(what: string, fn: () => void): void {
  try {
    fn();
  } catch (error) {
    console.error(`octet: ${what} failed`, error);
  }
}

/**
 * A coalescer over `commit`, which applies the batches in one store commit and returns, for
 * each, the sessions that ended, or null for a batch it could not apply. Every sink's `before`
 * runs before the commit, and the `after` of each batch that applied runs after it; a sink that
 * throws is reported and stops no other. A commit that throws is reported too, and runs no
 * `after`: the store applies in one `set`, so it is consistent either way, and no feed acts on
 * an outcome it was not told.
 */
export function createCoalescer(commit: (batches: Batch[]) => (string[] | null)[], options: CoalescerOptions = {}): Coalescer {
  const now = options.now ?? Date.now;
  const schedule = options.schedule ?? defaultSchedule;
  /** By feed (its sink), or by Mac for pushes that name none; in the order they first came. */
  const waiting = new Map<Sink | string, Waiting>();
  let last = -Infinity;
  let cancel: (() => void) | null = null;

  const run = () => {
    cancel?.();
    cancel = null;
    if (waiting.size === 0) return;
    const list = [...waiting.values()];
    waiting.clear();
    last = now();
    for (const w of list) guarded(`the feed of ${w.mac} before its commit`, () => w.sink?.before?.(w.frames));
    let ended: (string[] | null)[];
    try {
      ended = commit(list.map((w) => ({ mac: w.mac, frames: w.frames })));
    } catch (error) {
      console.error("octet: a store commit failed", error);
      return;
    }
    list.forEach((w, i) => {
      const gone = ended[i];
      if (gone != null) guarded(`the feed of ${w.mac} after its commit`, () => w.sink?.after?.(gone));
    });
  };

  return {
    push(mac, frames, sink) {
      const tag = sink ?? mac;
      const w = waiting.get(tag);
      if (w) for (const f of frames) w.frames.push(f);
      else waiting.set(tag, { mac, frames: frames.slice(), sink });
      if (cancel) return;
      const wait = last + COALESCE_MS - now();
      if (wait <= 0) run();
      else cancel = schedule(run, wait);
    },
    drop(mac, sink) {
      const tag = sink ?? mac;
      if (waiting.get(tag)?.mac === mac) waiting.delete(tag);
      if (waiting.size === 0 && cancel) {
        cancel();
        cancel = null;
      }
    },
  };
}
