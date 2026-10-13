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

describe("eas.json", () => {
  // Read from disk: a missing file fails here by name rather than as a module error.
  const read = (): Record<string, unknown> => {
    const fs = jest.requireActual<typeof import("fs")>("fs");
    const path = jest.requireActual<typeof import("path")>("path");
    return JSON.parse(fs.readFileSync(path.join(__dirname, "..", "eas.json"), "utf8")) as Record<string, unknown>;
  };
  type Profile = {
    developmentClient?: boolean;
    distribution?: string;
    android?: { buildType?: string };
    ios?: { buildConfiguration?: string };
  };
  const profiles = (): Record<string, Profile> => (read().build ?? {}) as Record<string, Profile>;

  it("has a development profile: a dev client, for internal installs only", () => {
    expect(profiles().development).toMatchObject({ developmentClient: true, distribution: "internal" });
  });

  it("has a preview profile for daily use: a release build with no dev client, an APK on Android", () => {
    const preview = profiles().preview;
    expect(preview).toBeDefined();
    expect(preview?.developmentClient).not.toBe(true);
    expect(preview?.distribution).toBe("internal");
    expect(preview?.android?.buildType).toBe("apk");
    expect(preview?.ios?.buildConfiguration).toBe("Release");
  });

  it("has a production profile with no dev client", () => {
    expect(profiles().production).toBeDefined();
    expect(profiles().production?.developmentClient).not.toBe(true);
  });

  it("holds no project id and no account", () => {
    const text = JSON.stringify(read());
    expect(text).not.toMatch(/projectId|owner|appleId|ascAppId|appleTeamId|serviceAccountKeyPath/);
  });
});
