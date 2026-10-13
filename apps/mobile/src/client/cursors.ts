import type { Cursor, ReplayFrom } from "../protocol";

/** Journals remembered per session; a session rarely has more than a handful. */
export const MAX_JOURNALS = 64;

interface Track {
  /** The last `seq` taken from each journal. `seq` only rises within a journal, so this is all
   * a duplicate check needs. */
  marks: Map<string, number>;
  /** Journals oldest first, as the hub lists them; one it did not list is newer than all. */
  order: string[];
  /** The newest journal's last `seq`: where a reconnect resumes. */
  cursor: Cursor | null;
}

/** Per session: what has been taken, and the cursor to resume from. Memory is bounded by the
 * journals of a session, never by its events. */
export class Cursors {
  private readonly tracks = new Map<string, Track>();

  constructor(saved: Record<string, Cursor> = {}) {
    for (const [session, cursor] of Object.entries(saved)) {
      this.tracks.set(session, {
        marks: new Map([[cursor.journal, cursor.seq]]),
        order: [cursor.journal],
        cursor: { journal: cursor.journal, seq: cursor.seq },
      });
    }
  }

  /** Takes `(journal, seq)`: true when it is new, false for a duplicate. */
  accept(session: string, journal: string, seq: number): boolean {
    const track = this.track(session);
    const mark = track.marks.get(journal);
    if (mark !== undefined && seq <= mark) return false;
    track.marks.set(journal, seq);
    let rank = track.order.indexOf(journal);
    if (rank < 0) {
      track.order.push(journal);
      rank = track.order.length - 1;
      while (track.order.length > MAX_JOURNALS) {
        const oldest = track.order.shift();
        if (oldest !== undefined) track.marks.delete(oldest);
        rank -= 1;
      }
    }
    const current = track.cursor ? track.order.indexOf(track.cursor.journal) : -1;
    // A late record of an older journal is taken once but does not move the cursor back.
    if (rank >= current) track.cursor = { journal, seq };
    return true;
  }

  /** A `subscribed` frame: from the start clears everything, from the cursor keeps what is
   * known and forgets journals the hub no longer lists. */
  apply(session: string, from: ReplayFrom, journals: readonly string[]): void {
    const track = this.track(session);
    if (from === "start") {
      track.marks.clear();
      track.cursor = null;
    } else {
      for (const journal of [...track.marks.keys()]) {
        if (!journals.includes(journal)) track.marks.delete(journal);
      }
    }
    track.order = journals.slice(-MAX_JOURNALS);
  }

  cursor(session: string): Cursor | null {
    const cursor = this.tracks.get(session)?.cursor;
    return cursor ? { journal: cursor.journal, seq: cursor.seq } : null;
  }

  /** How many journals are remembered for the session. */
  size(session: string): number {
    return this.tracks.get(session)?.marks.size ?? 0;
  }

  forget(session: string): void {
    this.tracks.delete(session);
  }

  snapshot(): Record<string, Cursor> {
    const out: Record<string, Cursor> = {};
    for (const session of this.tracks.keys()) {
      const cursor = this.cursor(session);
      if (cursor) out[session] = cursor;
    }
    return out;
  }

  private track(session: string): Track {
    let track = this.tracks.get(session);
    if (!track) {
      track = { marks: new Map(), order: [], cursor: null };
      this.tracks.set(session, track);
    }
    return track;
  }
}
