// The one hub connection of the app run: where the device token comes from, how pairing keeps it,
// and what the app's lifecycle does to it. No JSX here; `app/_layout.tsx` provides it to screens.
import * as SecureStore from "expo-secure-store";
import { createContext, useContext, useSyncExternalStore } from "react";
import { Platform } from "react-native";

import { HubClient, Refusal, type HubClientOptions, type SocketFactory } from "./client/HubClient";
import { isHubUrl, parsePairLink } from "./client/link";
import { webSocketFactory } from "./client/socket";
import { loadSettings, saveSettings } from "./persist";
import { connectStore, octetStore, type OctetStore } from "./store";

/** The keychain item holding the device token: the only place it is kept. */
export const TOKEN_KEY = "octet.token";

export const NOT_A_LINK = "That isn't an Octet pairing link";
export const UNPAIRED_NOTICE = "This phone was unpaired on the Mac";
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
}

/** A failed pairing: `clear` says the link is no good any more (the Mac answered, or took the
 * secret), so the screen forgets it; a link that is invalid or never reached the Mac is kept for editing. */
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

  private current: HubSnapshot = { status: "loading", notice: null, client: null };
  private readonly listeners = new Set<() => void>();
  private foreground = false;
  private savedName: string | null = null;
  /** The client of a pairing in flight, which is not the app's client until it succeeds. */
  private pairing: HubClient | null = null;
  private detach: (() => void) | null = null;
  private starting: Promise<void> | null = null;
  private unpairing = false;

  constructor(opts: HubControllerOptions = {}) {
    this.factory = opts.factory ?? webSocketFactory;
    this.store = opts.store ?? octetStore;
    this.tokens = opts.tokens ?? keychain;
    this.platform = opts.platform ?? Platform.OS;
    this.clientOptions = opts.clientOptions ?? {};
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
    const [token, settings] = await Promise.all([this.tokens.load().catch(() => null), loadSettings()]);
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

  /** The app came to the foreground (connect) or went to the background (close the socket and
   * leave no timer). `inactive` is not the background: a Face ID prompt passes through it. */
  setForeground(active: boolean): void {
    this.foreground = active;
    if (active) this.current.client?.start();
    else {
      this.pairing?.stop();
      this.current.client?.stop();
    }
  }

  /** Pairs with the Mac the link names. On success the token is in the keychain, the hub address
   * in settings, and the connection is up. Failures come back as the words to show. */
  async pair(link: string, name: string): Promise<PairResult> {
    if (this.pairing) return { ok: false, message: "Already pairing", clear: false };
    const parsed = parsePairLink(link);
    if (!parsed) return { ok: false, message: NOT_A_LINK, clear: false };
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
      this.adopt(client);
      return { ok: true };
    } catch (error) {
      if (!(error instanceof Refusal)) return { ok: false, message: "Pairing failed", clear: false };
      return { ok: false, message: error.message, clear: error.code !== "unsent" && error.code !== "lost" };
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

  /** The user unpairs this phone: the connection and what the Mac sent are dropped, then the
   * hub address and the token are deleted, and Pair shows with no notice. The device name stays
   * to prefill the next pairing. The Mac keeps listing the phone until `octet hub revoke` there.
   * A keychain that will not clear still ends at Pair: with no hub address the next start drops
   * the token. */
  async unpair(): Promise<void> {
    const client = this.current.client;
    if (this.unpairing || client === null) return;
    this.unpairing = true;
    try {
      this.release();
      client.stop();
      await saveSettings({ hubUrl: null });
      await this.tokens.clear().catch(() => undefined);
      this.set({ status: "unpaired", notice: null, client: null });
    } finally {
      this.unpairing = false;
    }
  }

  // Internals

  private set(patch: Partial<HubSnapshot>): void {
    this.current = { ...this.current, ...patch };
    for (const fn of [...this.listeners]) fn();
  }

  /** Makes `client` the app's connection, feeding the store, and connects it if in the foreground. */
  private adopt(client: HubClient): void {
    this.release();
    this.current.client?.stop();
    const offStore = connectStore(this.store, client);
    const offState = client.onState((state) => {
      if (state === "unpaired") void this.unpaired(client);
    });
    this.detach = () => {
      offStore();
      offState();
    };
    this.set({ status: "paired", notice: null, client });
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
    if (client !== this.current.client) return;
    this.set({ status: "unpaired", notice: UNPAIRED_NOTICE, client: null });
  }
}

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
