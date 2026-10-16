// Swift can't be compiled on this machine (no iOS SDK), so these read the native sources and configs
// as text and hold the cross-file contracts in place: the type push-to-start matches on, the content
// state's fields, the App Group, the Deny hand-off, and the iOS 17.2 floor.
import { readFileSync } from "node:fs";
import path from "node:path";

import appJson from "../../app.json";
import { ACTIVITY_ATTRIBUTES_TYPE, ACTIVITY_STATE_FIELDS, APP_GROUP } from "./index";

const app = path.resolve(__dirname, "..", "..");
const read = (...parts: string[]): string => readFileSync(path.join(app, ...parts), "utf8");

const moduleAttributes = read("modules", "octet-live", "ios", "OctetActivityAttributes.swift");
const widgetAttributes = read("targets", "octet-live", "OctetActivityAttributes.swift");
const moduleSwift = read("modules", "octet-live", "ios", "OctetLiveModule.swift");
const widgetSwift = read("targets", "octet-live", "OctetLiveWidget.swift");
const denySwift = read("targets", "octet-live", "_shared", "DenyIntent.swift");
const podspec = read("modules", "octet-live", "ios", "OctetLive.podspec");

/** The `let name: Type` lines inside `struct ContentState`. */
function contentStateFields(swift: string): Record<string, string> {
  const body = /struct ContentState\b[^{]*\{([\s\S]*?)\n\s*\}/.exec(swift)?.[1] ?? "";
  const fields: Record<string, string> = {};
  for (const m of body.matchAll(/^\s*(?:public\s+)?(?:let|var)\s+(\w+)\s*:\s*(\w+)/gm)) {
    fields[m[1] as string] = m[2] as string;
  }
  return fields;
}

describe("the ActivityAttributes type", () => {
  it("is named exactly as the hub's attributes-type, with no attributes of its own", () => {
    expect(moduleAttributes).toMatch(new RegExp(`struct ${ACTIVITY_ATTRIBUTES_TYPE}\\s*:\\s*ActivityAttributes\\b`));
    const outside = moduleAttributes.replace(/struct ContentState\b[^{]*\{[\s\S]*?\n\s*\}/, "");
    expect(outside).not.toMatch(/^\s*(?:public\s+)?(?:let|var)\s+\w+\s*:/m);
  });

  it("is the same file in the module and in the widget, so both decode one shape", () => {
    expect(widgetAttributes).toBe(moduleAttributes);
  });

  it("declares the hub's content-state fields, with matching Swift types", () => {
    const swiftType: Record<string, string> = { String: "string", Int: "number", Double: "number" };
    const fields = contentStateFields(moduleAttributes);
    expect(Object.keys(fields).sort()).toEqual(Object.keys(ACTIVITY_STATE_FIELDS).sort());
    for (const [name, type] of Object.entries(fields)) {
      expect([name, swiftType[type]]).toEqual([name, ACTIVITY_STATE_FIELDS[name as keyof typeof ACTIVITY_STATE_FIELDS]]);
    }
  });
});

describe("the Expo module", () => {
  it("is linked on iOS only, under the name the wrapper asks for", () => {
    const config = JSON.parse(read("modules", "octet-live", "expo-module.config.json")) as {
      platforms: string[];
      apple?: { modules?: string[] };
      android?: unknown;
    };
    expect(config.platforms).toEqual(["apple"]);
    expect(config.apple?.modules).toEqual(["OctetLiveModule"]);
    expect(config.android).toBeUndefined();
    expect(moduleSwift).toMatch(/class OctetLiveModule\s*:\s*Module\b/);
    expect(moduleSwift).toMatch(/Name\("OctetLive"\)/);
    expect(read("modules", "octet-live", "src", "OctetLiveModule.ts")).toMatch(
      /requireOptionalNativeModule<[^>]+>\("OctetLive"\)/,
    );
  });

  it("sends the three events the wrapper listens for", () => {
    expect(moduleSwift).toMatch(/Events\("pushToken",\s*"pushToStartToken",\s*"deny"\)/);
  });

  it("requires iOS 17.2", () => {
    expect(podspec).toMatch(/:ios\s*=>\s*'17\.2'/);
  });
});

describe("the widget target", () => {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const target = require("../../targets/octet-live/expo-target.config.js") as {
    type: string;
    name?: string;
    deploymentTarget?: string;
    entitlements?: Record<string, unknown>;
  };

  it("is a widget for iOS 17.2 in the App Group", () => {
    expect(target.type).toBe("widget");
    expect(target.deploymentTarget).toBe("17.2");
    expect(target.entitlements?.["com.apple.security.application-groups"]).toEqual([APP_GROUP]);
  });

  it("draws the activity configured for the attributes type", () => {
    expect(widgetSwift).toMatch(new RegExp(`ActivityConfiguration\\(for: ${ACTIVITY_ATTRIBUTES_TYPE}\\.self\\)`));
    expect(widgetSwift).toMatch(/DynamicIsland\s*\{/);
    for (const region of ["compactLeading", "compactTrailing", "minimal"]) {
      expect(widgetSwift).toContain(`${region}:`);
    }
  });

  it("shows the mascot pose for each kind from the target's assets", () => {
    for (const pose of ["coding", "approval", "success"]) {
      expect(widgetSwift).toContain(`"mascot-${pose}"`);
    }
  });

  it("opens Allow as an approve link, and Deny through the intent", () => {
    expect(widgetSwift).toContain('"octet://approve/');
    expect(widgetSwift).toContain("DenyIntent(mac: state.mac, session: state.session, approval:");
    expect(widgetSwift).toMatch(/Button\(intent: deny\)/);
    expect(denySwift).toMatch(/struct DenyIntent\s*:\s*LiveActivityIntent\b/);
  });
});

describe("the App Group contract", () => {
  it("is the group in app.json, the module, the widget and the intent", () => {
    expect(appJson.expo.ios.entitlements["com.apple.security.application-groups"]).toEqual([APP_GROUP]);
    for (const swift of [moduleSwift, widgetSwift, denySwift]) {
      expect(swift).toContain(`"${APP_GROUP}"`);
    }
  });

  it("keeps the entries under one key the module writes and the widget reads", () => {
    expect(moduleSwift).toContain('"entries"');
    expect(widgetSwift).toContain('"entries"');
  });

  it("hands a deny over under one key and one Darwin notification", () => {
    for (const swift of [moduleSwift, denySwift]) {
      expect(swift).toContain('"denies"');
      expect(swift).toContain('"dev.octet.mobile.deny"');
    }
  });
});
