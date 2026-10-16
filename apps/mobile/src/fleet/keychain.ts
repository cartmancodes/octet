// Where each pairing's device token is kept: its own item in the phone's keychain.
import * as SecureStore from "expo-secure-store";

import { isDevBuild } from "../build";

/** The keychain item that held the device token while the phone had one pairing. A development
 * build keeps its own, so that it never inherits (and so can never leak) the pairing a release
 * build on the same phone made. */
export const tokenKey = (dev: boolean): string => (dev ? "octet.token.dev" : "octet.token");
/** This build's one-pairing keychain item: read only to move its token to pairing 1's. */
export const TOKEN_KEY = tokenKey(isDevBuild());
/** The keychain item holding pairing `id`'s device token, the only place it is kept:
 * `octet.token.<id>`, or `octet.token.dev.<id>` in a development build. */
export const tokenKeyOf = (id: string, dev: boolean = isDevBuild()): string => `${tokenKey(dev)}.${id}`;

/** Where device tokens are kept, by item name. */
export interface Keychain {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  delete(key: string): Promise<void>;
}

/** One pairing's token. */
export interface TokenStore {
  load(): Promise<string | null>;
  save(token: string): Promise<void>;
  clear(): Promise<void>;
}

/** expo-secure-store's iOS default is a migratable item: it travels in encrypted backups to
 * another handset, where "revoke this phone" would no longer mean this phone. This-device-only
 * keeps each token in this phone's keychain, readable while unlocked. Reads and deletes name the
 * same options so they reach the same item. */
const KEYCHAIN = { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY };

export const secureStore: Keychain = {
  get: (key) => SecureStore.getItemAsync(key, KEYCHAIN),
  set: (key, value) => SecureStore.setItemAsync(key, value, KEYCHAIN),
  delete: (key) => SecureStore.deleteItemAsync(key, KEYCHAIN),
};

export const tokenStore = (keychain: Keychain, id: string): TokenStore => ({
  load: () => keychain.get(tokenKeyOf(id)),
  save: (token) => keychain.set(tokenKeyOf(id), token),
  clear: () => keychain.delete(tokenKeyOf(id)),
});

/** Writes `token` to the keychain, with a second try after clearing, once. False when it could
 * not be kept. */
export async function keepToken(tokens: TokenStore, token: string): Promise<boolean> {
  try {
    await tokens.save(token);
    return true;
  } catch {
    try {
      await tokens.clear();
      await tokens.save(token);
      return true;
    } catch {
      return false;
    }
  }
}
