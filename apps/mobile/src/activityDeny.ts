// The Island's Deny: each tap answers its approval no, once, on the Mac its host names. Moved out of
// src/activity.ts, which hands it the taps, the app's place (in front or not) and each Mac's link
// coming up. Plain TypeScript.
import type { DenyEvent } from "../modules/octet-live";
import { Refusal, type RequestBody } from "./client/HubClient";
import { hostKey, macOfKey, splitKey, type Links } from "./macs";
import { parseApprovalId } from "./navigation";
import { sendVia } from "./sendVia";
import { hasEnded, type OctetStore } from "./store";

/** Denies remembered, so the same one is answered once. */
const DENIES_SEEN = 64;
/** Denies kept for a Mac out of reach, at most, and for how long: the native side drops older. */
export const DENIES_HELD = 16;
const DENY_MAX_AGE_MS = 10 * 60_000;

/** What Deny needs of the Fleet (src/hub.ts). */
export interface DenyFleet {
  /** `status` is "loading" until the saved pairings are read. */
  snapshot(): { status: string };
  readonly links: Links;
  keyFor(host: string | null, raw: string): string | null;
  /** Sends `body` to Macs `macs` over background connections of their own; resolves to the Macs
   * that acked. */
  sendInBackground(macs: readonly string[], body: RequestBody): Promise<string[]>;
}

export interface DenyOptions {
  fleet: DenyFleet;
  store: OctetStore;
  now: () => number;
  /** Whether the app is in front (its links are up, or coming). */
  inFront: () => boolean;
  /** The send for a tap's request `id` settled: whatever came of it (sent, refused, lost, held for
   * the link, or nothing to send), the native side may let iOS suspend the app. Once per tap. */
  settled: (id: string) => void;
}

/** A Deny waiting for its Mac to be reached. */
interface HeldDeny {
  key: string;
  approval: number;
  at: number;
}

/**
 * Answers each Deny tapped on the Island once (by host, without case or a trailing dot, session and
 * approval), to the Mac `keyFor` resolves its host to; one that resolves to none sends nothing.
 * In front it goes over the Mac's link, or waits for it. In the background, when every link is
 * closed, it goes at once over a background connection to that Mac (hello `background: true`, the
 * answer, closed on the ack or after 10 s); if that gets no ack it waits for the link. At most 16
 * wait, for at most 10 minutes. Not sent for a session its Mac has listed as over, or as waiting on
 * nothing. Each tap is settled once (`settled`) whatever came of it, so the native side can let the
 * widget's Deny return and iOS suspend the app.
 */
export class IslandDenies {
  private readonly denied: string[] = [];
  private held: HeldDeny[] = [];
  /** Denies that came before the pairings were read (the native side hands over the taps made
   * while the app was closed as soon as it is listened to): resolved once they are. */
  private early: DenyEvent[] = [];

  constructor(private readonly o: DenyOptions) {}

  /** A tap, as the native side reports it. */
  deny(event: DenyEvent): void {
    if (this.o.fleet.snapshot().status === "loading") {
      const early = [...this.early, event];
      this.early = early.slice(-DENIES_HELD);
      for (const dropped of early.slice(0, -DENIES_HELD)) this.settle(dropped.id);
      return;
    }
    const done = this.once(event.id);
    const approval = parseApprovalId(event.approval);
    if (approval === null) return done();
    const mark = `${hostKey(event.mac)}\n${event.session}\n${approval}`;
    if (this.denied.includes(mark)) return done();
    this.denied.push(mark);
    if (this.denied.length > DENIES_SEEN) this.denied.shift();
    const key = this.o.fleet.keyFor(event.mac === "" ? null : event.mac, event.session);
    if (key === null) return done();
    this.send({ key, approval, at: this.o.now() }, done);
  }

  /** Settles request `id` (none for a tap the native side named no id for). */
  private settle(id: string | undefined): void {
    if (id !== undefined) this.o.settled(id);
  }

  /** A settle for request `id` that runs at most once, however many ways it ends. */
  private once(id: string | undefined): () => void {
    let done = false;
    return () => {
      if (done) return;
      done = true;
      this.settle(id);
    };
  }

  /** The Fleet changed: once the pairings are read, the taps that came before are resolved. */
  fleetChanged(): void {
    if (this.o.fleet.snapshot().status === "loading" || this.early.length === 0) return;
    const early = this.early;
    this.early = [];
    for (const event of early) this.deny(event);
  }

  /** Mac `mac`'s link is up: what waits for it goes. */
  ready(mac: string): void {
    const now = this.o.now();
    const mine = this.held.filter((h) => macOfKey(h.key) === mac);
    if (mine.length === 0) return;
    this.held = this.held.filter((h) => macOfKey(h.key) !== mac);
    // Each of these settled when it was first held.
    for (const held of mine) if (now - held.at <= DENY_MAX_AGE_MS) this.send(held, () => undefined);
  }

  /** Sends `held`, then calls `done` once it settled: answered, refused, lost, or held. */
  private send(held: HeldDeny, done: () => void): void {
    const mac = macOfKey(held.key);
    const raw = splitKey(held.key)?.raw;
    if (mac === null || raw === undefined) return done();
    const state = this.o.store.getState();
    if (state.listed.has(mac)) {
      if (hasEnded(state, held.key)) return done();
      const row = state.sessions.find((s) => s.id === held.key);
      if (row && row.approvals === 0) return done();
    }
    if (!this.o.inFront()) {
      const body: RequestBody = { type: "answer", session: raw, approval: held.approval, allow: false };
      void this.o.fleet
        .sendInBackground([mac], body)
        .then((acked) => {
          if (!acked.includes(mac)) this.hold(held);
        })
        .finally(done);
      return;
    }
    const client = this.o.fleet.links.client(mac);
    if (client?.state !== "ready") {
      this.hold(held);
      return done();
    }
    sendVia(this.o.fleet.links, held.key, { type: "answer", session: held.key, approval: held.approval, allow: false })
      .catch((error: unknown) => {
        if (error instanceof Refusal && error.code === "unsent") this.hold(held);
      })
      .finally(done);
  }

  private hold(held: HeldDeny): void {
    this.held = [...this.held.filter((h) => !(h.key === held.key && h.approval === held.approval)), held].slice(-DENIES_HELD);
  }
}
