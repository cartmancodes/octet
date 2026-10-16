// The Fleet: every pairing, one `MacLink` each, and what pairing, unpairing, the app's lifecycle
// and notifications do to them.
import { Platform } from "react-native";

import { HubClient, Refusal, type HubClientOptions, type SocketFactory } from "../client/HubClient";
import { NOT_A_LINK, isHubUrl, pairLinkProblem, parsePairLink } from "../client/link";
import { webSocketFactory } from "../client/socket";
import { DEFAULT_PUSH_KINDS, NOT_SAVED, loadSettings, readSettings, updateSettings, type Settings } from "../persist";
import { createCoalescer, type Coalescer } from "../coalesce";
import { MAX_MACS, TOO_MANY_MACS, cleanMacName, hostOf, keyFor, macNameOf, macOfHost, newMacId, tapKey, type Links, type Pairing, type Tap } from "../macs";
import { askForToken, currentToken, dropPushRegistration, projectId } from "../notifications";
import type { PushKinds } from "../protocol";
import { hasEnded, octetStore, type OctetStore } from "../store";
import { TOKEN_KEY, keepToken, secureStore, tokenKeyOf, tokenStore, type Keychain, type TokenStore } from "./keychain";
import { MacLink, UNREACHABLE, type LinkHost, type MacInfo, type PushResult } from "./macLink";

export { NOT_A_LINK };
/** The Mac revoked this phone: only that pairing goes. */
export const unpairedNotice = (macName: string): string => `This phone was unpaired on ${macName}`;
export const UNPAIR_FAILED_NOTICE = "This phone couldn't delete its pairing. Pair again to replace it.";
/** The Mac took the pairing (its secret is spent and the name is registered) but the phone could
 * not keep the token: only revoking on the Mac frees the name. */
const unsaved = (name: string): string =>
  `This phone paired but couldn't save it. On the Mac run \`octet hub revoke ${name}\`, then pair again with a new code.`;

/** `loading` until the saved pairings are read; `paired` with at least one link; `unpaired`
 * shows Pair. */
export type HubStatus = "loading" | "unpaired" | "paired";

export interface FleetSnapshot {
  status: HubStatus;
  /** The paired Macs, in the order paired. */
  macs: MacInfo[];
  /** Just paired, and the notifications step is up: Pair stays until `finishOffer`. */
  offer: boolean;
  /** Why some Mac did not take this phone's push registration (or why the phone could not make
   * one), for Settings: one line per Mac, named when there are several. */
  notices: string[];
  /** Why the user is on Pair (a Mac unpaired this phone, or an unpair did not finish); null for
   * a plain first pairing. Set too when one of several Macs unpaired this phone. */
  notice: string | null;
}

/** Whether any kind of push is wanted. */
const anyKind = (kinds: PushKinds): boolean => kinds.approvals || kinds.errors || kinds.turns;

/** A failed pairing: `clear` says the link is no good any more (the Mac says its code is spent
 * or expired, or took the secret), so the screen forgets it; any other link is kept, to edit or
 * to try again. */
export type PairResult = { ok: true } | { ok: false; message: string; clear: boolean };

export interface FleetOptions {
  factory?: SocketFactory;
  store?: OctetStore;
  /** Where the tokens are kept; the phone's keychain unless a test gives its own. */
  keychain?: Keychain;
  /** "ios" or "android"; sent to the Mac at pairing and names the default device. */
  platform?: string;
  /** Clock and timer overrides for the clients it makes (tests). */
  clientOptions?: Partial<Pick<HubClientOptions, "now" | "schedule" | "random">>;
  /** Makes a new pairing's id; `newMacId` unless a test gives its own. */
  newId?: () => string;
}

/**
 * Every pairing of the app run, one `MacLink` each: all connected in the foreground, all closed
 * in the background. The batches of every Mac go through one coalescer, so the store commits at
 * most once per 50 ms whichever Macs stream. Notifications are one choice for every Mac.
 */
export class Fleet {
  private readonly factory: SocketFactory;
  private readonly store: OctetStore;
  private readonly keychain: Keychain;
  private readonly platform: string;
  private readonly clientOptions: NonNullable<FleetOptions["clientOptions"]>;
  private readonly schedule: (fn: () => void, ms: number) => () => void;
  private readonly newId: () => string;
  private readonly coalescer: Coalescer;

  /** The saved pairings, as last saved: a pairing whose token could not be read has no link. */
  private pairings: Pairing[] = [];
  /** The links, in the pairings' order: the one map from Mac to client. */
  private all: MacLink[] = [];
  private current: FleetSnapshot = { status: "loading", macs: [], offer: false, notices: [], notice: null };
  private readonly listeners = new Set<() => void>();
  private foreground = false;
  private savedName: string | null = null;
  /** The client of a pairing in flight, which is no Mac's link until it succeeds. */
  private pairing: HubClient | null = null;
  /** The id that pairing will keep its token under: an unpair of the same Mac meanwhile must not
   * delete what it saves. */
  private pairingId: string | null = null;
  /** The pairing still waiting for an older version's token (see `migrate`); null when none. */
  private pending: string | null = null;
  /** What `macs` shows of each pairing with no link (its token unreadable, or still on its way),
   * kept so that the list keeps its identity. */
  private unlinked = new Map<string, MacInfo>();
  private starting: Promise<void> | null = null;
  /** Unpairs whose storage writes are not done: pairing waits for them. */
  private unpairing = 0;
  /** Push registrations wait their turn, so that each Mac ends with the last one asked for. */
  private pushQueue: Promise<unknown> = Promise.resolve();
  private readonly host: LinkHost;

  constructor(opts: FleetOptions = {}) {
    this.factory = opts.factory ?? webSocketFactory;
    this.store = opts.store ?? octetStore;
    this.keychain = opts.keychain ?? secureStore;
    this.platform = opts.platform ?? Platform.OS;
    this.clientOptions = opts.clientOptions ?? {};
    this.newId = opts.newId ?? newMacId;
    this.schedule =
      this.clientOptions.schedule ??
      ((fn, ms) => {
        const id = setTimeout(fn, ms);
        return () => clearTimeout(id);
      });
    const store = this.store;
    this.coalescer = createCoalescer((batches) => store.getState().applyBatches(batches), {
      now: this.clientOptions.now,
      schedule: this.schedule,
    });
    this.links = this.makeLinks();
    this.host = {
      store: this.store,
      coalescer: this.coalescer,
      links: this.links,
      schedule: this.schedule,
      inTurn: (job) => this.inTurn(job),
      changed: () => this.emit(),
      unpaired: (link) => this.macUnpaired(link),
    };
  }

  // What screens read

  snapshot = (): FleetSnapshot => this.current;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  /** Every Mac's client, as the Fleet has them at each call: one object for the Fleet's life, so
   * that a Mac coming or going (or paired again) changes nothing that holds it. A screen that
   * shows one Mac's client reads it with `useClient`. */
  readonly links: Links;

  /** "Android phone" on Android, "iPhone" otherwise. */
  defaultName(): string {
    return this.platform === "android" ? "Android phone" : "iPhone";
  }

  /** The name this phone paired under last, else the platform's. */
  initialName(): string {
    return this.savedName ?? this.defaultName();
  }

  // Lifecycle

  /** Reads the settings and the keychain, once however often it is called: moves the one
   * pairing of an older version to pairing 1, and connects each pairing whose token is there.
   * A keychain that cannot be read is no token for now: the pairing stays for the next launch. */
  init(): Promise<void> {
    this.starting ??= this.load();
    return this.starting;
  }

  private async load(): Promise<void> {
    const read = await readSettings();
    if (!read.ok) {
      // Settings that could not be read say nothing of the pairings: the tokens stay for the
      // next launch, and this one shows Pair.
      this.emit({ status: "unpaired" });
      return;
    }
    const settings = read.settings;
    this.savedName = settings.deviceName;
    await this.finishForgetting(settings);
    const { pairings, pending } = await this.migrate(settings);
    this.pending = pending;
    const tokens = await Promise.all(pairings.map((p) => this.keychain.get(tokenKeyOf(p.id)).then((t) => t, () => undefined)));
    const kept: Pairing[] = [];
    const links: MacLink[] = [];
    const dropped: string[] = [];
    pairings.forEach((pairing, i) => {
      const token = tokens[i];
      if (!isHubUrl(pairing.hubUrl)) {
        // An address that is not a pairing host (a tampered value) can reach nowhere it should.
        dropped.push(pairing.id);
      } else if (token === undefined || ((token === null || token === "") && pairing.id === pending)) {
        // Unreadable now, or the token is still on its way from the old entry: kept, not connected.
        kept.push(pairing);
      } else if (token === null || token === "") {
        // No token: it can reach nothing.
        dropped.push(pairing.id);
      } else {
        kept.push(pairing);
        const client = new HubClient({ ...this.clientOptions, url: pairing.hubUrl, token, factory: this.factory });
        links.push(new MacLink(this.host, pairing, client, tokenStore(this.keychain, pairing.id)));
      }
    });
    if (dropped.length > 0) {
      for (const id of dropped) await this.keychain.delete(tokenKeyOf(id)).catch(() => undefined);
      await updateSettings((s) => ({ pairings: s.pairings.filter((p) => !dropped.includes(p.id)) }));
    }
    this.pairings = kept;
    this.all = links;
    for (const link of links) link.attach();
    this.emit({ status: links.length > 0 ? "paired" : "unpaired" });
    if (this.foreground) for (const link of links) link.client.start();
  }

  /** Deletes the keychain entries of pairings that went while the keychain would not let go. */
  private async finishForgetting(settings: Settings): Promise<void> {
    if (settings.forget.length === 0) return;
    const done: string[] = [];
    for (const id of settings.forget) {
      // A pairing that is still saved keeps its entry (an id is never made twice, but be sure).
      if (settings.pairings.some((p) => p.id === id)) done.push(id);
      else if (await this.keychain.delete(tokenKeyOf(id)).then(() => true, () => false)) done.push(id);
    }
    if (done.length > 0) await updateSettings((s) => ({ forget: s.forget.filter((id) => !done.includes(id)) }));
  }

  /**
   * Moves the one pairing of an older version (settings' `hubUrl`, the token in `TOKEN_KEY`) to a
   * pairing of its own. Each step is safe to repeat, so a launch that dies part-way is finished
   * by the next: (1) the pairing is saved, with a fresh id, unless one for that host is saved
   * already; (2) the old token is copied to its entry and read back, unless its entry holds a
   * token already (an earlier copy, or a newer pairing); (3) only then the old entry is deleted;
   * (4) and `hubUrl` saved as null. An old token with no usable address is deleted, as before.
   * `pending` is the pairing still waiting for its token, when the move did not get that far.
   */
  private async migrate(settings: Settings): Promise<{ pairings: Pairing[]; pending: string | null }> {
    let pairings = settings.pairings;
    const legacy = settings.hubUrl;
    let old: string | null;
    try {
      old = await this.keychain.get(TOKEN_KEY);
    } catch {
      return { pairings, pending: null }; // unreadable: everything stays for the next launch
    }
    if (old === null || old === "") {
      if (legacy !== null) await updateSettings(() => ({ hubUrl: null })); // nothing to move
      return { pairings, pending: null };
    }
    if (legacy === null || !isHubUrl(legacy)) {
      // A token with no usable address (killed between the two saves, a tampered value, or a move
      // that saved its address as null) can reach nowhere it should: it goes.
      await this.keychain.delete(TOKEN_KEY).catch(() => undefined);
      if (legacy !== null) await updateSettings(() => ({ hubUrl: null }));
      return { pairings, pending: null };
    }
    const id = macOfHost(pairings, hostOf(legacy) ?? "");
    let target = pairings.find((p) => p.id === id);
    if (!target) {
      if (pairings.length >= MAX_MACS) {
        await this.keychain.delete(TOKEN_KEY).catch(() => undefined);
        await updateSettings(() => ({ hubUrl: null }));
        return { pairings, pending: null };
      }
      target = { id: this.freshId(pairings), hubUrl: legacy, deviceName: settings.deviceName ?? this.defaultName(), macName: macNameOf(legacy) };
      const next = [...pairings, target];
      if (!(await updateSettings(() => ({ pairings: next })))) return { pairings, pending: null };
      pairings = next;
    }
    const key = tokenKeyOf(target.id);
    try {
      const there = await this.keychain.get(key);
      if (there === null || there === "") {
        await this.keychain.set(key, old);
        if ((await this.keychain.get(key)) !== old) return { pairings, pending: target.id };
      }
      await this.keychain.delete(TOKEN_KEY);
    } catch {
      return { pairings, pending: target.id };
    }
    await updateSettings(() => ({ hubUrl: null }));
    return { pairings, pending: null };
  }

  /** The app came to the foreground (connect every link, or skip a backoff wait: the network may
   * well be back) or went to the background (close every socket and leave no timer, pings
   * included). `inactive` is not the background: a Face ID prompt passes through it. */
  setForeground(active: boolean): void {
    this.foreground = active;
    if (active) {
      for (const link of this.all) link.client.retry(); // a start when stopped; leaves a live or opening link alone
    } else {
      this.pairing?.stop();
      // A stop ends each link's run: back in the foreground each Mac is coming, not asleep, until
      // an attempt of the new run ends (the store's `tried`).
      for (const link of this.all) link.client.stop();
    }
  }

  /** Pairs with the Mac the link names: a new pairing, or, for a host already paired, a new
   * token for that pairing (its id and name kept). On success the token is in its keychain
   * entry, the pairing in settings, and the connection is up. A 5th Mac is refused before any
   * network call. Failures come back as the words to show. */
  async pair(link: string, name: string): Promise<PairResult> {
    if (this.pairing) return { ok: false, message: "Already pairing", clear: false };
    if (this.unpairing > 0) return { ok: false, message: "Finishing unpairing. Try again in a moment.", clear: false };
    const parsed = parsePairLink(link);
    if (!parsed) return { ok: false, message: pairLinkProblem(link) ?? NOT_A_LINK, clear: false };
    const sameId = macOfHost(this.pairings, hostOf(parsed.url) ?? "");
    if (sameId === null && this.pairings.length >= MAX_MACS) return { ok: false, message: TOO_MANY_MACS, clear: false };
    const deviceName = name.trim() === "" ? this.defaultName() : name.trim();
    // The id is made before the Mac spends its code: nothing after the Mac's answer can fail but
    // the saves, which say how to recover.
    const before = this.pairings.find((p) => p.id === sameId);
    const id = before?.id ?? this.freshId(this.pairings);
    const client = new HubClient({ ...this.clientOptions, url: parsed.url, token: null, factory: this.factory });
    this.pairing = client;
    this.pairingId = id;
    if (this.current.notice !== null) this.emit({ notice: null });
    try {
      const { token } = await client.pair(parsed.secret, deviceName, this.platform);
      const pairing: Pairing = { id, hubUrl: parsed.url, deviceName, macName: before?.macName ?? macNameOf(parsed.url) };
      const tokens = tokenStore(this.keychain, pairing.id);
      let notifications = false;
      const kept =
        (await keepToken(tokens, token)) &&
        (await updateSettings((s) => {
          notifications = s.notifications;
          return { pairings: withPairing(s.pairings, pairing), deviceName };
        }));
      if (!kept) {
        await tokens.clear().catch(() => undefined);
        return { ok: false, message: unsaved(deviceName), clear: true };
      }
      this.pairings = withPairing(this.pairings, pairing);
      this.savedName = deviceName;
      // The Mac's old link, when it pairs again, gives way to the new one.
      const old = this.all.find((l) => l.id === pairing.id);
      if (old) {
        old.client.stop();
        this.drop(old);
      }
      // The notifications step, unless they are on already (the new Mac is sent them at ready).
      this.adopt(pairing, client, tokens, projectId() !== null && !notifications);
      return { ok: true };
    } catch (error) {
      if (!(error instanceof Refusal)) return { ok: false, message: "Pairing failed", clear: false };
      // Only a spent or expired code is refused as `unpaired`; on any other refusal the Mac keeps
      // the code, and the link is good to try again (another device name, a moment later).
      return { ok: false, message: error.message, clear: error.code === "unpaired" };
    } finally {
      this.pairing = null;
      this.pairingId = null;
    }
  }

  /** A new pairing id, not one of `taken`'s. */
  private freshId(taken: readonly Pairing[]): string {
    let id = this.newId();
    for (let i = 0; i < 8 && taken.some((p) => p.id === id); i++) id = this.newId();
    return id;
  }

  /** Renames Mac `mac` (cleaned: 1 to 40 characters). False for a blank name or a Mac that is
   * not paired; the answer of the save otherwise. */
  async rename(mac: string, name: string): Promise<boolean> {
    const macName = cleanMacName(name);
    if (macName === null || !this.pairings.some((p) => p.id === mac)) return false;
    const renamed = (list: Pairing[]) => list.map((p) => (p.id === mac ? { ...p, macName } : p));
    this.pairings = renamed(this.pairings);
    this.all.find((l) => l.id === mac)?.rename(macName);
    this.emit();
    return updateSettings((s) => ({ pairings: renamed(s.pairings) }));
  }

  /**
   * The user unpairs Mac `mac` (with none, every Mac): each one's connection and what it sent
   * (keys, views, drafts, sent texts and toasts) are dropped at once, and Pair shows, with no
   * notice, when no Mac is left. Each ready link first tells its Mac "notifications off" (see
   * `MacLink.goodbye`); when no Mac is left and none of those unpaired had a link, the phone
   * drops its push registration instead. Then the pairing leaves settings and its token the keychain (tried
   * twice). If either will not go, the notice says so; a token left behind is deleted at the next
   * launch. The device name stays to prefill the next pairing. The Mac keeps listing the phone
   * until `octet hub revoke` there. Pairing waits for the writes, so they cannot delete a new
   * token.
   */
  async unpair(mac?: string): Promise<void> {
    // A pairing with no link (its token unreadable, or still on its way) is unpaired too.
    const ids = this.pairings.filter((p) => mac === undefined || p.id === mac).map((p) => p.id);
    if (ids.length === 0) return;
    this.unpairing += 1;
    try {
      const last = ids.length === this.pairings.length;
      const going = this.all.filter((l) => ids.includes(l.id));
      for (const link of going) {
        // Best effort, sent before the link is released and never waited on.
        link.goodbye(last);
        this.drop(link);
      }
      // No Mac is left, and none of these had a link to say "notifications off" on (each had its
      // token unreadable, or still on its way): the phone drops its push registration itself, so
      // that any Mac still pushing to it bounces and forgets the token.
      if (last && going.length === 0) void dropPushRegistration();
      const removed = this.removeStored(ids, last);
      // Before any await: a foreground event now finds no client to start.
      this.emit(this.all.length === 0 ? { status: "unpaired", notice: null, offer: false } : { notice: null });
      if (!(await removed)) this.emit({ notice: UNPAIR_FAILED_NOTICE });
    } finally {
      this.unpairing -= 1;
    }
  }

  /** The Mac refused this phone's token as unpaired or revoked: only its pairing goes (its token
   * deleted), and the notice names it; Pair shows when no Mac is left. Only the hub's refusal
   * gets here; a dropped network never does. */
  private macUnpaired(link: MacLink): void {
    if (!this.all.includes(link)) return;
    const last = this.pairings.every((p) => p.id === link.id);
    this.drop(link);
    this.unpairing += 1;
    void this.removeStored([link.id], last).finally(() => {
      this.unpairing -= 1;
    });
    const notice = unpairedNotice(link.pairing.macName);
    this.emit(this.all.length === 0 ? { status: "unpaired", notice, offer: false } : { notice });
  }

  /** Lets go of `link`: it stops feeding the store, and the store forgets what its Mac sent. */
  private drop(link: MacLink): void {
    link.release();
    this.all = this.all.filter((l) => l !== link);
    this.store.getState().reset(link.id);
  }

  /** Removes pairings `ids` from settings and their tokens from the keychain. With `last`, no Mac
   * is left: notifications go off with the kinds back at the defaults, so that a new pairing
   * starts there on the Mac (which forgot this phone's token with the phone). False when either
   * could not be done; a token left is in `forget`, for the next launch. */
  private async removeStored(ids: string[], last: boolean): Promise<boolean> {
    this.pairings = this.pairings.filter((p) => !ids.includes(p.id));
    // The pairing still waiting for an older version's token: the move ends with it, or the next
    // launch would make it again.
    const moving = this.pending !== null && ids.includes(this.pending);
    if (moving) this.pending = null;
    const settings = await updateSettings((s) => ({
      pairings: s.pairings.filter((p) => !ids.includes(p.id)),
      forget: [...new Set([...s.forget, ...ids])],
      ...(last || moving ? { hubUrl: null } : {}),
      ...(last ? { notifications: false, pushKinds: DEFAULT_PUSH_KINDS } : {}),
    }));
    if (moving) await this.keychain.delete(TOKEN_KEY).catch(() => undefined);
    const deleted: string[] = [];
    for (const id of ids) {
      // The same Mac pairing again meanwhile keeps its id: its new token is not this one's to
      // delete. (The id stays in `forget`, which the next launch drops for a saved pairing.)
      if (id === this.pairingId || this.pairings.some((p) => p.id === id)) continue;
      const clear = () => this.keychain.delete(tokenKeyOf(id)).then(() => true, () => false);
      if ((await clear()) || (await clear())) deleted.push(id);
    }
    if (settings && deleted.length > 0) await updateSettings((s) => ({ forget: s.forget.filter((id) => !deleted.includes(id)) }));
    return settings && ids.every((id) => deleted.includes(id) || this.pairings.some((p) => p.id === id) || id === this.pairingId);
  }

  /** Makes `client` Mac `pairing.id`'s link, feeding the store, and connects it if in the
   * foreground. `offer` holds Pair for the notifications step. */
  private adopt(pairing: Pairing, client: HubClient, tokens: TokenStore, offer: boolean): void {
    const link = new MacLink(this.host, pairing, client, tokens);
    const order = (l: MacLink) => this.pairings.findIndex((p) => p.id === l.id);
    this.all = [...this.all, link].sort((a, b) => order(a) - order(b));
    link.attach();
    this.emit({ status: "paired", notice: null, offer });
    if (this.foreground) client.start();
  }

  // Notifications

  /** Turns notifications on: permission, the token, each Mac's ack, and only then the setting.
   * The kinds sent are the saved ones (the defaults when none is on). The answer is the words to
   * show when no Mac took it; a Mac that did not is named in `notices`. Nothing is saved when
   * none did. A link that is coming back is waited for, briefly. */
  async turnOnNotifications(): Promise<PushResult> {
    if (this.all.length === 0) return { ok: false, message: UNREACHABLE };
    const got = await askForToken();
    if (!got.ok) return got;
    return this.inTurn(async () => {
      const saved = (await loadSettings()).pushKinds;
      return this.pushAll(got.token, anyKind(saved) ? saved : DEFAULT_PUSH_KINDS);
    });
  }

  /** Turns notifications off: each Mac is told first (a null token), then the setting is saved.
   * With no Mac in reach it stays on, since they would go on pushing; one out of reach while
   * another answered is named in `notices`, and told on its next ready. */
  async turnOffNotifications(): Promise<PushResult> {
    if (this.all.length === 0) return { ok: false, message: UNREACHABLE };
    return this.inTurn(() => this.pushAll(null, null));
  }

  /**
   * Turns one kind of push on or off. Each Mac is sent the token with all three kinds, and the
   * kinds are saved only on an ack. With notifications off, turning a kind on turns them on
   * through the permission flow, with the Mac's defaults (approvals and errors) plus this kind,
   * whatever kinds were kept from before; with them on, the token is the current one and the
   * phone's permission is never asked again. Turning the last kind off is turning notifications
   * off: a null token. The answer is the words to show when no Mac took it; the saved kinds are
   * then as they were.
   */
  async setPushKind(kind: keyof PushKinds, on: boolean): Promise<PushResult> {
    if (this.all.length === 0) return { ok: false, message: UNREACHABLE };
    return this.inTurn(async () => {
      const saved = await loadSettings();
      const next: PushKinds = { ...(saved.notifications ? saved.pushKinds : DEFAULT_PUSH_KINDS), [kind]: on };
      if (!anyKind(next)) return this.pushAll(null, next);
      const got = saved.notifications ? await currentToken() : await askForToken();
      if (!got.ok) return got;
      return this.pushAll(got.token, next);
    });
  }

  /** Runs `job` after every push job before it, whether those worked or not. */
  private inTurn<T>(job: () => Promise<T>): Promise<T> {
    const run = this.pushQueue.then(job, job);
    this.pushQueue = run.catch(() => undefined);
    return run;
  }

  /** Sends every Mac `register_push`, one after the other (each waited for briefly while it comes
   * back), and, when at least one acked, saves the setting: on when there is a token, else off;
   * and the kinds, when given. A Mac that did not ack keeps its words in `notices`; one that
   * missed a turn-off is told on its next ready. Runs inside `inTurn`. */
  private async pushAll(token: string | null, kinds: PushKinds | null): Promise<PushResult> {
    const targets = [...this.all];
    if (targets.length === 0) return { ok: false, message: UNREACHABLE };
    // Every link that is coming back is waited for at once; each is sent its turn as it comes.
    const ready = targets.map((l) => l.whenReady());
    const results: PushResult[] = [];
    for (const [i, link] of targets.entries()) results.push(await link.push((await ready[i]) ?? false, token, kinds));
    const failed = targets.flatMap((link, i) => {
      const r = results[i];
      return r && !r.ok && !link.gone ? [{ link, message: r.message }] : [];
    });
    const reached = targets.filter((link, i) => results[i]?.ok === true && !link.gone);
    if (reached.length === 0) {
      return { ok: false, message: failed.length > 0 ? failed.map((f) => this.named(f.link, f.message)).join("\n") : UNREACHABLE };
    }
    // The Macs that answered have the change; if the save fails the setting lags them, and the
    // user is told so.
    const saved = await updateSettings(() => ({ notifications: token !== null, ...(kinds ? { pushKinds: kinds } : {}) }));
    for (const link of reached) {
      link.owesOff = false;
      link.notify(null);
    }
    for (const { link, message } of failed) {
      if (token === null) link.owesOff = true;
      link.notify(message);
    }
    return saved ? { ok: true } : { ok: false, message: NOT_SAVED };
  }

  /** The phone's key of session `raw` on the Mac whose hub publishes `host` (see `keyFor`): the
   * one way from what a hub sends (a push, and in time the Island and an approve link) to a key.
   * Null for a host no pairing has, for no host with several pairings, and for a raw id that is
   * not one. A pairing with no link counts. */
  keyFor(host: string | null, raw: string): string | null {
    return keyFor(this.pairings, host, raw);
  }

  /** The key a push tap opens (see `tapKey`, which resolves through `keyFor`): null for the Inbox,
   * and for a session that Mac has ended. A pairing with no link counts: its session's screen
   * says it can't be reached. */
  tapKey(tap: Tap): string | null {
    const state = this.store.getState();
    return tapKey(tap, this.pairings, (key) => hasEnded(state, key));
  }

  /** The user dismissed the notice the Inbox shows (a Mac that unpaired this phone, an unpair
   * that did not finish) while some Mac is still paired. */
  dismissNotice(): void {
    if (this.current.notice !== null) this.emit({ notice: null });
  }

  /** The user answered the notifications step (or skipped it): Pair gives way to Sessions. */
  finishOffer(): void {
    if (this.current.offer) this.emit({ offer: false });
  }

  // Internals

  /** `message` about `link`'s Mac, with its name in front when there are several Macs. */
  private named(link: MacLink, message: string): string {
    return this.pairings.length > 1 ? `${link.pairing.macName}: ${message}` : message;
  }

  /** A `Links` over the links as they are at each call. */
  private makeLinks(): Links {
    return {
      client: (mac) => this.all.find((l) => l.id === mac)?.client ?? null,
      macs: () => this.all.map((l) => l.id),
    };
  }

  /** What `macs` shows of each pairing, in order: its link's, or, with no link, its own (the
   * store holds no link state for it, so it reads offline and not tried). */
  private describeMacs(): MacInfo[] {
    const unlinked = new Map<string, MacInfo>();
    const macs = this.pairings.map((p) => {
      const link = this.all.find((l) => l.id === p.id);
      if (link) return link.info();
      const host = hostOf(p.hubUrl) ?? "";
      const had = this.unlinked.get(p.id);
      const info: MacInfo = had && had.macName === p.macName && had.host === host ? had : { id: p.id, macName: p.macName, host };
      unlinked.set(p.id, info);
      return info;
    });
    this.unlinked = unlinked;
    return macs;
  }

  /** Rebuilds what is shown of the links, with `patch`, and tells every listener. Each part
   * keeps its identity while it is the same, so a hook that selects one part (`useMacs`,
   * `useFleetStatus`...) renders again only when that part changed. */
  private emit(patch: Partial<Pick<FleetSnapshot, "status" | "offer" | "notice">> = {}): void {
    const before = this.current;
    const macs = sameItems(before.macs, this.describeMacs());
    const notices = sameItems(
      before.notices,
      this.all.flatMap((l) => (l.pushNotice === null ? [] : [this.named(l, l.pushNotice)])),
    );
    this.current = { ...before, ...patch, macs, notices };
    for (const fn of [...this.listeners]) fn();
  }
}

/** `before` when `next` holds the same items in the same order, else `next`. */
function sameItems<T>(before: T[], next: T[]): T[] {
  return before.length === next.length && next.every((x, i) => x === before[i]) ? before : next;
}

/** `list` with `pairing` in place of the one of its id, or at the end. */
function withPairing(list: readonly Pairing[], pairing: Pairing): Pairing[] {
  return list.some((p) => p.id === pairing.id) ? list.map((p) => (p.id === pairing.id ? pairing : p)) : [...list, pairing];
}
