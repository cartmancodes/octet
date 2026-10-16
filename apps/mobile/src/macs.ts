// The Macs this phone is paired with, as the rest of the app names them. Every session is keyed
// `"<mac>/<session>"`: `<mac>` is the phone's own id for the pairing (never holding a `/`), and
// `<session>` the hub's raw id, which appears only on the wire. Pure: no React, no storage.
import type { HubClient, HubState } from "./client/HubClient";
import { field } from "./model/text";

/** Pairings kept, at most: one link each, all open in the foreground. */
export const MAX_MACS = 4;
/** Why a 5th pairing is refused, before any network call. */
export const TOO_MANY_MACS = "Unpair a Mac first (4 at most)";
/** A Mac's id: `m` and 8 lowercase hex characters, made by the phone. */
export const MAC_ID = /^m[0-9a-f]{8}$/;

/** Ids made by the last resort, so that two in the same millisecond still differ. */
let made = 0;

/** A new Mac id. From a CSPRNG where there is one: `crypto.getRandomValues` (Hermes, the test
 * runtime), else the native UUID v4 that expo-modules-core installs (whose first 8 hex digits are
 * all random). Failing both, from the time, a counter and `Math.random`: a pairing id is the
 * phone's own name for a Mac, not a secret, and it only has to differ from the other three. Never
 * throws: an id is made before the Mac spends its pairing code, and at the move of an older
 * version's pairing. */
export function newMacId(): string {
  const g = globalThis as { crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array }; expo?: { uuidv4?: () => string } };
  try {
    if (typeof g.crypto?.getRandomValues === "function") {
      const bytes = g.crypto.getRandomValues(new Uint8Array(4));
      return `m${Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")}`;
    }
    const uuid = typeof g.expo?.uuidv4 === "function" ? `m${g.expo.uuidv4().toLowerCase().slice(0, 8)}` : "";
    if (MAC_ID.test(uuid)) return uuid;
  } catch {
    // A source that is there but fails is no source: the last resort below.
  }
  made = (made + 1) >>> 0;
  // The low bits of the time, the counter spread over every bit (Knuth's multiplicative hash) and
  // 32 bits of Math.random, mixed: never the same id twice in a row.
  const mixed = (Date.now() ^ Math.imul(made, 0x9e3779b1) ^ Math.floor(Math.random() * 0x1_0000_0000)) >>> 0;
  return `m${mixed.toString(16).padStart(8, "0")}`;
}

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
export function hostOf(url: string | null | undefined): string | null {
  return HOST.exec(url ?? "")?.[1] ?? null;
}

/** Whether two hosts are the same, without case or a trailing dot. */
export const sameHost = (a: string, b: string): boolean => a.replace(/\.$/, "").toLowerCase() === b.replace(/\.$/, "").toLowerCase();

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

/** The links of the clients in `clients`, by Mac id, in its order. */
export function linksOf(clients: ReadonlyMap<string, HubClient>): Links {
  return {
    client: (mac) => clients.get(mac) ?? null,
    macs: () => [...clients.keys()],
  };
}

/** The longest name a Mac is given. */
export const MAC_NAME_CHARS = 40;

/** A Mac's name as the user gave it, cleaned: control and bidi characters dropped, each run of
 * spaces one space, at most `MAC_NAME_CHARS`; null when nothing is left. */
export function cleanMacName(raw: unknown): string | null {
  const name = field(field(raw, MAC_NAME_CHARS * 4).replace(/\s+/g, " ").trim(), MAC_NAME_CHARS).trim();
  return name === "" ? null : name;
}

/** What a Mac's group header says after its name: nothing when ready; "asleep" once its first
 * attempt is over and it is not ready (a dropped link, a Mac asleep or away); "connecting…"
 * before that. */
export function macStatus(state: HubState, tried: boolean): "asleep" | "connecting…" | null {
  if (state === "ready") return null;
  return tried ? "asleep" : "connecting…";
}

/** The Inbox's header line with several Macs: "2 Macs · 1 online". */
export const fleetLine = (paired: number, online: number): string => `${paired} Macs · ${online} online`;
