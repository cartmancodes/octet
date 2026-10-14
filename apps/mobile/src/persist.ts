// The only things the phone keeps on disk: non-secret settings. The device token is in
// expo-secure-store (wired in the pairing task); cursors are in memory and transcripts are never
// saved, so a cold start subscribes fresh.
import AsyncStorage from "@react-native-async-storage/async-storage";

export interface Settings {
  /** The name this phone pairs under; null until chosen. */
  deviceName: string | null;
  /** Allow asks for Face ID or the fingerprint. On unless the user turned it off. */
  faceId: boolean;
  /** The hub's address, wss://...; null until paired. */
  hubUrl: string | null;
  /** Push notifications are on: the Mac has this phone's token. Off unless the user turned it on. */
  notifications: boolean;
  /** The transcript's text size in points, which `Aa` cycles. */
  textSize: TextSize;
}

export const TEXT_SIZES = [12, 13, 14] as const;
export type TextSize = (typeof TEXT_SIZES)[number];

export const DEFAULT_SETTINGS: Settings = { deviceName: null, faceId: true, hubUrl: null, notifications: false, textSize: 13 };
export const SETTINGS_KEY = "octet.settings.v1";
/** What to tell the user when a settings write failed. */
export const NOT_SAVED = "Couldn't save that setting";

/** The longest string kept: a longer one is not a name or a URL. */
const MAX_CHARS = 2_048;

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const text = (x: unknown): string | null => (typeof x === "string" && x.length <= MAX_CHARS ? x : null);
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
    return {
      ok: true,
      settings: {
        deviceName: text(value.deviceName),
        faceId: typeof value.faceId === "boolean" ? value.faceId : DEFAULT_SETTINGS.faceId,
        hubUrl: text(value.hubUrl),
        notifications: typeof value.notifications === "boolean" ? value.notifications : DEFAULT_SETTINGS.notifications,
        textSize: textSize(value.textSize),
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
  saving = saving.then(() => write(patch));
  return saving;
}

async function write(patch: Partial<Settings>): Promise<boolean> {
  try {
    // A read that failed would write the defaults over what is saved (the hub address too).
    const read = await readSettings();
    if (!read.ok) return false;
    const next: Settings = { ...read.settings, ...patch };
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({
        deviceName: next.deviceName,
        faceId: next.faceId,
        hubUrl: next.hubUrl,
        notifications: next.notifications,
        textSize: next.textSize,
      }),
    );
    return true;
  } catch {
    // The next change tries again.
    return false;
  }
}
