// The Live Activity in the Dynamic Island and on the Lock Screen (iOS 17.2 and later). Where the
// native module is missing (Android, Expo Go, jest) every call is a no-op: isSupported() is false,
// start, update, end and setEntries resolve, and every listener's remover does nothing.
import native from "./src/OctetLiveModule";

export type ActivityKind = "running" | "approval" | "done";

/**
 * The content state, exactly as the hub sends it in `aps.content-state`: `session` is the hub's raw
 * session id (`{pid}-{started}`) or "", and `mac` is the hub's published host or "". It names no
 * session, command or workspace; the widget reads those from the App Group.
 */
export interface ActivityState {
  kind: ActivityKind;
  session: string;
  mac: string;
  /** Unix seconds: when the leading session's current state began. */
  since: number;
  approvals: number;
}

/** A Deny tapped in the widget: the host and raw session it showed, and the approval's id. */
export interface DenyEvent {
  mac: string;
  session: string;
  approval: string;
}

/** An ActivityKit token as lowercase hex, or null when the running activity's token is gone.
 * `activity` is the id of the activity it is for, when the native side names it (a push-to-start
 * token is for none). */
export interface TokenEvent {
  token: string | null;
  activity?: string;
}

/** What the widget shows for one session's waiting approval. */
export interface WidgetEntry {
  name: string;
  command: string;
  approval: string;
}

export interface Subscription {
  remove(): void;
}

export const ACTIVITY_KINDS: readonly ActivityKind[] = ["running", "approval", "done"];

/** The content state's fields and their JSON types; a test holds them to the hub's payload. */
export const ACTIVITY_STATE_FIELDS: {
  readonly [K in keyof ActivityState]-?: ActivityState[K] extends string ? "string" : "number";
} = { kind: "string", session: "string", mac: "string", since: "number", approvals: "number" };

/** The Swift `ActivityAttributes` type's name, which push-to-start matches on. */
export const ACTIVITY_ATTRIBUTES_TYPE = "OctetActivity";

/** The App Group the app and the widget share. */
export const APP_GROUP = "group.dev.octet.mobile";

/** The App Group key for a session, as the widget sees it: `<mac>/<raw session>`, the host
 * lowercased and without a trailing dot (the widget looks it up the same way), so a host that
 * differs only in case still finds its entry. */
export function entryKey(mac: string, session: string): string {
  return `${mac.replace(/\.$/, "").toLowerCase()}/${session}`;
}

const nothing: Subscription = { remove: () => undefined };

export function isSupported(): boolean {
  return native?.isSupported() ?? false;
}

export async function start(state: ActivityState): Promise<void> {
  await native?.start(state);
}

export async function update(state: ActivityState): Promise<void> {
  await native?.update(state);
}

export async function end(): Promise<void> {
  await native?.end();
}

/** Replaces every entry in the App Group; `{}` clears them (on unpair). */
export async function setEntries(entries: Record<string, WidgetEntry>): Promise<void> {
  await native?.setEntries(JSON.stringify(entries));
}

/** An ActivityKit token: 1 to 1024 lowercase hex digits, as the hub takes them (protocol.rs's
 * `ACTIVITY_TOKEN_DIGITS`). Apple doesn't fix the length; iOS 26.5 issues 256 digits. */
const TOKEN = /^[0-9a-f]{1,1024}$/;
/** An activity's id is a UUID; anything longer is not one. */
const ACTIVITY_ID_CHARS = 128;

/** A token event from native, or null if it isn't one. */
export function asTokenEvent(raw: unknown): TokenEvent | null {
  if (typeof raw !== "object" || raw === null) return null;
  const { token, activity } = raw as { token?: unknown; activity?: unknown };
  const named = typeof activity === "string" && activity !== "" && activity.length <= ACTIVITY_ID_CHARS ? { activity } : {};
  if (token === null) return { token: null, ...named };
  return typeof token === "string" && TOKEN.test(token) ? { token, ...named } : null;
}

/** A deny event from native, or null if it isn't one. */
export function asDenyEvent(raw: unknown): DenyEvent | null {
  if (typeof raw !== "object" || raw === null) return null;
  const { mac, session, approval } = raw as Record<string, unknown>;
  if (typeof mac !== "string" || typeof session !== "string" || typeof approval !== "string") return null;
  if (session === "" || approval === "") return null;
  return { mac, session, approval };
}

/** Each running activity's push token, and null when it ends. */
export function addPushTokenListener(listener: (event: TokenEvent) => void): Subscription {
  return (
    native?.addListener("pushToken", (raw) => {
      const event = asTokenEvent(raw);
      if (event) listener(event);
    }) ?? nothing
  );
}

/** The push-to-start token, whenever ActivityKit issues one. */
export function addPushToStartTokenListener(listener: (event: TokenEvent) => void): Subscription {
  return (
    native?.addListener("pushToStartToken", (raw) => {
      const event = asTokenEvent(raw);
      if (event?.token) listener(event);
    }) ?? nothing
  );
}

/** Deny taps from the widget, including any made before this listener was added. */
export function addDenyListener(listener: (event: DenyEvent) => void): Subscription {
  return (
    native?.addListener("deny", (raw) => {
      const event = asDenyEvent(raw);
      if (event) listener(event);
    }) ?? nothing
  );
}
