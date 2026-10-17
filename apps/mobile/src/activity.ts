// The Live Activity while the app runs (the Dynamic Island and the Lock Screen, iOS 17.2 and later):
// the app shows what every Mac's sessions are doing, gives one Mac (the one that drives the Island
// while the phone is away) the ActivityKit tokens so its hub can start and update the activity,
// keeps the widget's App Group entries (names and commands, which never leave the phone), and
// hands the Island's Deny to src/activityDeny.ts. The Island's Allow is a link
// (src/navigation.ts), opened by the shell behind Face ID and the arm. Where Live Activities are not
// supported (Android, Expo Go, jest) it does nothing at all.
import { createContext, useContext } from "react";

import * as octetLive from "../modules/octet-live";
import type { ActivityState, DenyEvent, Subscription, TokenEvent, WidgetEntry } from "../modules/octet-live";
import { IslandDenies, type DenyFleet } from "./activityDeny";
import { ActivityModel, IDLE_END_MS, activityRegistration, rowsOf, widgetEntries, type ActivityTokens } from "./activityModel";
import { Refusal, type HubClient, type RequestBody } from "./client/HubClient";
import { MAC_ID, macOfKey, splitKey } from "./macs";
import { loadSettings, saveSettings } from "./persist";
import { sendTo } from "./sendVia";
import type { OctetState, OctetStore } from "./store";

/** The least time between two changes the app makes to the activity. */
export const DRIVE_GAP_MS = 1_000;
/** How long a register or clear the hub refused for now (`RETRIED`) waits before it is tried once more. */
export const REGISTER_RETRY_MS = 5_000;
/** The refusals that may pass: the hub was busy, rate-limited, or could not save. Any other is a
 * definite no, tried again only on the next change or connection. */
const RETRIED: ReadonlySet<string> = new Set(["rate_limited", "busy", "unavailable", "timeout"]);
/** What a Mac is told when it is not the one that drives the Island (or the switch is off). */
const CLEAR: RequestBody = { type: "clear_activity" };

/** The native module, as the controller uses it (modules/octet-live); a test gives a fake. */
export interface LiveApi {
  isSupported(): boolean;
  start(state: ActivityState): Promise<void>;
  update(state: ActivityState): Promise<void>;
  end(): Promise<void>;
  setEntries(entries: Record<string, WidgetEntry>): Promise<void>;
  addPushTokenListener(listener: (event: TokenEvent) => void): Subscription;
  addPushToStartTokenListener(listener: (event: TokenEvent) => void): Subscription;
  addDenyListener(listener: (event: DenyEvent) => void): Subscription;
}

/** The app's native module: a no-op wherever there is none. */
export const nativeLive: LiveApi = {
  isSupported: octetLive.isSupported,
  start: octetLive.start,
  update: octetLive.update,
  end: octetLive.end,
  setEntries: octetLive.setEntries,
  addPushTokenListener: octetLive.addPushTokenListener,
  addPushToStartTokenListener: octetLive.addPushToStartTokenListener,
  addDenyListener: octetLive.addDenyListener,
};

/** What the controller needs of the Fleet (src/hub.ts). */
export interface ActivityFleet extends DenyFleet {
  /** `status` is "loading" until the saved pairings are read; `macs` are the pairings, in order. */
  snapshot(): { status: string; macs: readonly { id: string; host: string }[] };
  subscribe(fn: () => void): () => void;
}

export interface LiveActivityOptions {
  api?: LiveApi;
  fleet: ActivityFleet;
  store: OctetStore;
  /** Clock and timer overrides (tests). */
  now?: () => number;
  schedule?: (fn: () => void, ms: number) => () => void;
}

const stateKey = (s: ActivityState): string => `${s.kind}\n${s.session}\n${s.mac}\n${s.since}\n${s.approvals}`;

/**
 * Drives the Live Activity from the store while the app is in the foreground, at most once per
 * `DRIVE_GAP_MS`: starts it while something waits or runs, updates it as that changes, and ends it
 * `IDLE_END_MS` after every session went idle. Writes the App Group's entries whenever the waiting
 * approvals change, and shows the activity again after each write so the widget reads them anew.
 * Turned off, it ends the activity, tells every Mac to forget the tokens and empties the entries.
 *
 * One Mac drives the Island while the phone is away: the owner, the Mac of the current lead session
 * (else the one saved from before, else the first paired). Only it is given the phone's ActivityKit
 * tokens, each time they change, over a link that is up (out of reach, on its next connection);
 * every other Mac is told `clear_activity`, so no two hubs fight over one activity. The owner
 * changes only while the app is in front, and is saved, so a launch in the background hands over
 * to the same Mac. The other Macs still push notifications.
 *
 * One activity, two drivers: an activity that is already there (the hub started it while the phone
 * was away) is adopted as soon as ActivityKit hands over its token, and driven from then on, done
 * or not; one that lands while the switch is off is ended. A token that comes while the app is in
 * the background is handed to the owner over a background connection (`Fleet.sendInBackground`),
 * so its hub can update that activity. One the user dismissed is not started again until what it
 * would show changes. Whether the phone has Live
 * Activities (`isSupported()`, which the user can change in the phone's settings) is asked again at
 * each foreground: gone, every Mac forgets the tokens and the entries empty, as with the switch off.
 */
export class LiveActivity {
  private supportedNow: boolean;
  /** Following the store, the Fleet and the native events: from the first time it is supported. */
  private attached = false;
  private readonly api: LiveApi;
  private readonly fleet: ActivityFleet;
  private readonly store: OctetStore;
  private readonly now: () => number;
  private readonly schedule: (fn: () => void, ms: number) => () => void;
  private readonly model = new ActivityModel();

  /** The setting, once read. */
  private on: boolean | null = null;
  private foreground = false;
  private stopped = true;
  private offs: (() => void)[] = [];

  /** Whether an activity the app started (or took over) may still run. */
  private live = false;
  /** The last activity known ended or was dismissed, and none is live since: it is not started
   * again for nothing (coming back to the app, the entries written again), only for a change. */
  private dismissed = false;
  /** Each live activity's push token, by its id ("" for an event that names none), the newest last. */
  private readonly activities = new Map<string, string>();
  /** What the activity was last given, or tried with. */
  private sent: string | null = null;
  private lastAt = -Infinity;
  private trailing: (() => void) | null = null;
  private idleTimer: (() => void) | null = null;
  private running = false;
  private again = false;
  private force = false;

  private tokens: ActivityTokens = { token: null, start: null };
  /** The Mac that drives the Island while the phone is away; null with none paired. */
  private owner: string | null = null;
  /** What each link's Mac took (a registration, or `clear_activity`), and what is on its way to it.
   * By client: a Mac paired again is a new device to its hub. Reset when the switch goes off, so
   * every Mac is told again; and each launch tells each Mac that isn't the owner once. */
  private told = new WeakMap<HubClient, string>();
  private telling = new WeakMap<HubClient, string>();
  /** What each link's Mac was last handed over in the background, answered or not: a hand-over is
   * tried once per Mac for a body, so nothing in the background tries it again and again. */
  private handedOver = new WeakMap<HubClient, string>();
  /** What each link's Mac refused for now and was given a second try at: one per body. */
  private retried = new WeakMap<HubClient, string>();
  /** The second try waiting, by Mac. */
  private readonly retries = new Map<string, () => void>();

  /** The App Group's entries as last written; null when they are to be written again. */
  private entries: string | null = null;
  private macs: readonly { id: string; host: string }[] = [];

  private readonly denies: IslandDenies;
  /** Settings' switch, following `supported`. */
  private readonly listeners = new Set<() => void>();

  constructor({ api = nativeLive, fleet, store, now = Date.now, schedule }: LiveActivityOptions) {
    this.api = api;
    this.fleet = fleet;
    this.store = store;
    this.now = now;
    this.schedule =
      schedule ??
      ((fn, ms) => {
        const id = setTimeout(fn, ms);
        return () => clearTimeout(id);
      });
    this.supportedNow = api.isSupported();
    this.denies = new IslandDenies({ fleet, store, now: this.now, inFront: () => this.foreground });
  }

  /** For Settings: told when `supported` changes. */
  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => void this.listeners.delete(fn);
  };

  /** Whether the phone has Live Activities, as last asked (at the start and each foreground). */
  get supported(): boolean {
    return this.supportedNow;
  }

  /** The switch, as saved; on until the setting is read. */
  get enabled(): boolean {
    return this.on ?? true;
  }

  /** Starts following the store, the Fleet and the native events; where unsupported, nothing, until
   * a foreground finds it supported. */
  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    if (this.supportedNow) this.attach();
  }

  private attach(): void {
    this.attached = true;
    this.macs = this.fleet.snapshot().macs;
    this.model.observe(rowsOf(this.store.getState().sessions), this.now());
    this.offs = [
      this.store.subscribe((state, prev) => this.storeChanged(state, prev)),
      this.fleet.subscribe(() => this.fleetChanged()),
    ];
    const subscriptions = [
      this.api.addPushToStartTokenListener((e) => this.startToken(e.token)),
      this.api.addPushTokenListener((e) => this.activityToken(e)),
      this.api.addDenyListener((e) => this.deny(e)),
    ];
    this.offs.push(() => {
      for (const s of subscriptions) s.remove();
    });
    if (this.on !== null) {
      this.syncAll();
      this.writeEntries();
      this.drive(true);
      return;
    }
    void loadSettings().then((s) => {
      if (this.stopped || this.on !== null) return;
      this.on = s.liveActivity;
      if (s.liveActivityMac !== null && this.owner === null) this.owner = s.liveActivityMac;
      this.chooseOwner();
      this.syncAll();
      if (!this.on) {
        // One that landed before the setting was read, with the switch off: ended.
        if (this.activities.size > 0) this.endUnwanted();
        return;
      }
      this.writeEntries();
      this.drive(true);
      // Launched in the background for a token (iOS woke a closed app): it came before the setting.
      if (!this.foreground) this.handOver();
    });
  }

  stop(): void {
    this.stopped = true;
    this.attached = false;
    for (const off of this.offs) off();
    this.offs = [];
    this.cancelTimers();
    for (const cancel of this.retries.values()) cancel();
    this.retries.clear();
  }

  /** The app came to the foreground, or left it: the activity is driven only in the foreground
   * (the hub drives it while the phone is away). In front again, every Mac whose link is up is told
   * what it is to hold, unless it took that already: an old owner that missed its `clear_activity`
   * (out of reach when the tokens moved, or it refused) forgets them now; one whose link is still
   * coming is told when it is ready. */
  setForeground(active: boolean): void {
    this.foreground = active;
    if (!active) {
      this.cancelTimers();
      return;
    }
    const supported = this.api.isSupported();
    if (supported !== this.supportedNow && !this.stopped) this.supportChanged(supported);
    this.chooseOwner();
    this.syncAll();
    this.drive(true);
  }

  /** Live Activities came or went in the phone's settings. Gone: as with the switch off, every Mac
   * is told to forget the tokens, then the App Group empties. Back: registered again. */
  private supportChanged(supported: boolean): void {
    this.supportedNow = supported;
    for (const fn of [...this.listeners]) fn();
    if (!this.attached) {
      if (supported) this.attach();
      return;
    }
    this.told = new WeakMap();
    this.telling = new WeakMap();
    this.handedOver = new WeakMap();
    this.entries = null;
    if (!supported) {
      this.cancelTimers();
      this.live = false;
      this.sent = null;
      this.syncAll();
      if (this.on === true) void this.api.setEntries({}).catch(() => undefined);
      return;
    }
    this.syncAll();
    this.writeEntries();
  }

  /** Turns the Live Activity on or off, saved first. Off: the activity ends, every Mac is told to
   * forget the tokens, and the App Group is emptied. On: every Mac is registered again. False when
   * the setting could not be saved (nothing changes then). */
  async setEnabled(on: boolean): Promise<boolean> {
    if (!(await saveSettings({ liveActivity: on }))) return false;
    this.on = on;
    if (!this.attached || this.stopped) return true;
    if (!on) {
      this.cancelTimers();
      this.live = false;
      this.sent = null;
      this.entries = null;
      // Every Mac is told before the end, once: the end makes ActivityKit report the token gone,
      // and the sync that sets off finds each clear on its way already.
      this.told = new WeakMap();
      this.telling = new WeakMap();
      this.handedOver = new WeakMap();
      this.syncAll();
      await this.api.end().catch(() => undefined);
      await this.api.setEntries({}).catch(() => undefined);
      return true;
    }
    this.syncAll();
    this.writeEntries();
    this.drive(true);
    return true;
  }

  // What the activity shows

  /** Whether the activity is wanted: the switch on and the phone supporting it; null until the
   * setting is read. */
  private wanted(): boolean | null {
    return this.on === null ? null : this.on && this.supportedNow;
  }

  /** Wanted, started and in the foreground. */
  private driving(): boolean {
    return this.wanted() === true && this.attached && this.foreground && !this.stopped;
  }

  private hostOf = (mac: string): string | null => this.macs.find((m) => m.id === mac)?.host ?? null;

  /** Picks the Mac that drives the Island: while the app is in front, the lead session's Mac;
   * otherwise (or with no lead) the owner it had, while that one is still paired; else the first
   * paired. A change is saved, and moves the tokens there (the old owner is told to forget them). */
  private chooseOwner(): void {
    // Not before the saved owner and the pairings are both read: either alone would pick wrongly.
    if (!this.attached || this.stopped || this.on === null || this.fleet.snapshot().status === "loading") return;
    const paired = this.macs.map((m) => m.id);
    let next = this.owner !== null && paired.includes(this.owner) ? this.owner : (paired[0] ?? null);
    if (this.foreground) {
      const lead = this.model.content(this.now()).key;
      const mac = lead === "" ? null : macOfKey(lead);
      if (mac !== null && paired.includes(mac)) next = mac;
    }
    if (next === this.owner) return;
    this.owner = next;
    if (next === null || MAC_ID.test(next)) void saveSettings({ liveActivityMac: next });
    this.syncAll();
  }

  /** The content state now: the lead's raw id and its Mac's host, as the hub sends them. */
  private stateNow(): ActivityState {
    const lead = this.model.content(this.now());
    const split = lead.key === "" ? null : splitKey(lead.key);
    return {
      kind: lead.kind,
      session: split?.raw ?? "",
      mac: split ? (this.hostOf(split.mac) ?? "") : "",
      since: Math.floor(lead.since / 1000),
      approvals: lead.approvals,
    };
  }

  /** Brings the activity up to date, at most once per `DRIVE_GAP_MS`; `force` shows it again even
   * when nothing changed (after the entries were written). */
  private drive(force = false): void {
    if (force) this.force = true;
    if (!this.driving()) return;
    if (this.running) {
      this.again = true;
      return;
    }
    if (this.trailing) return; // a later drive is coming
    const wait = this.lastAt + DRIVE_GAP_MS - this.now();
    if (wait > 0) {
      this.trailing = this.schedule(() => {
        this.trailing = null;
        this.drive();
      }, wait);
      return;
    }
    void this.step();
  }

  private async step(): Promise<void> {
    const now = this.now();
    const state = this.stateNow();
    const key = stateKey(state);
    // Forced (coming back, the entries written again) shows a live activity again, but starts no
    // new one over one the user dismissed: only a change does.
    const changed = key !== this.sent || (this.force && (this.live || !this.dismissed));
    let call: "start" | "update" | "end" | null = null;
    if (state.kind === "done") {
      const idle = this.model.idleSince() ?? now;
      if (this.live && now >= idle + IDLE_END_MS) call = "end";
      else if (this.live) {
        if (changed) call = "update";
        this.idleTimer?.();
        this.idleTimer = this.schedule(() => {
          this.idleTimer = null;
          this.drive();
        }, idle + IDLE_END_MS - now);
      }
    } else if (changed) {
      call = this.live ? "update" : "start";
    }
    this.force = false;
    if (call === null) return;
    this.running = true;
    this.lastAt = now;
    this.sent = key;
    try {
      if (call === "start") {
        await this.api.start(state);
        this.live = true;
        this.dismissed = false;
        // Switched off while it was starting: the end went first, so this one ends now.
        if (this.on === false) {
          this.live = false;
          await this.api.end();
        }
      } else if (call === "update") await this.api.update(state);
      else {
        this.live = false;
        await this.api.end();
      }
    } catch {
      // Live Activities turned off on the phone, or the activity is gone: it is tried again when
      // what it would show changes.
      if (call === "start") this.live = false;
    } finally {
      this.running = false;
      if (this.again) {
        this.again = false;
        this.drive();
      }
    }
  }

  private cancelTimers(): void {
    this.trailing?.();
    this.trailing = null;
    this.idleTimer?.();
    this.idleTimer = null;
  }

  private storeChanged(state: OctetState, prev: OctetState): void {
    if (state.sessions !== prev.sessions) {
      this.model.observe(rowsOf(state.sessions), this.now());
      if (this.foreground) this.chooseOwner();
      this.writeEntries();
      this.drive();
    }
    if (state.hubs !== prev.hubs) {
      for (const [mac, hub] of state.hubs) {
        if (hub !== "ready" || prev.hubs.get(mac) === "ready") continue;
        this.sync(mac);
        this.denies.ready(mac);
      }
    }
  }

  private fleetChanged(): void {
    const snapshot = this.fleet.snapshot();
    this.denies.fleetChanged();
    const macs = snapshot.macs;
    if (macs !== this.macs) {
      this.macs = macs;
      this.chooseOwner();
      this.writeEntries();
      this.drive();
    }
    // Launched in the background for a token: it came before the pairings were read.
    if (snapshot.status !== "loading" && !this.foreground) this.handOver();
  }

  private deny(event: DenyEvent): void {
    this.denies.deny(event);
  }

  // The App Group

  /** Writes the entries when they changed, then shows the activity again. */
  private writeEntries(): void {
    if (this.wanted() !== true || !this.attached || this.stopped) return;
    const entries = widgetEntries(this.store.getState().sessions, this.hostOf);
    const text = JSON.stringify(entries);
    if (text === this.entries) return;
    this.entries = text;
    this.api.setEntries(entries).then(
      () => this.drive(true),
      () => {
        if (this.entries === text) this.entries = null; // the next change writes them again
      },
    );
  }

  // Tokens

  private startToken(token: string | null): void {
    if (token === null || token === this.tokens.start) return;
    this.tokens = { ...this.tokens, start: token };
    this.syncAll();
  }

  /** A live activity's token, or its end. The token registered is the newest live activity's: the
   * end of another (older, or already over) leaves it. A token means an activity is live, whoever
   * started it: it is adopted and driven from now on. */
  private activityToken({ token, activity = "" }: TokenEvent): void {
    // The end of an activity it never saw (one already over when the native side looked) says
    // nothing of the one that is live.
    if (token === null && activity !== "" && !this.activities.has(activity)) return;
    if (token === null) {
      this.activities.delete(activity);
      if (this.activities.size === 0) {
        // The last one ended (or was dismissed): the next change starts one, not this one.
        this.live = false;
        this.dismissed = true;
        if (this.driving()) this.sent = stateKey(this.stateNow());
      }
    } else {
      this.activities.delete(activity);
      this.activities.set(activity, token);
      if (this.wanted() === false) {
        // It landed with the switch off (or Live Activities gone): ended, not adopted.
        this.endUnwanted();
      } else {
        this.live = true;
        this.dismissed = false;
        this.drive(true);
      }
    }
    const newest = [...this.activities.values()].at(-1) ?? null;
    if (newest === this.tokens.token) return;
    this.tokens = { ...this.tokens, token: newest };
    this.syncAll();
    if (newest !== null && !this.foreground) this.handOver();
  }

  /** An activity that is not wanted (it landed with the switch off): every one is ended. */
  private endUnwanted(): void {
    this.live = false;
    void this.api.end().catch(() => undefined);
  }

  /** In the background every link is closed: the new token goes to the owner, if it lacks it, over
   * a background connection (`Fleet.sendInBackground`), so its hub can update the activity while
   * the phone is away. Tried when the token comes, and again once the setting and the pairings are
   * read (a closed app that iOS launched in the background for it), at most once for the same
   * body. This needs iOS to give the app time in the background for the token, and the phone
   * unlocked (a Mac's device token is in the keychain, readable only then: a locked phone's
   * pairings have no link); without either the owner gets the token on the next foreground. */
  private handOver(): void {
    if (this.wanted() !== true || this.stopped || this.tokens.token === null) return;
    const body = activityRegistration(this.tokens);
    if (body === null) return;
    const said = JSON.stringify(body);
    const macs = (this.owner === null ? [] : [this.owner]).filter((mac) => {
      const client = this.fleet.links.client(mac);
      return client !== null && this.told.get(client) !== said && this.telling.get(client) !== said && this.handedOver.get(client) !== said;
    });
    if (macs.length === 0) return;
    for (const mac of macs) {
      const client = this.fleet.links.client(mac);
      if (!client) continue;
      this.telling.set(client, said);
      this.handedOver.set(client, said);
    }
    void this.fleet.sendInBackground(macs, body).then((acked) => {
      for (const mac of macs) {
        const client = this.fleet.links.client(mac);
        if (!client || this.telling.get(client) !== said) continue;
        this.telling.delete(client);
        if (acked.includes(mac)) this.told.set(client, said);
        else this.sync(mac); // back in the foreground meanwhile: over the link
      }
    });
  }

  private syncAll(): void {
    for (const mac of this.fleet.links.macs()) this.sync(mac);
  }

  /** Tells Mac `mac` what it is to hold, over a link that is up, unless it has it already or it is
   * on its way: the tokens, for the owner while the Live Activity is wanted; `clear_activity` for
   * every other Mac, and for every Mac while it is off. A refusal for now (`RETRIED`: busy, rate
   * limited, not saved) is tried once more `REGISTER_RETRY_MS` on, if that Mac is still to hold
   * the same; any refusal, and a dropped link, is tried again on the next change, connection or
   * foreground. */
  private sync(mac: string): void {
    const client = this.fleet.links.client(mac);
    if (!client || client.state !== "ready" || this.stopped || !this.attached) return;
    const wanted = this.wanted();
    if (wanted === null) return; // the setting is not read yet
    const body = wanted && mac === this.owner ? activityRegistration(this.tokens) : CLEAR;
    if (body === null) return;
    const said = JSON.stringify(body);
    if (this.told.get(client) === said || this.telling.get(client) === said) return;
    this.telling.set(client, said);
    sendTo(this.fleet.links, mac, body)
      .then(
        () => {
          if (this.telling.get(client) === said) this.told.set(client, said);
          if (this.retried.get(client) === said) this.retried.delete(client);
        },
        (error: unknown) => {
          if (!(error instanceof Refusal) || !RETRIED.has(error.code) || this.retried.get(client) === said) return;
          this.retried.set(client, said);
          this.retryLater(mac);
        },
      )
      .finally(() => {
        if (this.telling.get(client) === said) this.telling.delete(client);
      });
  }

  /** Syncs Mac `mac` again `REGISTER_RETRY_MS` from now: whatever it is to hold then, which a
   * newer registration may have given it already. */
  private retryLater(mac: string): void {
    this.retries.get(mac)?.();
    this.retries.set(
      mac,
      this.schedule(() => {
        this.retries.delete(mac);
        this.sync(mac);
      }, REGISTER_RETRY_MS),
    );
  }
}

/** The native module the shell's controller uses: the app's, unless a test gives a fake. */
export const LiveApiContext = createContext<LiveApi>(nativeLive);

/** The shell's Live Activity controller (app/_layout.tsx), for Settings' switch. */
export const LiveActivityContext = createContext<LiveActivity | null>(null);

export const useLiveActivity = (): LiveActivity | null => useContext(LiveActivityContext);
