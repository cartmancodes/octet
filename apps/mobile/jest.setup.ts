// Test doubles every test file shares: async-storage and the keychain are in memory.
jest.mock("@react-native-async-storage/async-storage", () =>
  jest.requireActual("@react-native-async-storage/async-storage/jest/async-storage-mock"),
);

jest.mock("expo-secure-store", () => {
  const kept = new Map<string, string>();
  return {
    WHEN_UNLOCKED_THIS_DEVICE_ONLY: 6,
    getItemAsync: jest.fn(async (key: string, _options?: object) => kept.get(key) ?? null),
    setItemAsync: jest.fn(async (key: string, value: string, _options?: object) => {
      kept.set(key, value);
    }),
    deleteItemAsync: jest.fn(async (key: string, _options?: object) => {
      kept.delete(key);
    }),
  };
});

// FlashList 2 measures its parent and rows through native layout, which a test renderer cannot do.
// (Its own `jestSetup` still points at a component that FlashList 2 no longer exports.) The path
// is private to FlashList: src/flashListMock.test.tsx fails by name if an upgrade moves it.
jest.mock("@shopify/flash-list/dist/recyclerview/utils/measureLayout", () => ({
  ...jest.requireActual("@shopify/flash-list/dist/recyclerview/utils/measureLayout"),
  measureParentSize: jest.fn(() => ({ x: 0, y: 0, width: 400, height: 900 })),
  measureFirstChildLayout: jest.fn(() => ({ x: 0, y: 0, width: 400, height: 900 })),
  measureItemLayout: jest.fn(() => ({ x: 0, y: 0, width: 400, height: 100 })),
}));

// Biometrics succeed by default; a test that needs a failure sets it. Haptics do nothing.
jest.mock("expo-local-authentication", () => ({
  hasHardwareAsync: jest.fn(async () => true),
  isEnrolledAsync: jest.fn(async () => true),
  authenticateAsync: jest.fn(async (_options?: object) => ({ success: true })),
}));

jest.mock("expo-haptics", () => ({
  ImpactFeedbackStyle: { Light: "light", Medium: "medium", Heavy: "heavy" },
  NotificationFeedbackType: { Success: "success", Warning: "warning", Error: "error" },
  impactAsync: jest.fn(async () => undefined),
  notificationAsync: jest.fn(async () => undefined),
}));

// The root view's colour is native; the layout sets it from the theme.
jest.mock("expo-system-ui", () => ({
  setBackgroundColorAsync: jest.fn(async (_color: string | null) => undefined),
  getBackgroundColorAsync: jest.fn(async () => null),
}));

// Notifications: permission is granted and a token comes back unless a test says otherwise. Nothing
// reaches a native module. The behaviours are set before each test, because `restoreAllMocks`
// (which several files call) strips a `jest.fn`'s implementation.
jest.mock("expo-notifications", () => ({
  AndroidImportance: { DEFAULT: 5, HIGH: 6 },
  setNotificationHandler: jest.fn(),
  setNotificationChannelAsync: jest.fn(),
  getPermissionsAsync: jest.fn(),
  requestPermissionsAsync: jest.fn(),
  getExpoPushTokenAsync: jest.fn(),
  addPushTokenListener: jest.fn(),
  addNotificationResponseReceivedListener: jest.fn(),
  getLastNotificationResponse: jest.fn(),
  clearLastNotificationResponse: jest.fn(),
  unregisterForNotificationsAsync: jest.fn(),
  dismissAllNotificationsAsync: jest.fn(),
}));

// The splash stays up until the app knows whether it is paired.
jest.mock("expo-splash-screen", () => ({
  preventAutoHideAsync: jest.fn(async () => true),
  hideAsync: jest.fn(async () => undefined),
}));

// There is no native animation driver under test (its values never move), so animations run on the
// JS driver: a test sees an opening drawer's scrim reach its opacity. RN decides this in
// `shouldUseNativeDriver`, which only its private (non-exported) NativeAnimatedHelper module holds, so
// the test reaches into that path. It is a plain require: if a React Native upgrade moves or renames
// the file, every test fails loudly at set-up ("Cannot find module"), rather than animations
// silently stopping. Fix the path (or drop this) then.
const animatedHelper = jest.requireActual<{ default: { shouldUseNativeDriver: () => boolean } }>(
  "react-native/src/private/animated/NativeAnimatedHelper",
).default;
animatedHelper.shouldUseNativeDriver = () => false;

beforeEach(() => {
  // The app is in front, as it is for anyone tapping it. React Native's mock leaves it unset.
  const { AppState } = jest.requireActual<typeof import("react-native")>("react-native");
  (AppState as { currentState: string }).currentState = "active";
  const granted = { granted: true, canAskAgain: true, status: "granted" };
  const n = jest.requireMock("expo-notifications") as Record<string, jest.Mock>;
  for (const mock of Object.values(n)) if (jest.isMockFunction(mock)) mock.mockReset();
  n.setNotificationChannelAsync!.mockResolvedValue(null);
  n.getPermissionsAsync!.mockResolvedValue(granted);
  n.requestPermissionsAsync!.mockResolvedValue(granted);
  n.getExpoPushTokenAsync!.mockResolvedValue({ type: "expo", data: "ExponentPushToken[abc-123_XYZ]" });
  n.addPushTokenListener!.mockImplementation(() => ({ remove: jest.fn() }));
  n.addNotificationResponseReceivedListener!.mockImplementation(() => ({ remove: jest.fn() }));
  n.getLastNotificationResponse!.mockReturnValue(null);
  n.unregisterForNotificationsAsync!.mockResolvedValue(undefined);
  n.dismissAllNotificationsAsync!.mockResolvedValue(undefined);
  const splash = jest.requireMock("expo-splash-screen") as Record<string, jest.Mock>;
  for (const mock of Object.values(splash)) mock.mockReset();
  splash.preventAutoHideAsync!.mockResolvedValue(true);
  splash.hideAsync!.mockResolvedValue(undefined);
});
