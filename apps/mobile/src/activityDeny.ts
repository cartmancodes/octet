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
 * nothing.
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
      this.early = [...this.early, event].slice(-DENIES_HELD);
      return;
    }
    const approval = parseApprovalId(event.approval);
    if (approval === null) return;
    const mark = `${hostKey(event.mac)}\n${event.session}\n${approval}`;
    if (this.denied.includes(mark)) return;
    this.denied.push(mark);
    if (this.denied.length > DENIES_SEEN) this.denied.shift();
    const key = this.o.fleet.keyFor(event.mac === "" ? null : event.mac, event.session);
    if (key !== null) this.send({ key, approval, at: this.o.now() });
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
    for (const held of mine) if (now - held.at <= DENY_MAX_AGE_MS) this.send(held);
  }

  private send(held: HeldDeny): void {
    const mac = macOfKey(held.key);
    const raw = splitKey(held.key)?.raw;
    if (mac === null || raw === undefined) return;
    const state = this.o.store.getState();
    if (state.listed.has(mac)) {
      if (hasEnded(state, held.key)) return;
      const row = state.sessions.find((s) => s.id === held.key);
      if (row && row.approvals === 0) return;
    }
    if (!this.o.inFront()) {
      const body: RequestBody = { type: "answer", session: raw, approval: held.approval, allow: false };
      void this.o.fleet.sendInBackground([mac], body).then((acked) => {
        if (!acked.includes(mac)) this.hold(held);
      });
      return;
    }
    const client = this.o.fleet.links.client(mac);
    if (client?.state !== "ready") {
      this.hold(held);
      return;
    }
    sendVia(this.o.fleet.links, held.key, { type: "answer", session: held.key, approval: held.approval, allow: false }).catch((error: unknown) => {
      if (error instanceof Refusal && error.code === "unsent") this.hold(held);
    });
  }

  private hold(held: HeldDeny): void {
    this.held = [...this.held.filter((h) => !(h.key === held.key && h.approval === held.approval)), held].slice(-DENIES_HELD);
  }
}
