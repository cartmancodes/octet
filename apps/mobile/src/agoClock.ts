// The clock the "ago" texts count from: one for the Inbox and the drawer, so that both read the
// same instant and the drawer reuses the Inbox's lists (`inboxOf`, src/inbox.ts) rather than
// sorting the sessions again. It moves on every 30 s while some screen reads it, and when one asks
// (the drawer, as it opens).
import { useSyncExternalStore } from "react";

/** How often the "ago" texts move on. */
export const AGO_TICK_MS = 30_000;

let now = Date.now();
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | null = null;

/** Reads the time afresh, and draws every reader again. */
export function tickAgo(): void {
  now = Date.now();
  for (const fn of [...listeners]) fn();
}

function subscribe(fn: () => void): () => void {
  if (listeners.size === 0) {
    // The first reader since none: the time it starts at, not the time the last one left.
    now = Date.now();
    timer = setInterval(tickAgo, AGO_TICK_MS);
  }
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0 && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
}

const read = (): number => now;
const still = (): (() => void) => () => {};

/** The clock in milliseconds. While `live` is false (a drawer that is shut) it is read but not
 * followed: nothing draws again as it ticks. */
export function useAgoClock(live: boolean = true): number {
  return useSyncExternalStore(live ? subscribe : still, read, read);
}
