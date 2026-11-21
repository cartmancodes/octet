// The only things the phone keeps on disk: non-secret settings. Each pairing's private key is in
// expo-secure-store (src/fleet/keychain.ts); cursors are in memory and transcripts are never
// saved, so a cold start subscribes fresh.
import AsyncStorage from "@react-native-async-storage/async-storage";

import { isHex32 } from "./client/link";
import { MAC_ID, MAX_MACS, UNNAMED_MAC, cleanMacName, type Pairing } from "./macs";
import type { PushKinds } from "./protocol";

export interface Settings {
  /** The name this phone pairs under; null until chosen. */
  deviceName: string | null;
  /** Allow asks for Face ID or the fingerprint. On unless the user turned it off. */
  faceId: boolean;
  /** Read, never saved: these settings were saved by a version from before keys, which paired
   * by a hub's address and a token (they carry `hubUrl`). Its tokens are deleted at this launch,
   * once, and the user pairs again. */
  beforeKeys: boolean;
  /** The Macs this phone is paired with, in the order paired: at most `MAX_MACS`, each id and each
   * channel once. */
  pairings: Pairing[];
  /** Ids of pairings that went whose keychain entries could not be deleted yet, and of pairings
   * from before keys: the next launch deletes their entries. */
  forget: string[];
  /** Push notifications are on: every Mac has this phone's token. One choice for every Mac. Off
   * unless the user turned it on. */
  notifications: boolean;
  /** What every Mac pushes this phone while notifications are on. Missing is the Mac's own default
   * for a new pairing (approvals and errors on, finished turns off), except in settings saved
   * before kinds existed with notifications on: the Mac was pushing all three, so all three. */
  pushKinds: PushKinds;
  /** The transcript's text size in points, which `Aa` cycles. */
  textSize: TextSize;
  /** The Live Activity (the Dynamic Island and the Lock Screen) is on, where the phone has it. On
   * unless the user turned it off. */
  liveActivity: boolean;
  /** The Mac that drives the Live Activity while the phone is away (one Mac at a time: the one of
   * the lead session when the app was last in front); null for none chosen yet. */
  liveActivityMac: string | null;
}

export const TEXT_SIZES = [12, 13, 14] as const;
export type TextSize = (typeof TEXT_SIZES)[number];

/** What the hub sends a newly paired device, which is what an app with nothing saved shows. */
export const DEFAULT_PUSH_KINDS: PushKinds = { approvals: true, errors: true, turns: false };
/** What a Mac from before kinds existed pushed: everything. */
const ALL_PUSH_KINDS: PushKinds = { approvals: true, errors: true, turns: true };

export const DEFAULT_SETTINGS: Settings = {
  deviceName: null,
  faceId: true,
  beforeKeys: false,
  pairings: [],
  forget: [],
  notifications: false,
  pushKinds: DEFAULT_PUSH_KINDS,
  textSize: 13,
  liveActivity: true,
  liveActivityMac: null,
};
export const SETTINGS_KEY = "octet.settings.v1";
/** What to tell the user when a settings write failed. */
export const NOT_SAVED = "Couldn't save that setting";

/** The longest string kept: a longer one is not a name or a URL. */
const MAX_CHARS = 2_048;

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const text = (x: unknown): string | null => (typeof x === "string" && x.length <= MAX_CHARS ? x : null);
/** Each kind that is missing or not a boolean is the default's. */
function pushKinds(x: unknown): PushKinds {
  if (!isObj(x)) return DEFAULT_PUSH_KINDS;
  const kind = (k: keyof PushKinds): boolean => {
    const v = x[k];
    return typeof v === "boolean" ? v : DEFAULT_PUSH_KINDS[k];
  };
  return { approvals: kind("approvals"), errors: kind("errors"), turns: kind("turns") };
}
/** The saved pairings that are whole: an id of the phone's making, a relay address, a channel id
 * and a hub key (64 lowercase hex digits each) and a device name; a missing or blank Mac name is
 * "Mac". The first of an id or a channel is kept; at most `MAX_MACS`. `dropped` is the ids of the
 * rest that are not kept ids (a pairing from before keys, which named its hub by `hubUrl`, among
 * them): their keychain entries are to be deleted. The relay is checked where it is dialled. */
function pairings(x: unknown): { kept: Pairing[]; dropped: string[] } {
  if (!Array.isArray(x)) return { kept: [], dropped: [] };
  const kept: Pairing[] = [];
  const seen: string[] = [];
  for (const item of x as unknown[]) {
    if (!isObj(item)) continue;
    const id = typeof item.id === "string" && MAC_ID.test(item.id) ? item.id : null;
    if (id === null) continue;
    seen.push(id);
    const relay = text(item.relay);
    const deviceName = text(item.deviceName);
    const { channel, hubKey } = item;
    if (kept.length >= MAX_MACS || !relay || deviceName === null) continue;
    if (typeof channel !== "string" || !isHex32(channel) || typeof hubKey !== "string" || !isHex32(hubKey)) continue;
    if (kept.some((p) => p.id === id || p.channel === channel)) continue;
    kept.push({ id, relay, channel, hubKey, deviceName, macName: cleanMacName(item.macName) ?? UNNAMED_MAC });
  }
  const ids = new Set(kept.map((p) => p.id));
  return { kept, dropped: [...new Set(seen.filter((id) => !ids.has(id)))] };
}
/** The ids still to forget: Mac ids only, each once, at most a few. */
const FORGET_MAX = 16;
function forget(x: unknown, dropped: readonly string[] = []): string[] {
  const saved = Array.isArray(x) ? (x as unknown[]) : [];
  const ids = [...saved, ...dropped].filter((id): id is string => typeof id === "string" && MAC_ID.test(id));
  return [...new Set(ids)].slice(-FORGET_MAX);
}
const textSize = (x: unknown): TextSize => (TEXT_SIZES as readonly unknown[]).includes(x) ? (x as TextSize) : DEFAULT_SETTINGS.textSize;

/** The saved settings; each field that is missing or not the right type is its default. None
 * saved, or garbage saved, is the defaults: a save then writes over it. `ok: false` only when
 * storage could not be read at all: what it holds is unknown, which is not the same as nothing. */
export async function readSettings(): Promise<{ ok: true; settings: Settings } | { ok: false }> {
  let raw: string | null;
  try {
    raw = await AsyncStorage.getItem(SETTINGS_KEY);
  } catch {
    return { ok: false };
  }
  if (raw === null) return { ok: true, settings: DEFAULT_SETTINGS };
  try {
    const value: unknown = JSON.parse(raw);
    if (!isObj(value)) return { ok: true, settings: DEFAULT_SETTINGS };
    const notifications =
      typeof value.notifications === "boolean" ? value.notifications : DEFAULT_SETTINGS.notifications;
    // Saved before kinds existed, with notifications on: keep what the Mac was pushing, rather
    // than turn finished turns off behind the user's back on the next register.
    const upgraded = notifications && !("pushKinds" in value);
    const saved = pairings(value.pairings);
    return {
      ok: true,
      settings: {
        deviceName: text(value.deviceName),
        faceId: typeof value.faceId === "boolean" ? value.faceId : DEFAULT_SETTINGS.faceId,
        beforeKeys: "hubUrl" in value,
        pairings: saved.kept,
        forget: forget(value.forget, saved.dropped),
        notifications,
        pushKinds: upgraded ? ALL_PUSH_KINDS : pushKinds(value.pushKinds),
        textSize: textSize(value.textSize),
        liveActivity: typeof value.liveActivity === "boolean" ? value.liveActivity : DEFAULT_SETTINGS.liveActivity,
        liveActivityMac: typeof value.liveActivityMac === "string" && MAC_ID.test(value.liveActivityMac) ? value.liveActivityMac : null,
      },
    };
  } catch {
    return { ok: true, settings: DEFAULT_SETTINGS };
  }
}

/** The saved settings, or the defaults when they cannot be read. */
export async function loadSettings(): Promise<Settings> {
  const read = await readSettings();
  return read.ok ? read.settings : DEFAULT_SETTINGS;
}

/** Saves wait their turn: a patch reads what the one before wrote. */
let saving: Promise<boolean> = Promise.resolve(true);

/** Saves `patch` over what is saved. A failed write is dropped (settings are a convenience) and
 * reported as false, for the few callers that cannot go on without it. */
export function saveSettings(patch: Partial<Settings>): Promise<boolean> {
  return updateSettings(() => patch);
}

/** `saveSettings` with the patch made from what is saved, in the same turn: for a change to a
 * list (the pairings) that must not lose another change made just before it. */
export function updateSettings(change: (saved: Settings) => Partial<Settings>): Promise<boolean> {
  saving = saving.then(() => write(change));
  return saving;
}

async function write(change: (saved: Settings) => Partial<Settings>): Promise<boolean> {
  try {
    // A read that failed would write the defaults over what is saved (the pairings too).
    const read = await readSettings();
    if (!read.ok) return false;
    const next: Settings = { ...read.settings, ...change(read.settings) };
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({
        deviceName: next.deviceName,
        faceId: next.faceId,
        pairings: next.pairings,
        forget: next.forget,
        notifications: next.notifications,
        pushKinds: next.pushKinds,
        textSize: next.textSize,
        liveActivity: next.liveActivity,
        liveActivityMac: next.liveActivityMac,
      }),
    );
    return true;
  } catch {
    // The next change tries again.
    return false;
  }
}
