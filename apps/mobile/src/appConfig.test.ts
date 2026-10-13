// The native shell's colours, before any JavaScript runs: they follow the system's appearance and
// match the theme, so a dark phone never flashes white on launch or behind a screen.
import appJson from "../app.json";
import packageJson from "../package.json";
import { dark } from "./theme";

const expo = appJson.expo as typeof appJson.expo & {
  backgroundColor?: string;
  android: { backgroundColor?: string };
};

describe("app.json", () => {
  it("follows the system's light or dark appearance", () => {
    expect(expo.userInterfaceStyle).toBe("automatic");
  });

  it("paints the root view the dark theme's background, on every platform and on Android", () => {
    expect(expo.backgroundColor).toBe(dark.bg);
    expect(expo.android.backgroundColor).toBe(dark.bg);
  });

  it("has expo-system-ui, which Android needs for the appearance and iOS for the root colour", () => {
    expect((packageJson.dependencies as Record<string, string>)["expo-system-ui"]).toMatch(/^\d+\.\d+\.\d+$/);
  });
});
