// Push notifications: the permission, the Expo push token, the Android channels, the foreground
// rule and what a tap names. The Mac's hub sends the pushes (title "Octet", body "Approval
// waiting" or "Turn finished", `data.session`, a channel); this file only prepares the phone for
// them. Registering the token with the hub is `HubController`'s job (src/hub.ts).
import Constants from "expo-constants";
import * as Notifications from "expo-notifications";
import { Platform } from "react-native";

export const NO_PROJECT_ID = "Notifications need this app's EAS project id. See the README.";
export const DENIED = "Notifications are off for Octet in the phone's settings.";
const FAILED = "Couldn't turn on notifications. Try again.";

/** What Expo's push service hands out; the hub accepts the same shape and no other. */
const PUSH_TOKEN = /^Expo(nent)?PushToken\[[A-Za-z0-9_-]{1,200}\]$/;
/** The hub's session ids are `{pid}-{started}`, both numbers. */
const SESSION_ID = /^[0-9]+-[0-9]+$/;
/** No id the hub makes is longer; a tap's data comes from outside the app. */
const SESSION_ID_CHARS = 128;

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
  try {
    const { data } = await Notifications.getExpoPushTokenAsync({ projectId: id });
    return PUSH_TOKEN.test(data) ? { ok: true, token: data } : { ok: false, message: FAILED };
  } catch {
    return { ok: false, message: FAILED };
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

/** Calls `fn` when the phone's push token rotates; returns the stop. */
export function onTokenChange(fn: () => void): () => void {
  const subscription = Notifications.addPushTokenListener(fn);
  return () => subscription.remove();
}

/**
 * Taps on notifications: a tap while the app runs (the listener) and the tap that launched it
 * (the last response). `fn` gets the session the tap names, or null when the data names none that
 * is valid. A tap is handed over once, however many ways it is reported, and the native side is
 * told it was taken, so that no later mount of the shell opens it again. Returns the stop.
 */
export function watchTaps(fn: (session: string | null) => void): () => void {
  let live = true;
  let last: string | null = null;
  const take = (response: Notifications.NotificationResponse | null): void => {
    if (!live || response === null) return;
    const id = response.notification.request.identifier;
    if (id === last) return;
    last = id;
    fn(sessionFromData(response.notification.request.content.data));
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
