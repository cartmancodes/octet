// The native shell's colours, before any JavaScript runs: they follow the system's appearance and
// match the theme, so a dark phone never flashes white on launch or behind a screen.
import appJson from "../app.json";
import packageJson from "../package.json";
import { dark, light } from "./theme";

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

  it("has the notifications and splash-screen plugins, pinned", () => {
    const plugins = expo.plugins as (string | [string, object])[];
    const names = plugins.map((p) => (typeof p === "string" ? p : p[0]));
    expect(names).toContain("expo-notifications");
    expect(names).toContain("expo-splash-screen");
    const deps = packageJson.dependencies as Record<string, string>;
    expect(deps["expo-notifications"]).toMatch(/^\d+\.\d+\.\d+$/);
    expect(deps["expo-splash-screen"]).toMatch(/^\d+\.\d+\.\d+$/);
  });

  it("paints the splash with the theme's background, light and dark, and never invents an EAS project id", () => {
    const plugins = expo.plugins as (string | [string, { backgroundColor?: string; dark?: { backgroundColor?: string } }])[];
    const splash = plugins.find((p) => Array.isArray(p) && p[0] === "expo-splash-screen");
    expect(splash).toBeDefined();
    const options = (splash as [string, { backgroundColor?: string; dark?: { backgroundColor?: string } }])[1];
    expect(options.backgroundColor).toBe(light.bg);
    expect(options.dark?.backgroundColor).toBe(dark.bg);
    expect((expo as { extra?: { eas?: unknown } }).extra?.eas).toBeUndefined();
  });
});
