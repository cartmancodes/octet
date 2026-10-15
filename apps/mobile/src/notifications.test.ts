import * as Notifications from "expo-notifications";
import { Platform } from "react-native";

import {
  DENIED,
  NO_PROJECT_ID,
  askForToken,
  configureNotifications,
  currentToken,
  projectId,
  sessionFromData,
  watchTaps,
} from "./notifications";

let mockExtra: { eas?: { projectId?: string } } | undefined;
let mockEas: { projectId?: string } | undefined;
jest.mock("expo-constants", () => ({
  __esModule: true,
  default: {
    get expoConfig() {
      return { extra: mockExtra };
    },
    get easConfig() {
      return mockEas;
    },
  },
}));

const n = Notifications as jest.Mocked<typeof Notifications>;
const PROJECT = "11111111-2222-3333-4444-555555555555";

beforeEach(() => {
  jest.clearAllMocks();
  mockExtra = { eas: { projectId: PROJECT } };
  mockEas = undefined;
});

describe("the EAS project id", () => {
  it("comes from the app's config, else the EAS config, and is never invented", () => {
    expect(projectId()).toBe(PROJECT);
    mockExtra = undefined;
    mockEas = { projectId: "from-eas" };
    expect(projectId()).toBe("from-eas");
    mockEas = undefined;
    expect(projectId()).toBeNull();
    mockExtra = { eas: { projectId: "" } };
    expect(projectId()).toBeNull();
  });

  it("is the words the README section is named for", () => {
    expect(NO_PROJECT_ID).toBe("Notifications need this app's EAS project id. See the README.");
    expect(DENIED).toBe("Notifications are off for Octet in the phone's settings.");
  });
});

describe("turning on", () => {
  it("asks for permission, then for the token of the project", async () => {
    await expect(askForToken()).resolves.toEqual({ ok: true, token: "ExponentPushToken[abc-123_XYZ]" });
    expect(n.requestPermissionsAsync).toHaveBeenCalledTimes(1);
    expect(n.getExpoPushTokenAsync).toHaveBeenCalledWith({ projectId: PROJECT });
  });

  it("stops at a denial, in the words for the phone's settings, without a token", async () => {
    n.requestPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    await expect(askForToken()).resolves.toEqual({ ok: false, message: DENIED });
    expect(n.getExpoPushTokenAsync).not.toHaveBeenCalled();
  });

  it("with no project id neither asks nor fetches", async () => {
    mockExtra = undefined;
    await expect(askForToken()).resolves.toEqual({ ok: false, message: NO_PROJECT_ID });
    expect(n.requestPermissionsAsync).not.toHaveBeenCalled();
    expect(n.getExpoPushTokenAsync).not.toHaveBeenCalled();
  });

  it("gives up on a token fetch after 15 s, and one that answers leaves no timer behind", async () => {
    jest.useFakeTimers();
    try {
      await expect(askForToken()).resolves.toMatchObject({ ok: true });
      expect(jest.getTimerCount()).toBe(0);
      n.getExpoPushTokenAsync.mockReturnValueOnce(new Promise(() => {}) as never);
      let result: unknown;
      void currentToken().then((value) => (result = value));
      await jest.advanceTimersByTimeAsync(14_999);
      expect(result).toBeUndefined();
      await jest.advanceTimersByTimeAsync(1);
      expect(result).toEqual({ ok: false, message: "Couldn't turn on notifications. Try again." });
    } finally {
      jest.useRealTimers();
    }
  });

  it("says so when the token cannot be had", async () => {
    n.getExpoPushTokenAsync.mockRejectedValueOnce(new Error("offline"));
    await expect(askForToken()).resolves.toEqual({ ok: false, message: "Couldn't turn on notifications. Try again." });
  });

  it("registers only a token of the shape Expo's push service gives", async () => {
    n.getExpoPushTokenAsync.mockResolvedValueOnce({ type: "expo", data: "not a token" } as never);
    await expect(askForToken()).resolves.toMatchObject({ ok: false });
  });

  it("the current token is fetched without a prompt, and only while permission is still granted", async () => {
    await expect(currentToken()).resolves.toEqual({ ok: true, token: "ExponentPushToken[abc-123_XYZ]" });
    expect(n.requestPermissionsAsync).not.toHaveBeenCalled();
    n.getPermissionsAsync.mockResolvedValueOnce({ granted: false, canAskAgain: false, status: "denied" } as never);
    await expect(currentToken()).resolves.toEqual({ ok: false, message: DENIED });
  });
});

describe("while the app is in the foreground", () => {
  it("shows nothing: no banner, no list entry, no sound, no badge", async () => {
    await configureNotifications();
    const handler = n.setNotificationHandler.mock.calls[0]?.[0];
    await expect(handler?.handleNotification({} as never)).resolves.toEqual({
      shouldShowBanner: false,
      shouldShowList: false,
      shouldPlaySound: false,
      shouldSetBadge: false,
    });
  });
});

describe("Android channels", () => {
  const android = () => jest.replaceProperty(Platform, "OS", "android");

  it("creates approvals and errors (high, with sound) and turns (default) at start-up", async () => {
    const replaced = android();
    try {
      await configureNotifications();
    } finally {
      replaced.restore();
    }
    expect(n.setNotificationChannelAsync).toHaveBeenCalledWith("approvals", {
      name: "Approvals",
      importance: 6,
      sound: "default",
    });
    expect(n.setNotificationChannelAsync).toHaveBeenCalledWith("errors", {
      name: "Session errors",
      importance: 6,
      sound: "default",
    });
    expect(n.setNotificationChannelAsync).toHaveBeenCalledWith("turns", { name: "Finished turns", importance: 5 });
    // Every channel the hub's pushes name, and no other.
    expect(n.setNotificationChannelAsync.mock.calls.map(([id]) => id).sort()).toEqual(["approvals", "errors", "turns"]);
  });

  it("creates no channel on iOS", async () => {
    await configureNotifications();
    expect(n.setNotificationChannelAsync).not.toHaveBeenCalled();
  });

  it("a channel that cannot be made does not stop the app", async () => {
    n.setNotificationChannelAsync.mockRejectedValue(new Error("no"));
    const replaced = android();
    try {
      await expect(configureNotifications()).resolves.toBeUndefined();
    } finally {
      replaced.restore();
      n.setNotificationChannelAsync.mockResolvedValue(null);
    }
  });
});

describe("what a tap names", () => {
  it("is a session id of the hub's shape: pid, a hyphen, the start time", () => {
    expect(sessionFromData({ session: "4821-1790000000" })).toBe("4821-1790000000");
  });

  it.each([
    ["no data", undefined],
    ["no session", {}],
    ["a number", { session: 4821 }],
    ["an empty string", { session: "" }],
    ["a path", { session: "../settings" }],
    ["a query", { session: "1-2?x=3" }],
    ["letters", { session: "live" }],
    ["a missing half", { session: "4821-" }],
    ["a trailing newline", { session: "4821-1\n" }],
    ["over 128 characters", { session: `${"1".repeat(127)}-2` }],
  ])("is nothing for %s", (_name, data) => {
    expect(sessionFromData(data)).toBeNull();
  });

  it("allows the longest id, 128 characters", () => {
    const id = `${"1".repeat(100)}-${"2".repeat(27)}`;
    expect(id).toHaveLength(128);
    expect(sessionFromData({ session: id })).toBe(id);
  });
});

describe("watching taps", () => {
  const response = (data: unknown, id = "n1") => ({ notification: { request: { identifier: id, content: { data } } } });

  it("hands over a cold start's tap once, and a warm one from the listener", () => {
    n.getLastNotificationResponse.mockReturnValueOnce(response({ session: "1-2" }) as never);
    const heard: (string | null)[] = [];
    const stop = watchTaps((s) => heard.push(s));
    expect(heard).toEqual(["1-2"]);
    const listener = n.addNotificationResponseReceivedListener.mock.calls[0]?.[0];
    listener?.(response({ session: "3-4" }, "n2") as never);
    listener?.(response({ session: "3-4" }, "n2") as never); // the same tap, delivered twice
    listener?.(response({ session: "bad" }, "n3") as never);
    expect(heard).toEqual(["1-2", "3-4", null]);
    stop();
  });

  it("clears the last response once it is taken, so that no later start sees it again", () => {
    let last: unknown = response({ session: "1-2" });
    n.getLastNotificationResponse.mockImplementation(() => last as never);
    n.clearLastNotificationResponse.mockImplementation(() => {
      last = null;
    });
    const heard: (string | null)[] = [];
    watchTaps((s) => heard.push(s))();
    watchTaps((s) => heard.push(s))();
    expect(heard).toEqual(["1-2"]);
    expect(n.clearLastNotificationResponse).toHaveBeenCalledTimes(1);
  });

  it("does not repeat the cold start's tap when the listener reports it too", () => {
    n.getLastNotificationResponse.mockReturnValueOnce(response({ session: "1-2" }) as never);
    const heard: (string | null)[] = [];
    watchTaps((s) => heard.push(s));
    const listener = n.addNotificationResponseReceivedListener.mock.calls[0]?.[0];
    listener?.(response({ session: "1-2" }) as never);
    expect(heard).toEqual(["1-2"]);
  });

  it("a module without the last response is no tap, and not an error", () => {
    n.getLastNotificationResponse.mockImplementation(() => {
      throw new Error("unavailable");
    });
    const heard: (string | null)[] = [];
    expect(() => watchTaps((s) => heard.push(s))).not.toThrow();
    expect(heard).toEqual([]);
  });

  it("stops listening when asked", () => {
    const remove = jest.fn();
    n.addNotificationResponseReceivedListener.mockReturnValueOnce({ remove } as never);
    const heard: (string | null)[] = [];
    watchTaps((s) => heard.push(s))();
    expect(remove).toHaveBeenCalled();
  });
});
