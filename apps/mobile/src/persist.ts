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
}

export const DEFAULT_SETTINGS: Settings = { deviceName: null, faceId: true, hubUrl: null };
export const SETTINGS_KEY = "octet.settings.v1";

/** The longest string kept: a longer one is not a name or a URL. */
const MAX_CHARS = 2_048;

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const text = (x: unknown): string | null => (typeof x === "string" && x.length <= MAX_CHARS ? x : null);

/** The saved settings; each field that is missing or not the right type is its default. */
export async function loadSettings(): Promise<Settings> {
  try {
    const raw = await AsyncStorage.getItem(SETTINGS_KEY);
    if (raw === null) return DEFAULT_SETTINGS;
    const value: unknown = JSON.parse(raw);
    if (!isObj(value)) return DEFAULT_SETTINGS;
    return {
      deviceName: text(value.deviceName),
      faceId: typeof value.faceId === "boolean" ? value.faceId : DEFAULT_SETTINGS.faceId,
      hubUrl: text(value.hubUrl),
    };
  } catch {
    return DEFAULT_SETTINGS;
  }
}

/** Saves wait their turn: a patch reads what the one before wrote. */
let saving: Promise<void> = Promise.resolve();

/** Saves `patch` over what is saved. A failed write is dropped: settings are a convenience. */
export function saveSettings(patch: Partial<Settings>): Promise<void> {
  saving = saving.then(() => write(patch));
  return saving;
}

async function write(patch: Partial<Settings>): Promise<void> {
  try {
    const next: Settings = { ...(await loadSettings()), ...patch };
    await AsyncStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({ deviceName: next.deviceName, faceId: next.faceId, hubUrl: next.hubUrl }),
    );
  } catch {
    // The next change tries again.
  }
}
