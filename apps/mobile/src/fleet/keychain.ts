// Where each pairing's private key is kept: its own item in the phone's keychain, as hex.
import * as SecureStore from "expo-secure-store";

import { isDevBuild } from "../build";
import { fromHex, isHex32, toHex } from "../client/link";
import { newKeyPair, type KeyPair } from "../client/noise";

/** The keychain item that held the device token while the phone had one pairing, before keys. A
 * development build keeps its own, so that it never inherits (and so can never leak) the pairing a
 * release build on the same phone made. */
export const tokenKey = (dev: boolean): string => (dev ? "octet.token.dev" : "octet.token");
/** This build's one-pairing item from before keys: only ever deleted now. */
export const TOKEN_KEY = tokenKey(isDevBuild());
/** The keychain item holding pairing `id`'s private key, the only place it is kept, under the name
 * its token had before keys: `octet.token.<id>`, or `octet.token.dev.<id>` in a development build. */
export const tokenKeyOf = (id: string, dev: boolean = isDevBuild()): string => `${tokenKey(dev)}.${id}`;

/** Where private keys are kept, by item name. */
export interface Keychain {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  delete(key: string): Promise<void>;
}

/** One pairing's private key, as hex. */
export interface KeyStore {
  load(): Promise<string | null>;
  save(hex: string): Promise<void>;
  clear(): Promise<void>;
}

/** expo-secure-store's iOS default is a migratable item: it travels in encrypted backups to
 * another handset, where "revoke this phone" would no longer mean this phone. This-device-only
 * keeps each key in this phone's keychain, readable while unlocked. Reads and deletes name the
 * same options so they reach the same item. */
const KEYCHAIN = { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY };

export const secureStore: Keychain = {
  get: (key) => SecureStore.getItemAsync(key, KEYCHAIN),
  set: (key, value) => SecureStore.setItemAsync(key, value, KEYCHAIN),
  delete: (key) => SecureStore.deleteItemAsync(key, KEYCHAIN),
};

export const keyStore = (keychain: Keychain, id: string): KeyStore => ({
  load: () => keychain.get(tokenKeyOf(id)),
  save: (hex) => keychain.set(tokenKeyOf(id), hex),
  clear: () => keychain.delete(tokenKeyOf(id)),
});

/** The key pair whose private half is `hex` as the keychain keeps it, or null for a value that is
 * not 32 bytes of lowercase hex (a token from before keys never reaches here: those are deleted). */
export function keyPairOf(hex: string): KeyPair | null {
  const privateKey = isHex32(hex) ? fromHex(hex) : null;
  return privateKey === null ? null : newKeyPair(() => privateKey);
}

/** Writes `key`'s private half to the keychain, with a second try after clearing, once. False when
 * it could not be kept. */
export async function keepKey(keys: KeyStore, key: KeyPair): Promise<boolean> {
  const hex = toHex(key.privateKey);
  try {
    await keys.save(hex);
    return true;
  } catch {
    try {
      await keys.clear();
      await keys.save(hex);
      return true;
    } catch {
      return false;
    }
  }
}
