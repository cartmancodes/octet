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
