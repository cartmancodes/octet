// The one hub connection of the app run: where the device token comes from, how pairing keeps it,
// and what the app's lifecycle does to it. No JSX here; `app/_layout.tsx` provides it to screens.
import * as SecureStore from "expo-secure-store";
import { createContext, useContext, useMemo, useSyncExternalStore } from "react";
import { Platform } from "react-native";

import { HubClient, Refusal, type HubClientOptions, type SocketFactory } from "./client/HubClient";
import { NOT_A_LINK, isHubUrl, pairLinkProblem, parsePairLink } from "./client/link";
import { webSocketFactory } from "./client/socket";
import { isDevBuild } from "./build";
import { DEFAULT_PUSH_KINDS, NOT_SAVED, loadSettings, readSettings, saveSettings } from "./persist";
import { createCoalescer, type Coalescer } from "./coalesce";
import { SOLO_MAC, soloLinks, type Links } from "./macs";
import { DENIED, askForToken, currentToken, dropPushRegistration, onTokenChange, projectId } from "./notifications";
import type { PushKinds } from "./protocol";
import { connectStore, octetStore, type OctetStore } from "./store";

/** The keychain item holding the device token. A development build keeps its own, so that it
 * never inherits (and so can never leak) the pairing a release build on the same phone made. */
export const tokenKey = (dev: boolean): string => (dev ? "octet.token.dev" : "octet.token");
/** The keychain item holding this build's device token: the only place it is kept. */
export const TOKEN_KEY = tokenKey(isDevBuild());

export { NOT_A_LINK };
export const UNPAIRED_NOTICE = "This phone was unpaired on the Mac";
export const UNPAIR_FAILED_NOTICE = "This phone couldn't delete its pairing. Pair again to replace it.";
/** The Mac took the pairing (its secret is spent and the name is registered) but the phone could
 * not keep the token: only revoking on the Mac frees the name. */
const unsaved = (name: string): string =>
  `This phone paired but couldn't save it. On the Mac run \`octet hub revoke ${name}\`, then pair again with a new code.`;

/** `loading` until the saved token is read; `paired` has a client; `unpaired` shows Pair. */
export type HubStatus = "loading" | "unpaired" | "paired";

export interface HubSnapshot {
  status: HubStatus;
  /** Why the user is on Pair (the Mac unpaired this phone); null for a plain first pairing. */
  notice: string | null;
  /** The connection, while paired. */
  client: HubClient | null;
  /** Just paired, and the notifications step is up: Pair stays until `finishOffer`. */
  offer: boolean;
  /** Why the Mac did not take this phone's push registration (or why the phone could not make
   * one), for Settings to show; null when the last registration worked or none was tried. */
  pushNotice: string | null;
}

/** Whether any kind of push is wanted. */
const anyKind = (kinds: PushKinds): boolean => kinds.approvals || kinds.errors || kinds.turns;

/** The outcome of turning notifications on or off: done, or the words for why not. */
export type PushResult = { ok: true } | { ok: false; message: string };

const UNREACHABLE = "Can't reach your Mac";
/** How long turning notifications on or off waits for a connection that is coming back. */
const READY_WAIT_MS = 8_000;
/** How long unpair leaves the connection open for the Mac to hear "notifications off". */
const GOODBYE_MS = 2_000;

/** A failed pairing: `clear` says the link is no good any more (the Mac says its code is spent
 * or expired, or took the secret), so the screen forgets it; any other link is kept, to edit or
 * to try again. */
export type PairResult = { ok: true } | { ok: false; message: string; clear: boolean };

export interface TokenStore {
  load(): Promise<string | null>;
  save(token: string): Promise<void>;
  clear(): Promise<void>;
}

/** expo-secure-store's iOS default is a migratable item: it travels in encrypted backups to
 * another handset, where "revoke this phone" would no longer mean this phone. This-device-only
 * keeps the token in this phone's keychain, readable while unlocked. Reads and deletes name the
 * same options so they reach the same item. */
const KEYCHAIN = { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY };

const keychain: TokenStore = {
  load: () => SecureStore.getItemAsync(TOKEN_KEY, KEYCHAIN),
  save: (token) => SecureStore.setItemAsync(TOKEN_KEY, token, KEYCHAIN),
  clear: () => SecureStore.deleteItemAsync(TOKEN_KEY, KEYCHAIN),
};

export interface HubControllerOptions {
  factory?: SocketFactory;
  store?: OctetStore;
  tokens?: TokenStore;
  /** "ios" or "android"; sent to the Mac at pairing and names the default device. */
  platform?: string;
  /** Clock and timer overrides for the clients it makes (tests). */
  clientOptions?: Partial<Pick<HubClientOptions, "now" | "schedule" | "random">>;
}

export class HubController {
  private readonly factory: SocketFactory;
  private readonly store: OctetStore;
  private readonly tokens: TokenStore;
  private readonly platform: string;
  private readonly clientOptions: NonNullable<HubControllerOptions["clientOptions"]>;
  private readonly schedule: (fn: () => void, ms: number) => () => void;
  /** One store commit per 50 ms, whichever Mac's client the batches come from. */
  private readonly coalescer: Coalescer;

  private current: HubSnapshot = { status: "loading", notice: null, client: null, offer: false, pushNotice: null };
  private readonly listeners = new Set<() => void>();
  private foreground = false;
  private savedName: string | null = null;
  /** The client of a pairing in flight, which is not the app's client until it succeeds. */
  private pairing: HubClient | null = null;
  private detach: (() => void) | null = null;
  private starting: Promise<void> | null = null;
  private unpairing = false;
  /** Push registrations wait their turn, so that the Mac ends with the last one asked for. */
  private pushQueue: Promise<unknown> = Promise.resolve();

  constructor(opts: HubControllerOptions = {}) {
    this.factory = opts.factory ?? webSocketFactory;
    this.store = opts.store ?? octetStore;
    this.tokens = opts.tokens ?? keychain;
    this.platform = opts.platform ?? Platform.OS;
    this.clientOptions = opts.clientOptions ?? {};
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
  }

  // What screens read

  snapshot = (): HubSnapshot => this.current;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  /** "Android phone" on Android, "iPhone" otherwise. */
  defaultName(): string {
    return this.platform === "android" ? "Android phone" : "iPhone";
  }

  /** The name this phone paired under last, else the platform's. */
  initialName(): string {
    return this.savedName ?? this.defaultName();
  }

  // Lifecycle

  /** Reads the keychain and settings, once however often it is called: a token and a hub address make a client. A keychain
   * that cannot be read is no token: the user pairs again. */
  init(): Promise<void> {
    this.starting ??= this.load();
    return this.starting;
  }

  private async load(): Promise<void> {
    const [token, read] = await Promise.all([this.tokens.load().catch(() => null), readSettings()]);
    if (!read.ok) {
      // Settings that could not be read say nothing of the hub address: the token stays for
      // the next launch, and this one shows Pair.
      this.set({ status: "unpaired" });
      return;
    }
    const settings = read.settings;
    this.savedName = settings.deviceName;
    if (token === null || token === "") {
      this.set({ status: "unpaired" });
    } else if (settings.hubUrl === null || !isHubUrl(settings.hubUrl)) {
      // A token with no usable address (killed between the two saves, or a tampered value) can
      // reach nowhere it should: drop it and pair again.
      await this.tokens.clear().catch(() => undefined);
      this.set({ status: "unpaired" });
    } else {
      this.adopt(new HubClient({ ...this.clientOptions, url: settings.hubUrl, token, factory: this.factory }));
    }
  }

  /** The app came to the foreground (connect, or skip a backoff wait: the network may well be
   * back) or went to the background (close the socket and leave no timer, pings included).
   * `inactive` is not the background: a Face ID prompt passes through it. */
  setForeground(active: boolean): void {
    this.foreground = active;
    if (active) this.current.client?.retry(); // a start when stopped; leaves a live or opening link alone
    else {
      this.pairing?.stop();
      this.current.client?.stop();
    }
  }

  /** Pairs with the Mac the link names. On success the token is in the keychain, the hub address
   * in settings, and the connection is up. Failures come back as the words to show. */
  async pair(link: string, name: string): Promise<PairResult> {
    if (this.pairing) return { ok: false, message: "Already pairing", clear: false };
    if (this.unpairing) return { ok: false, message: "Finishing unpairing. Try again in a moment.", clear: false };
    const parsed = parsePairLink(link);
    if (!parsed) return { ok: false, message: pairLinkProblem(link) ?? NOT_A_LINK, clear: false };
    const deviceName = name.trim() === "" ? this.defaultName() : name.trim();
    const client = new HubClient({ ...this.clientOptions, url: parsed.url, token: null, factory: this.factory });
    this.pairing = client;
    if (this.current.notice !== null) this.set({ notice: null });
    try {
      const { token } = await client.pair(parsed.secret, deviceName, this.platform);
      if (!(await this.keep(token, parsed.url, deviceName))) {
        await this.tokens.clear().catch(() => undefined);
        return { ok: false, message: unsaved(deviceName), clear: true };
      }
      this.savedName = deviceName;
      this.adopt(client, projectId() !== null);
      return { ok: true };
    } catch (error) {
      if (!(error instanceof Refusal)) return { ok: false, message: "Pairing failed", clear: false };
      // Only a spent or expired code is refused as `unpaired`; on any other refusal the Mac keeps
      // the code, and the link is good to try again (another device name, a moment later).
      return { ok: false, message: error.message, clear: error.code === "unpaired" };
    } finally {
      this.pairing = null;
    }
  }

  /** Writes the token to the keychain (a second try after clearing, once) and the address to
   * settings. False when either could not be kept. */
  private async keep(token: string, url: string, deviceName: string): Promise<boolean> {
    try {
      await this.tokens.save(token);
    } catch {
      try {
        await this.tokens.clear();
        await this.tokens.save(token);
      } catch {
        return false;
      }
    }
    return saveSettings({ hubUrl: url, deviceName });
  }

  /** The user unpairs this phone: the connection and what the Mac sent are dropped and Pair shows
   * at once, with no notice; then the hub address and the token are deleted (the keychain twice).
   * If either will not go, Pair shows `UNPAIR_FAILED_NOTICE`; the next start drops the token
   * anyway, because with no hub address it can reach nowhere. The device name stays to prefill
   * the next pairing. The Mac keeps listing the phone until `octet hub revoke` there. Pairing
   * waits for the writes, so they cannot delete a new token. */
  async unpair(): Promise<void> {
    const client = this.current.client;
    if (this.unpairing || client === null) return;
    this.unpairing = true;
    try {
      // Best effort, sent before the connection is released and never waited on: the Mac stops
      // pushing to this phone. Revoking on the Mac ends it there anyway.
      this.stopAfterGoodbye(client);
      this.release();
      // Before any await: a foreground event now finds no client to start.
      this.set({ status: "unpaired", notice: null, client: null, offer: false, pushNotice: null });
      const address = await saveSettings({ hubUrl: null, notifications: false, pushKinds: DEFAULT_PUSH_KINDS });
      let token = await this.tokens.clear().then(() => true, () => false);
      if (!token) token = await this.tokens.clear().then(() => true, () => false);
      if (!address || !token) this.set({ notice: UNPAIR_FAILED_NOTICE });
    } finally {
      this.unpairing = false;
    }
  }

  /** Stops `client`, but first, on a ready link, tells the Mac "notifications off" and keeps the
   * link until the Mac answers or refuses, or `GOODBYE_MS` pass. Nothing waits on it. If the Mac
   * did not ack it, the phone drops its push registration instead, so that the Mac's pushes
   * bounce and it forgets the token. */
  private stopAfterGoodbye(client: HubClient): void {
    if (client.state !== "ready") {
      client.stop();
      void dropPushRegistration();
      return;
    }
    let finished = false;
    let cancel = () => {};
    const finish = (heard: boolean) => {
      if (finished) return;
      finished = true;
      cancel();
      client.stop();
      if (!heard) void dropPushRegistration();
    };
    cancel = this.schedule(() => finish(false), GOODBYE_MS);
    client.request({ type: "register_push", token: null }).then(
      () => finish(true),
      () => finish(false),
    );
  }

  // Notifications

  /** Turns notifications on: permission, the token, the Mac's ack, and only then the setting.
   * The kinds sent are the saved ones (the defaults when none is on). The answer is the words to
   * show; nothing is saved on a failure. A link that is coming back is waited for, briefly. */
  async turnOnNotifications(): Promise<PushResult> {
    const client = this.current.client;
    if (client === null) return { ok: false, message: UNREACHABLE };
    const got = await askForToken();
    if (!got.ok) return got;
    return this.inTurn(async () => {
      const saved = (await loadSettings()).pushKinds;
      return this.push(client, got.token, anyKind(saved) ? saved : DEFAULT_PUSH_KINDS);
    });
  }

  /** Turns notifications off: the Mac is told first (a null token), then the setting is saved.
   * With the Mac out of reach it stays on, since the Mac would go on pushing. */
  async turnOffNotifications(): Promise<PushResult> {
    const client = this.current.client;
    if (client === null) return { ok: false, message: UNREACHABLE };
    return this.inTurn(() => this.push(client, null, null));
  }

  /**
   * Turns one kind of push on or off. The Mac is sent the token with all three kinds, and the
   * kinds are saved only on its ack. With notifications off, turning a kind on turns them on
   * through the permission flow, with the Mac's defaults (approvals and errors) plus this kind,
   * whatever kinds were kept from before; with them on, the token is the current one and the phone's permission is never asked again.
   * Turning the last kind off is turning notifications off: a null token. The answer is the
   * words to show when it did not work; the saved kinds are then as they were.
   */
  async setPushKind(kind: keyof PushKinds, on: boolean): Promise<PushResult> {
    const client = this.current.client;
    if (client === null) return { ok: false, message: UNREACHABLE };
    return this.inTurn(async () => {
      const saved = await loadSettings();
      const next: PushKinds = { ...(saved.notifications ? saved.pushKinds : DEFAULT_PUSH_KINDS), [kind]: on };
      if (!anyKind(next)) return this.push(client, null, next);
      const got = saved.notifications ? await currentToken() : await askForToken();
      if (!got.ok) return got;
      return this.push(client, got.token, next);
    });
  }

  /** Runs `job` after every push job before it, whether those worked or not. */
  private inTurn<T>(job: () => Promise<T>): Promise<T> {
    const run = this.pushQueue.then(job, job);
    this.pushQueue = run.catch(() => undefined);
    return run;
  }

  /** Sends the Mac `register_push` (with `kinds`, when given and there is a token) and, on its
   * ack, saves the setting: on when there is a token, else off; and the kinds, when given. Runs
   * inside `inTurn`. */
  private async push(client: HubClient, token: string | null, kinds: PushKinds | null): Promise<PushResult> {
    if (!(await this.whenReady(client)) || client !== this.current.client) return { ok: false, message: UNREACHABLE };
    try {
      // "No token" says everything: kinds are saved here, but not sent with it.
      await client.request(token !== null && kinds ? { type: "register_push", token, kinds } : { type: "register_push", token });
    } catch (error) {
      return { ok: false, message: wordsOf(error) };
    }
    // An ack that comes after the unpair is for a pairing that is gone: it saves nothing.
    if (client !== this.current.client) return { ok: false, message: UNREACHABLE };
    // The Mac has the change; if the save fails the setting lags it, and the user is told so.
    const saved = await saveSettings({ notifications: token !== null, ...(kinds ? { pushKinds: kinds } : {}) });
    if (!saved) return { ok: false, message: NOT_SAVED };
    this.notify(null);
    return { ok: true };
  }

  /** Resolves true when `client` is ready, now or within `READY_WAIT_MS`; false when it is not. */
  private whenReady(client: HubClient): Promise<boolean> {
    if (client.state === "ready") return Promise.resolve(true);
    return new Promise<boolean>((resolve) => {
      let off = () => {};
      let cancel = () => {};
      const done = (ready: boolean) => {
        off();
        cancel();
        resolve(ready);
      };
      off = client.onState((state) => {
        if (state === "ready") done(true);
        else if (state === "unpaired") done(false);
      });
      cancel = this.schedule(() => done(false), READY_WAIT_MS);
    });
  }

  /** The user answered the notifications step (or skipped it): Pair gives way to Sessions. */
  finishOffer(): void {
    if (this.current.offer) this.set({ offer: false });
  }

  /** Registers the current push token with the Mac, if the user turned notifications on. Runs
   * after each ready and on a token rotation. A hub refusal shows on Settings; a link that
   * dropped is no news, the next ready tries again. With the permission taken away in the phone's
   * settings, the Mac is told "no token" (it would push to a phone that shows nothing) and the
   * setting stays on, so a permission given back registers again on the next ready. */
  private syncPush(client: HubClient): Promise<void> {
    return this.inTurn(async () => {
      if (!(await loadSettings()).notifications) return;
      const got = await currentToken();
      // The user may have turned it off while the token was coming; the kinds are read again
      // too, since they may have changed.
      const settings = await loadSettings();
      if (!settings.notifications) return;
      if (client !== this.current.client || client.state !== "ready") return;
      if (!got.ok) {
        if (got.message === DENIED) {
          await client.request({ type: "register_push", token: null }).catch(() => undefined);
          if (client !== this.current.client) return;
        }
        this.notify(got.message);
        return;
      }
      try {
        // The phone's kinds are the truth: a Mac that still has the ones from before kinds
        // existed (all three on) is brought to them.
        await client.request({ type: "register_push", token: got.token, kinds: settings.pushKinds });
        if (client === this.current.client) this.notify(null);
      } catch (error) {
        if (client !== this.current.client) return;
        if (error instanceof Refusal && error.code !== "unsent" && error.code !== "lost") this.notify(error.message);
      }
    });
  }

  private notify(pushNotice: string | null): void {
    if (this.current.pushNotice !== pushNotice) this.set({ pushNotice });
  }

  // Internals

  private set(patch: Partial<HubSnapshot>): void {
    this.current = { ...this.current, ...patch };
    for (const fn of [...this.listeners]) fn();
  }

  /** Makes `client` the app's connection, feeding the store, and connects it if in the foreground.
   * `offer` holds Pair for the notifications step. */
  private adopt(client: HubClient, offer = false): void {
    this.release();
    this.current.client?.stop();
    const offStore = connectStore(this.store, client, { mac: SOLO_MAC, coalescer: this.coalescer });
    const offState = client.onState((state) => {
      if (state === "unpaired") void this.unpaired(client);
      else if (state === "ready") void this.syncPush(client);
    });
    // A rotated token while the link is down is sent by the next ready.
    const offToken = onTokenChange(() => {
      if (client.state === "ready") void this.syncPush(client);
    });
    this.detach = () => {
      offStore();
      offState();
      offToken();
    };
    this.set({ status: "paired", notice: null, client, offer, pushNotice: null });
    if (this.foreground) client.start();
  }

  /** Drops the connection and what the old Mac sent. */
  private release(): void {
    this.detach?.();
    this.detach = null;
    this.store.getState().reset();
  }

  /** The Mac refused this phone's token as unpaired or revoked: the token is deleted and the
   * user is sent to Pair. Only the hub's refusal gets here; a dropped network never does. */
  private async unpaired(client: HubClient): Promise<void> {
    if (client !== this.current.client) return;
    this.release();
    try {
      await this.tokens.clear();
    } catch {
      // The keychain keeps it; the next start is refused again and comes back here.
    }
    // The Mac forgot this phone's token with the phone: a new pairing starts with notifications
    // off, and its notifications step asks again.
    await saveSettings({ notifications: false, pushKinds: DEFAULT_PUSH_KINDS });
    if (client !== this.current.client) return;
    this.set({ status: "unpaired", notice: UNPAIRED_NOTICE, client: null, offer: false, pushNotice: null });
  }
}

/** The words for a failed command: the hub's own, else a plain one. */
const wordsOf = (error: unknown): string => (error instanceof Refusal ? error.message : "Something went wrong. Try again.");

/** The app's connection. */
export const hub = new HubController();

export const HubContext = createContext<HubController>(hub);

export const useHubController = (): HubController => useContext(HubContext);

export function useHubSnapshot(): HubSnapshot {
  const controller = useHubController();
  return useSyncExternalStore(controller.subscribe, controller.snapshot, controller.snapshot);
}

/** The connection while paired; null before pairing and after the Mac unpaired this phone. */
export const useHubClient = (): HubClient | null => useHubSnapshot().client;

/** The connection by Mac, for code that holds a session key: the one Mac, `SOLO_MAC`, while
 * paired. A new object when the connection changes, the same one until then. */
export function useLinks(): Links {
  const client = useHubClient();
  return useMemo(() => soloLinks(client), [client]);
}
