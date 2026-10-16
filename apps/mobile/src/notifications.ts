// Push notifications: the permission, the Expo push token, the Android channels, the foreground
// rule and what a tap names. The Mac's hub sends the pushes (title "Octet", body "Approval
// waiting", "Session error" or "Turn finished", `data.session`, `data.mac` (the host it is
// published at) when it knows it, and the channel "approvals", "errors" or "turns"); this file
// only prepares the phone for them. Registering the token with each Mac's hub is the Fleet's job
// (`MacLink`, src/hub.ts), and so is finding the Mac a tap is for (`Fleet.tapKey`).
import Constants from "expo-constants";
import * as Notifications from "expo-notifications";
import { Platform } from "react-native";

import type { Tap } from "./macs";

export type { Tap };

export const NO_PROJECT_ID = "Notifications need this app's EAS project id. See the README.";
export const DENIED = "Notifications are off for Octet in the phone's settings.";
const FAILED = "Couldn't turn on notifications. Try again.";

/** What Expo's push service hands out; the hub accepts the same shape and no other. */
const PUSH_TOKEN = /^Expo(nent)?PushToken\[[A-Za-z0-9_-]{1,200}\]$/;
/** The hub's session ids are `{pid}-{started}`, both numbers. */
const SESSION_ID = /^[0-9]+-[0-9]+$/;
/** No id the hub makes is longer; a tap's data comes from outside the app. */
const SESSION_ID_CHARS = 128;
/** How long a push token fetch may take. It can hang (no network to Expo, or to Apple or
 * Google), and push work runs one job at a time, so a hung fetch would hold every later one. */
const TOKEN_WAIT_MS = 15_000;

/** The app's EAS project id, from its config; null when it has none. Never made up: push tokens
 * belong to the project that built the app. */
export function projectId(): string | null {
  const id = Constants.expoConfig?.extra?.eas?.projectId ?? Constants.easConfig?.projectId;
  return typeof id === "string" && id !== "" ? id : null;
}

/** The session a notification names, or null for data that is not a session id of the hub's shape. */
export function sessionFromData(data: unknown): string | null {
  if (typeof data !== "object" || data === null) return null;
  const session = (data as { session?: unknown }).session;
  return typeof session === "string" && session.length <= SESSION_ID_CHARS && SESSION_ID.test(session)
    ? session
    : null;
}

/** The longest DNS name, and the longest label of one. */
const HOST_CHARS = 253;
const HOST_LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/i;

/** The host a notification names as its Mac (`data.mac`), or null for data that is not a DNS
 * name: 253 characters at most, each label 1 to 63 letters, digits and hyphens, never starting or
 * ending with a hyphen. */
function hostFromData(data: unknown): string | null {
  if (typeof data !== "object" || data === null) return null;
  const mac = (data as { mac?: unknown }).mac;
  if (typeof mac !== "string" || mac.length > HOST_CHARS) return null;
  return mac.split(".").every((label) => HOST_LABEL.test(label)) ? mac : null;
}

/** What a notification's data names: the session, and the host of the Mac that sent it. Each is
 * null when the data names none that is valid. */
export function tapFromData(data: unknown): Tap {
  return { host: hostFromData(data), session: sessionFromData(data) };
}

/** Start-up, once: nothing shows while the app is open (the screen is already live), and on
 * Android the channels the hub's pushes name exist. A channel that cannot be made is not a
 * reason to stop. */
export async function configureNotifications(): Promise<void> {
  Notifications.setNotificationHandler({
    handleNotification: async () => ({
      shouldShowBanner: false,
      shouldShowList: false,
      shouldPlaySound: false,
      shouldSetBadge: false,
    }),
  });
  if (Platform.OS !== "android") return;
  try {
    await Notifications.setNotificationChannelAsync("approvals", {
      name: "Approvals",
      importance: Notifications.AndroidImportance.HIGH,
      sound: "default",
    });
    await Notifications.setNotificationChannelAsync("errors", {
      name: "Session errors",
      importance: Notifications.AndroidImportance.HIGH,
      sound: "default",
    });
    await Notifications.setNotificationChannelAsync("turns", {
      name: "Finished turns",
      importance: Notifications.AndroidImportance.DEFAULT,
    });
  } catch {
    // Android makes a channel of its own for a push that names none.
  }
}

/** A token, or the words for why there is none. */
export type PushToken = { ok: true; token: string } | { ok: false; message: string };

async function tokenFor(id: string): Promise<PushToken> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), TOKEN_WAIT_MS);
  });
  try {
    const got = await Promise.race([Notifications.getExpoPushTokenAsync({ projectId: id }), late]);
    if (got === null) return { ok: false, message: FAILED };
    return PUSH_TOKEN.test(got.data) ? { ok: true, token: got.data } : { ok: false, message: FAILED };
  } catch {
    return { ok: false, message: FAILED };
  } finally {
    clearTimeout(timer);
  }
}

/** Turning on: asks permission (the system shows its prompt once), then fetches the token. */
export async function askForToken(): Promise<PushToken> {
  const id = projectId();
  if (id === null) return { ok: false, message: NO_PROJECT_ID };
  try {
    const permission = await Notifications.requestPermissionsAsync();
    if (!permission.granted) return { ok: false, message: DENIED };
  } catch {
    return { ok: false, message: FAILED };
  }
  return tokenFor(id);
}

/** The token to register at start-up. Never prompts: a permission the user has since taken away
 * is `DENIED`. */
export async function currentToken(): Promise<PushToken> {
  const id = projectId();
  if (id === null) return { ok: false, message: NO_PROJECT_ID };
  try {
    const permission = await Notifications.getPermissionsAsync();
    if (!permission.granted) return { ok: false, message: DENIED };
  } catch {
    return { ok: false, message: FAILED };
  }
  return tokenFor(id);
}

/** Best effort: this phone stops receiving pushes for the app, so Expo reports its token as gone
 * (`DeviceNotRegistered`) and the hub drops it. For an unpair the Mac did not hear. A later
 * turn-on registers again. */
export async function dropPushRegistration(): Promise<void> {
  try {
    await Notifications.unregisterForNotificationsAsync();
  } catch {
    // Nothing more can be done from here; revoking on the Mac drops the token there.
  }
}

/** Best effort: removes the app's notifications from the tray. The app shows the live state once
 * it is open, so what they said is old news. */
export async function clearDelivered(): Promise<void> {
  try {
    await Notifications.dismissAllNotificationsAsync();
  } catch {
    // They stay in the tray; nothing depends on them.
  }
}

/** Calls `fn` when the phone's push token rotates; returns the stop. */
export function onTokenChange(fn: () => void): () => void {
  const subscription = Notifications.addPushTokenListener(fn);
  return () => subscription.remove();
}

/**
 * Taps on notifications: a tap while the app runs (the listener) and the tap that launched it
 * (the last response). `fn` gets what the tap names (`tapFromData`). A tap is handed over once,
 * however many ways it is reported, and the native side is told it was taken, so that no later
 * mount of the shell opens it again. Returns the stop.
 */
export function watchTaps(fn: (tap: Tap) => void): () => void {
  let live = true;
  let last: string | null = null;
  const take = (response: Notifications.NotificationResponse | null): void => {
    if (!live || response === null) return;
    const id = response.notification.request.identifier;
    if (id === last) return;
    last = id;
    fn(tapFromData(response.notification.request.content.data));
    try {
      Notifications.clearLastNotificationResponse();
    } catch {
      // Without the native module there is no last response to repeat either.
    }
  };
  const subscription = Notifications.addNotificationResponseReceivedListener(take);
  try {
    take(Notifications.getLastNotificationResponse());
  } catch {
    // The module cannot say (an UnavailabilityError): the launch was not a tap.
  }
  return () => {
    live = false;
    subscription.remove();
  };
}
