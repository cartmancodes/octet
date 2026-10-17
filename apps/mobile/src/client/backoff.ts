/** Reconnect delays: 0.5 s doubling to 30 s, each cut by up to half at random so that phones
 * that lost the Mac together do not come back together. */
export const BACKOFF_START_MS = 500;
export const BACKOFF_MAX_MS = 30_000;

export class Backoff {
  private attempt = 0;

  /** `random` returns [0, 1); tests pass a fixed one. */
  constructor(private readonly random: () => number = Math.random) {}

  /** The next wait in milliseconds. */
  next(): number {
    const ceiling = Math.min(BACKOFF_MAX_MS, BACKOFF_START_MS * 2 ** this.attempt);
    this.attempt = Math.min(this.attempt + 1, 16);
    return Math.round(ceiling * (0.5 + 0.5 * this.random()));
  }

  reset(): void {
    this.attempt = 0;
  }
}
