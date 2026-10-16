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
  /** Hands over a Mac's batch. */
  push(mac: string, frames: readonly BatchItem[], sink?: Sink): void;
  /** Forgets what `mac` has waiting (its feed stopped). */
  drop(mac: string): void;
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
  frames: BatchItem[];
  sink: Sink | undefined;
}

/** A coalescer over `commit`, which applies the batches in one store commit and returns, for
 * each, the sessions that ended. */
export function createCoalescer(commit: (batches: Batch[]) => string[][], options: CoalescerOptions = {}): Coalescer {
  const now = options.now ?? Date.now;
  const schedule = options.schedule ?? defaultSchedule;
  const waiting = new Map<string, Waiting>();
  let last = -Infinity;
  let cancel: (() => void) | null = null;

  const run = () => {
    cancel?.();
    cancel = null;
    if (waiting.size === 0) return;
    const list = [...waiting];
    waiting.clear();
    last = now();
    for (const [, w] of list) w.sink?.before?.(w.frames);
    let ended: string[][];
    try {
      ended = commit(list.map(([mac, w]) => ({ mac, frames: w.frames })));
    } catch {
      // A batch the store would not take (a key of another Mac): nothing of it was applied.
      return;
    }
    list.forEach(([, w], i) => w.sink?.after?.(ended[i] ?? []));
  };

  return {
    push(mac, frames, sink) {
      const w = waiting.get(mac);
      if (w) {
        for (const f of frames) w.frames.push(f);
        w.sink = sink ?? w.sink;
      } else waiting.set(mac, { frames: frames.slice(), sink });
      if (cancel) return;
      const wait = last + COALESCE_MS - now();
      if (wait <= 0) run();
      else cancel = schedule(run, wait);
    },
    drop(mac) {
      waiting.delete(mac);
      if (waiting.size === 0 && cancel) {
        cancel();
        cancel = null;
      }
    },
  };
}
