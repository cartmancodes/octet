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

describe("the Live Activity", () => {
  const config = expo as typeof expo & {
    ios: {
      deploymentTarget?: string;
      entitlements?: Record<string, unknown>;
      infoPlist?: { NSSupportsLiveActivities?: boolean; NSSupportsLiveActivitiesFrequentUpdates?: boolean };
    };
  };

  it("adds the widget target through @bacons/apple-targets, pinned exactly as a dev dependency", () => {
    const plugins = expo.plugins as (string | [string, object])[];
    expect(plugins.map((p) => (typeof p === "string" ? p : p[0]))).toContain("@bacons/apple-targets");
    const dev = packageJson.devDependencies as Record<string, string>;
    expect(dev["@bacons/apple-targets"]).toMatch(/^\d+\.\d+\.\d+$/);
    expect((packageJson.dependencies as Record<string, string>)["@bacons/apple-targets"]).toBeUndefined();
  });

  it("shares the App Group with the widget", () => {
    expect(config.ios.entitlements?.["com.apple.security.application-groups"]).toEqual(["group.dev.octet.mobile"]);
  });

  it("supports Live Activities, without frequent updates", () => {
    expect(config.ios.infoPlist?.NSSupportsLiveActivities).toBe(true);
    expect(config.ios.infoPlist?.NSSupportsLiveActivitiesFrequentUpdates).toBe(false);
  });

  it("doesn't raise the app's iOS floor: only the widget needs iOS 17.2", () => {
    expect(config.ios.deploymentTarget).toBeUndefined();
  });

  it("doesn't link apple-targets' own ExtensionStorage module into the app", () => {
    const autolinking = (packageJson as { expo?: { autolinking?: { exclude?: string[] } } }).expo?.autolinking;
    expect(autolinking?.exclude).toEqual(["@bacons/apple-targets"]);
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

describe("the app's reach", () => {
  const config = expo as typeof expo & {
    ios: { infoPlist?: { NSAppTransportSecurity?: { NSAllowsArbitraryLoads?: boolean } } };
    android: { blockedPermissions?: string[]; permissions?: string[] };
  };

  it("keeps App Transport Security on: the app opens only wss:// to its Mac", () => {
    expect(config.ios.infoPlist?.NSAppTransportSecurity?.NSAllowsArbitraryLoads).toBe(false);
  });

  it("blocks the Android permissions it does not use, and keeps the ones it does", () => {
    const blocked = config.android.blockedPermissions ?? [];
    for (const p of [
      "android.permission.SYSTEM_ALERT_WINDOW",
      "android.permission.READ_EXTERNAL_STORAGE",
      "android.permission.WRITE_EXTERNAL_STORAGE",
      "android.permission.RECORD_AUDIO",
    ]) {
      expect(blocked).toContain(p);
    }
    for (const kept of ["CAMERA", "INTERNET", "USE_BIOMETRIC", "USE_FINGERPRINT", "POST_NOTIFICATIONS", "VIBRATE"]) {
      expect(blocked).not.toContain(`android.permission.${kept}`);
    }
  });
});
