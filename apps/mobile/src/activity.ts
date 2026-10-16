// The Live Activity while the app runs (the Dynamic Island and the Lock Screen, iOS 17.2 and later):
// the app shows what every Mac's sessions are doing, gives each Mac the ActivityKit tokens so its
// hub can start and update the activity while the phone is away, keeps the widget's App Group
// entries (names and commands, which never leave the phone), and answers the Island's Deny. The
// Island's Allow is a link (src/navigation.ts), opened by the shell behind Face ID and the arm.
// Where Live Activities are not supported (Android, Expo Go, jest) it does nothing at all.
import { createContext, useContext } from "react";

import * as octetLive from "../modules/octet-live";
import type { ActivityState, DenyEvent, Subscription, TokenEvent, WidgetEntry } from "../modules/octet-live";
import { ActivityModel, IDLE_END_MS, activityRegistration, rowsOf, widgetEntries, type ActivityTokens } from "./activityModel";
import { Refusal, type HubClient } from "./client/HubClient";
import { macOfKey, splitKey, type Links } from "./macs";
import { parseApprovalId } from "./navigation";
import { loadSettings, saveSettings } from "./persist";
import { sendTo, sendVia } from "./sendVia";
import { hasEnded, type OctetState, type OctetStore } from "./store";

export {
  ActivityModel,
  COMMAND_CHARS,
  ENTRIES_BYTES,
  ENTRIES_LIMIT,
  IDLE_END_MS,
  NAME_CHARS,
  activityRegistration,
  rowsOf,
  widgetEntries,
  type ActivityRow,
  type ActivityTokens,
  type Lead,
} from "./activityModel";

/** The least time between two changes the app makes to the activity. */
export const DRIVE_GAP_MS = 1_000;
/** Denies remembered, so the same one is answered once. */
const DENIES_SEEN = 64;
/** Denies kept for a Mac out of reach, at most, and for how long: the native side drops older. */
const DENIES_HELD = 16;
const DENY_MAX_AGE_MS = 10 * 60_000;

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
export interface ActivityFleet {
  /** `status` is "loading" until the saved pairings are read. */
  snapshot(): { status: string; macs: readonly { id: string; host: string }[] };
  subscribe(fn: () => void): () => void;
  readonly links: Links;
  keyFor(host: string | null, raw: string): string | null;
}

export interface LiveActivityOptions {
  api?: LiveApi;
  fleet: ActivityFleet;
  store: OctetStore;
  /** Clock and timer overrides (tests). */
  now?: () => number;
  schedule?: (fn: () => void, ms: number) => () => void;
}

/** A Deny waiting for its Mac to be reached. */
interface HeldDeny {
  key: string;
  approval: number;
  at: number;
}

const stateKey = (s: ActivityState): string => `${s.kind}\n${s.session}\n${s.mac}\n${s.since}\n${s.approvals}`;

/**
 * Drives the Live Activity from the store while the app is in the foreground, at most once per
 * `DRIVE_GAP_MS`: starts it while something waits or runs, updates it as that changes, and ends it
 * `IDLE_END_MS` after every session went idle. Registers the phone's ActivityKit tokens with every
 * Mac, each time they change, over a link that is up (a Mac out of reach is told on its next
 * connection). Writes the App Group's entries whenever the waiting approvals change, and shows the
 * activity again after each write so the widget reads them anew. Answers each Deny once, to the Mac
 * its host names. Turned off, it ends the activity, tells every Mac to forget the tokens and
 * empties the entries.
 */
export class LiveActivity {
  readonly supported: boolean;
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
  /** What the activity was last given, or tried with. */
  private sent: string | null = null;
  private lastAt = -Infinity;
  private trailing: (() => void) | null = null;
  private idleTimer: (() => void) | null = null;
  private running = false;
  private again = false;
  private force = false;

  private tokens: ActivityTokens = { token: null, start: null };
  /** What each link's Mac took, and what is on its way to it. By client: a Mac paired again is a
   * new device to its hub. */
  private told = new WeakMap<HubClient, string>();
  private telling = new WeakMap<HubClient, string>();
  /** Links whose Mac took `clear_activity` since the switch was last turned off. With it off, each
   * other link's Mac is told on its next connection: one that missed the turn-off, in this run or
   * an earlier one, would go on starting activities the user turned off. */
  private cleared = new WeakSet<HubClient>();

  /** The App Group's entries as last written; null when they are to be written again. */
  private entries: string | null = null;
  private macs: readonly { id: string; host: string }[] = [];

  private readonly denied: string[] = [];
  private held: HeldDeny[] = [];
  /** Denies that came before the pairings were read (the native side hands over the taps made
   * while the app was closed as soon as it is listened to): resolved once they are. */
  private early: DenyEvent[] = [];

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
    this.supported = api.isSupported();
  }

  /** The switch, as saved; on until the setting is read. */
  get enabled(): boolean {
    return this.on ?? true;
  }

  /** Starts following the store, the Fleet and the native events. Nothing where unsupported. */
  start(): void {
    if (!this.supported || !this.stopped) return;
    this.stopped = false;
    this.macs = this.fleet.snapshot().macs;
    this.model.observe(rowsOf(this.store.getState().sessions), this.now());
    this.offs = [
      this.store.subscribe((state, prev) => this.storeChanged(state, prev)),
      this.fleet.subscribe(() => this.fleetChanged()),
    ];
    const subscriptions = [
      this.api.addPushToStartTokenListener((e) => this.startToken(e.token)),
      this.api.addPushTokenListener((e) => this.activityToken(e.token)),
      this.api.addDenyListener((e) => this.deny(e)),
    ];
    this.offs.push(() => {
      for (const s of subscriptions) s.remove();
    });
    void loadSettings().then((s) => {
      if (this.stopped || this.on !== null) return;
      this.on = s.liveActivity;
      this.syncAll();
      if (!this.on) return;
      this.writeEntries();
      this.drive(true);
    });
  }

  stop(): void {
    this.stopped = true;
    for (const off of this.offs) off();
    this.offs = [];
    this.cancelTimers();
  }

  /** The app came to the foreground, or left it: the activity is driven only in the foreground
   * (the hub drives it while the phone is away). */
  setForeground(active: boolean): void {
    this.foreground = active;
    if (active) this.drive(true);
    else this.cancelTimers();
  }

  /** Turns the Live Activity on or off, saved first. Off: the activity ends, every Mac is told to
   * forget the tokens, and the App Group is emptied. On: every Mac is registered again. False when
   * the setting could not be saved (nothing changes then). */
  async setEnabled(on: boolean): Promise<boolean> {
    if (!(await saveSettings({ liveActivity: on }))) return false;
    this.on = on;
    if (!this.supported || this.stopped) return true;
    if (!on) {
      this.cancelTimers();
      this.live = false;
      this.sent = null;
      this.entries = null;
      await this.api.end().catch(() => undefined);
      await this.api.setEntries({}).catch(() => undefined);
      this.told = new WeakMap();
      this.telling = new WeakMap();
      this.cleared = new WeakSet();
      this.syncAll();
      return true;
    }
    this.syncAll();
    this.writeEntries();
    this.drive(true);
    return true;
  }

  // What the activity shows

  /** On, read, started and in the foreground. */
  private driving(): boolean {
    return this.supported && this.on === true && this.foreground && !this.stopped;
  }

  private hostOf = (mac: string): string | null => this.macs.find((m) => m.id === mac)?.host ?? null;

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
    const changed = key !== this.sent || this.force;
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
      this.writeEntries();
      this.drive();
    }
    if (state.hubs !== prev.hubs) {
      for (const [mac, hub] of state.hubs) {
        if (hub !== "ready" || prev.hubs.get(mac) === "ready") continue;
        this.sync(mac);
        this.flushDenies(mac);
      }
    }
  }

  private fleetChanged(): void {
    const snapshot = this.fleet.snapshot();
    if (snapshot.status !== "loading" && this.early.length > 0) {
      const early = this.early;
      this.early = [];
      for (const event of early) this.deny(event);
    }
    const macs = snapshot.macs;
    if (macs === this.macs) return;
    this.macs = macs;
    this.writeEntries();
    this.drive();
  }

  // The App Group

  /** Writes the entries when they changed, then shows the activity again. */
  private writeEntries(): void {
    if (!this.supported || this.on !== true || this.stopped) return;
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

  private activityToken(token: string | null): void {
    if (token === null) {
      // That activity ended (or was dismissed): the next change starts one, not this one.
      this.live = false;
      if (this.driving()) this.sent = stateKey(this.stateNow());
    }
    if (token === this.tokens.token) return;
    this.tokens = { ...this.tokens, token };
    this.syncAll();
  }

  private syncAll(): void {
    for (const mac of this.fleet.links.macs()) this.sync(mac);
  }

  /** Tells Mac `mac` the tokens (or, switched off, to forget them), over a link that is up, unless
   * it has them already or they are on their way. */
  private sync(mac: string): void {
    const client = this.fleet.links.client(mac);
    if (!client || client.state !== "ready" || this.stopped) return;
    if (this.on === null) return; // the setting is not read yet
    if (!this.on) {
      if (this.cleared.has(client)) return;
      this.cleared.add(client);
      sendTo(this.fleet.links, mac, { type: "clear_activity" }).catch(() => {
        this.cleared.delete(client); // told on its next connection
      });
      return;
    }
    const body = activityRegistration(this.tokens);
    if (body === null) return;
    const said = JSON.stringify(body);
    if (this.told.get(client) === said || this.telling.get(client) === said) return;
    this.telling.set(client, said);
    sendTo(this.fleet.links, mac, body)
      .then(
        () => {
          if (this.telling.get(client) === said) this.told.set(client, said);
        },
        () => undefined, // a refusal or a dropped link: tried on the next change or connection
      )
      .finally(() => {
        if (this.telling.get(client) === said) this.telling.delete(client);
      });
  }

  // Deny

  private deny(event: DenyEvent): void {
    if (this.fleet.snapshot().status === "loading") {
      this.early = [...this.early, event].slice(-DENIES_HELD);
      return;
    }
    const approval = parseApprovalId(event.approval);
    if (approval === null) return;
    const mark = `${event.mac}\n${event.session}\n${approval}`;
    if (this.denied.includes(mark)) return;
    this.denied.push(mark);
    if (this.denied.length > DENIES_SEEN) this.denied.shift();
    const key = this.fleet.keyFor(event.mac === "" ? null : event.mac, event.session);
    if (key !== null) this.sendDeny({ key, approval, at: this.now() });
  }

  /** Denies `held.approval` of session `held.key` on its Mac, or keeps it for the Mac's next
   * connection. Not sent for a session its Mac has listed as over, or as waiting on nothing. */
  private sendDeny(held: HeldDeny): void {
    const mac = macOfKey(held.key);
    if (mac === null) return;
    const state = this.store.getState();
    if (state.listed.has(mac)) {
      if (hasEnded(state, held.key)) return;
      const row = state.sessions.find((s) => s.id === held.key);
      if (row && row.approvals === 0) return;
    }
    const client = this.fleet.links.client(mac);
    if (client?.state !== "ready") {
      this.hold(held);
      return;
    }
    sendVia(this.fleet.links, held.key, { type: "answer", session: held.key, approval: held.approval, allow: false }).catch((error: unknown) => {
      if (error instanceof Refusal && error.code === "unsent") this.hold(held);
    });
  }

  private hold(held: HeldDeny): void {
    this.held = [...this.held.filter((h) => !(h.key === held.key && h.approval === held.approval)), held].slice(-DENIES_HELD);
  }

  private flushDenies(mac: string): void {
    const now = this.now();
    const mine = this.held.filter((h) => macOfKey(h.key) === mac);
    if (mine.length === 0) return;
    this.held = this.held.filter((h) => macOfKey(h.key) !== mac);
    for (const held of mine) if (now - held.at <= DENY_MAX_AGE_MS) this.sendDeny(held);
  }
}

/** The native module the shell's controller uses: the app's, unless a test gives a fake. */
export const LiveApiContext = createContext<LiveApi>(nativeLive);

/** The shell's Live Activity controller (app/_layout.tsx), for Settings' switch. */
export const LiveActivityContext = createContext<LiveActivity | null>(null);

export const useLiveActivity = (): LiveActivity | null => useContext(LiveActivityContext);
