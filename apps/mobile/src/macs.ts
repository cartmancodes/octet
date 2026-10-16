// The Macs this phone is paired with, as the rest of the app names them. Every session is keyed
// `"<mac>/<session>"`: `<mac>` is the phone's own id for the pairing (never holding a `/`), and
// `<session>` the hub's raw id, which appears only on the wire. Pure: no React, no storage.
import type { HubClient } from "./client/HubClient";

/** The one Mac's id until pairings give each its own (Phase D, Task 2). Nothing persisted is
 * keyed by it. */
export const SOLO_MAC = "m0";

/** The store's key of `raw` on Mac `mac`. */
export const keyOf = (mac: string, raw: string): string => `${mac}/${raw}`;

/** The Mac and raw id of a key, split on its first `/` (a Mac id never holds one; a raw id may);
 * null for a string with no Mac or no raw id. */
export function splitKey(key: string): { mac: string; raw: string } | null {
  const at = key.indexOf("/");
  if (at <= 0 || at === key.length - 1) return null;
  return { mac: key.slice(0, at), raw: key.slice(at + 1) };
}

/** The hub's own id of a key; null for a string that is not one. */
export const rawOf = (key: string): string | null => splitKey(key)?.raw ?? null;

/** The Mac of a key; null for a string that is not one. */
export const macOfKey = (key: string): string | null => splitKey(key)?.mac ?? null;

/** One pairing, as the phone keeps it. */
export interface Pairing {
  id: string;
  hubUrl: string;
  deviceName: string;
  macName: string;
}

const HOST = /^[a-z][a-z0-9+.-]*:\/\/(\[[^\]/]*\]|[^/:?#[\]]+)/i;

/** The host of a hub address, as written ("[::1]" keeps its brackets); null when it has none. */
function hostOf(url: string | null | undefined): string | null {
  return HOST.exec(url ?? "")?.[1] ?? null;
}

const sameHost = (a: string, b: string): boolean => a.replace(/\.$/, "").toLowerCase() === b.replace(/\.$/, "").toLowerCase();

/** The pairing whose hub host is `host`, compared without case; null when none is. */
export function macOfHost(pairings: readonly Pick<Pairing, "id" | "hubUrl">[], host: string): string | null {
  if (host === "") return null;
  for (const p of pairings) {
    const own = hostOf(p.hubUrl);
    if (own !== null && sameHost(own, host)) return p.id;
  }
  return null;
}

const SOME_MAC = "Mac";
const LABEL_MAX = 63;

/** The Mac's name: the first label of the hub's host ("studio" from "wss://studio.tail1.ts.net/").
 * An IP address is kept whole. "Mac" without an address. */
export function macNameOf(hubUrl: string | null | undefined): string {
  const host = hostOf(hubUrl);
  if (!host) return SOME_MAC;
  if (host.startsWith("[") || /^[0-9.]+$/.test(host)) return host;
  const first = host.split(".")[0] ?? "";
  return first === "" ? SOME_MAC : first.slice(0, LABEL_MAX);
}

/** Where the connection to each Mac is: what code that holds a key asks for its Mac's client. */
export interface Links {
  /** The connection to `mac`; null for a Mac not paired, or not connected yet. */
  client(mac: string): HubClient | null;
  /** The Macs there are connections to. */
  macs(): string[];
}

/** The links of a phone with at most one Mac, `SOLO_MAC`. */
export function soloLinks(client: HubClient | null): Links {
  return {
    client: (mac) => (client !== null && mac === SOLO_MAC ? client : null),
    macs: () => (client !== null ? [SOLO_MAC] : []),
  };
}
