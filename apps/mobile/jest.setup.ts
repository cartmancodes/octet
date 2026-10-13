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
// (Its own `jestSetup` still points at a component that FlashList 2 no longer exports.)
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
