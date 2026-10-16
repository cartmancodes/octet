// One pairing's link: its client, what it feeds the store, and its push registration.
import { Refusal, type HubClient } from "../client/HubClient";
import type { Coalescer } from "../coalesce";
import { hostOf, type Links, type Pairing } from "../macs";
import { DENIED, currentToken, dropPushRegistration, onTokenChange } from "../notifications";
import { loadSettings } from "../persist";
import type { PushKinds } from "../protocol";
import { connectStore, type OctetStore } from "../store";
import type { TokenStore } from "./keychain";

/** One paired Mac, as screens show it. Its link's state is the store's (`useHub`, `useTried`):
 * kept in one place, so no screen can read two that disagree. */
export interface MacInfo {
  /** The phone's id for the pairing: every key of the Mac's sessions starts `<id>/`. */
  id: string;
  macName: string;
  /** The hub's host, as the pairing link gave it. */
  host: string;
}

/** The outcome of turning notifications on or off: done, or the words for why not. */
export type PushResult = { ok: true } | { ok: false; message: string };

export const UNREACHABLE = "Can't reach your Mac";
/** How long turning notifications on or off waits for a connection that is coming back. */
const READY_WAIT_MS = 8_000;
/** How long unpair leaves the connection open for the Mac to hear "notifications off". */
const GOODBYE_MS = 2_000;

/** What a link needs of the Fleet that owns it. */
export interface LinkHost {
  readonly store: OctetStore;
  readonly coalescer: Coalescer;
  /** Every Mac's client, as the Fleet has them at each call. */
  readonly links: Links;
  readonly schedule: (fn: () => void, ms: number) => () => void;
  /** Runs a push job after every one before it, of whichever Mac. */
  inTurn<T>(job: () => Promise<T>): Promise<T>;
  /** The link's push notice changed. */
  changed(link: MacLink): void;
  /** The Mac refused this phone's token as unpaired or revoked. */
  unpaired(link: MacLink): void;
}

/**
 * One pairing: its client, its token in the keychain, and its push registration (sent after each
 * ready, on a token rotation, and when notifications are turned on or off), with the notice of
 * the last one that did not work. What it feeds the store is keyed by its id.
 */
export class MacLink {
  /** Why this Mac did not take the push registration; null when the last one worked. */
  pushNotice: string | null = null;
  /** Turning notifications off did not reach this Mac: it is told on its next ready. */
  owesOff = false;
  /** Released: whatever answers come for it now are for a pairing that is gone. */
  gone = false;
  private shown: MacInfo;
  private detach: (() => void) | null = null;
  /** The `whenReady` calls waiting: each ends false at `release`. */
  private readonly waiters = new Set<(ready: boolean) => void>();

  constructor(
    private readonly host: LinkHost,
    public pairing: Pairing,
    readonly client: HubClient,
    readonly tokens: TokenStore,
  ) {
    this.shown = this.describe();
  }

  get id(): string {
    return this.pairing.id;
  }

  /** What screens show of it; the same object until it is renamed. */
  info(): MacInfo {
    return this.shown;
  }

  rename(macName: string): void {
    this.pairing = { ...this.pairing, macName };
    this.shown = this.describe();
  }

  private describe(): MacInfo {
    return { id: this.id, macName: this.pairing.macName, host: hostOf(this.pairing.hubUrl) ?? "" };
  }

  /** Feeds the store from the client, keyed by this Mac (its link's state included), and follows
   * its state for the push registration and a refused token, and the push token. */
  attach(): void {
    const { client } = this;
    const offStore = connectStore(this.host.store, client, { mac: this.id, links: this.host.links, coalescer: this.host.coalescer });
    const offState = client.onState((state) => {
      if (state === "unpaired") this.host.unpaired(this);
      else if (state === "ready") void this.syncPush();
    });
    // A rotated token while the link is down is sent by the next ready.
    const offToken = onTokenChange(() => {
      if (client.state === "ready") void this.syncPush();
    });
    this.detach = () => {
      offStore();
      offState();
      offToken();
    };
  }

  /** Stops feeding the store and following the client (the client is left as it is), and ends
   * every `whenReady` wait, false: a push waiting for this Mac gives up at once. */
  release(): void {
    this.gone = true;
    this.detach?.();
    this.detach = null;
    for (const done of [...this.waiters]) done(false);
  }

  /** Stops the client, but first, on a ready link, tells the Mac to forget this phone's Live
   * Activity tokens (`clear_activity`, so it neither starts nor updates an activity for a Mac the
   * phone no longer has) and "notifications off", and keeps the link until the Mac answers the
   * second or refuses it, or `GOODBYE_MS` pass. The hub answers in order, so an answer to the
   * second comes after the first was handled; the first's own answer, or refusal, changes nothing.
   * Nothing waits on it. If the Mac did not ack "notifications off" and `dropRegistration` (no
   * other Mac pushes to this phone), the phone drops its push registration instead, so that the
   * Mac's pushes bounce and it forgets the token. */
  goodbye(dropRegistration: boolean): void {
    const { client } = this;
    if (client.state !== "ready") {
      client.stop();
      if (dropRegistration) void dropPushRegistration();
      return;
    }
    let finished = false;
    let cancel = () => {};
    const finish = (heard: boolean) => {
      if (finished) return;
      finished = true;
      cancel();
      client.stop();
      if (!heard && dropRegistration) void dropPushRegistration();
    };
    cancel = this.host.schedule(() => finish(false), GOODBYE_MS);
    client.request({ type: "clear_activity" }).catch(() => undefined);
    client.request({ type: "register_push", token: null }).then(
      () => finish(true),
      () => finish(false),
    );
  }

  /** Resolves true when the client is ready, now or within `READY_WAIT_MS`; false when it is not. */
  whenReady(): Promise<boolean> {
    const { client } = this;
    if (client.state === "ready") return Promise.resolve(true);
    if (this.gone) return Promise.resolve(false);
    return new Promise<boolean>((resolve) => {
      let off = () => {};
      let cancel = () => {};
      const done = (ready: boolean) => {
        this.waiters.delete(done);
        off();
        cancel();
        resolve(ready);
      };
      this.waiters.add(done);
      off = client.onState((state) => {
        if (state === "ready") done(true);
        else if (state === "unpaired") done(false);
      });
      cancel = this.host.schedule(() => done(false), READY_WAIT_MS);
    });
  }

  /** Sends the Mac `register_push` (with `kinds`, when given and there is a token), once `ready`.
   * Saves nothing: the Fleet saves the setting once every Mac has had its turn. */
  async push(ready: boolean, token: string | null, kinds: PushKinds | null): Promise<PushResult> {
    if (!ready || this.gone) return { ok: false, message: UNREACHABLE };
    try {
      // "No token" says everything: kinds are saved, but not sent with it.
      await this.client.request(token !== null && kinds ? { type: "register_push", token, kinds } : { type: "register_push", token });
    } catch (error) {
      return { ok: false, message: wordsOf(error) };
    }
    // An ack that comes after the unpair is for a pairing that is gone.
    return this.gone ? { ok: false, message: UNREACHABLE } : { ok: true };
  }

  /** Registers the current push token with the Mac, if the user turned notifications on; with
   * them off, tells a Mac that missed the turn-off. Runs after each ready and on a token
   * rotation. A hub refusal shows on Settings; a link that dropped is no news, the next ready
   * tries again. With the permission taken away in the phone's settings, the Mac is told "no
   * token" (it would push to a phone that shows nothing) and the setting stays on, so a
   * permission given back registers again on the next ready. */
  private syncPush(): Promise<void> {
    const { client } = this;
    return this.host.inTurn(async () => {
      if (this.gone) return;
      if (!(await loadSettings()).notifications) {
        if (this.owesOff && client.state === "ready") await this.sendOff();
        return;
      }
      const got = await currentToken();
      // The user may have turned it off while the token was coming; the kinds are read again
      // too, since they may have changed.
      const settings = await loadSettings();
      if (!settings.notifications) return;
      if (this.gone || client.state !== "ready") return;
      if (!got.ok) {
        if (got.message === DENIED) {
          await client.request({ type: "register_push", token: null }).catch(() => undefined);
          if (this.gone) return;
        }
        this.notify(got.message);
        return;
      }
      try {
        // The phone's kinds are the truth: a Mac that still has the ones from before kinds
        // existed (all three on) is brought to them.
        await client.request({ type: "register_push", token: got.token, kinds: settings.pushKinds });
        if (!this.gone) this.notify(null);
      } catch (error) {
        if (this.gone) return;
        if (error instanceof Refusal && error.code !== "unsent" && error.code !== "lost") this.notify(error.message);
      }
    });
  }

  /** Tells the Mac "notifications off", which it missed when they were turned off. */
  private async sendOff(): Promise<void> {
    try {
      await this.client.request({ type: "register_push", token: null });
      if (this.gone) return;
      this.owesOff = false;
      this.notify(null);
    } catch (error) {
      if (!this.gone && error instanceof Refusal && error.code !== "unsent" && error.code !== "lost") this.notify(error.message);
    }
  }

  notify(pushNotice: string | null): void {
    if (this.pushNotice === pushNotice) return;
    this.pushNotice = pushNotice;
    this.host.changed(this);
  }
}

/** The words for a failed command: the hub's own, else a plain one. */
const wordsOf = (error: unknown): string => (error instanceof Refusal ? error.message : "Something went wrong. Try again.");
