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

/** The brace-balanced body that follows the first match of `start` (a declaration or a call). */
function block(swift: string, start: RegExp): string {
  const at = start.exec(swift);
  if (!at) return "";
  const open = swift.indexOf("{", at.index + at[0].length - 1);
  let depth = 0;
  for (let i = open; i < swift.length; i++) {
    if (swift[i] === "{") depth++;
    else if (swift[i] === "}" && --depth === 0) return swift.slice(open, i + 1);
  }
  return "";
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

  it("installs where the app does: Expo's own iOS floor, not the Island's", () => {
    const expoCore = readFileSync(path.join(app, "node_modules", "expo-modules-core", "ExpoModulesCore.podspec"), "utf8");
    const floor = /:ios\s*=>\s*'([\d.]+)'/.exec(expoCore)?.[1];
    expect(floor).toBe("16.4");
    expect(/:ios\s*=>\s*'([\d.]+)'/.exec(podspec)?.[1]).toBe(floor);
  });

  it("keeps the activity type behind iOS 17.2, so the compiler guards every ActivityKit call", () => {
    for (const swift of [moduleAttributes, widgetAttributes]) {
      expect(swift).toMatch(/@available\(iOS 17\.2, \*\)\s*\nstruct OctetActivity\b/);
    }
    expect(denySwift).toMatch(/@available\(iOS 17\.2, \*\)\s*\nstruct DenyIntent\b/);
  });

  it("is unsupported below iOS 17.2", () => {
    expect(moduleSwift).toMatch(
      /Function\("isSupported"\) \{ \(\) -> Bool in\s*if #available\(iOS 17\.2, \*\) \{\s*return ActivityAuthorizationInfo\(\)\.areActivitiesEnabled\s*\}\s*return false/,
    );
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

  it("hands each deny over under its own deny.<uuid> key, pinged by one Darwin notification", () => {
    for (const swift of [moduleSwift, denySwift]) {
      expect(swift).toContain('"deny."');
      expect(swift).toContain('"dev.octet.mobile.deny"');
      // No shared list that a read-modify-write could lose a deny from.
      expect(swift).not.toMatch(/array\(forKey:/);
      expect(swift).not.toContain('"denies"');
    }
    expect(denySwift).toContain("UUID().uuidString");
    expect(moduleSwift).toMatch(/hasPrefix\(denyPrefix\)/);
    expect(moduleSwift).toMatch(/removeObject\(forKey: key\)/);
  });
});

describe("what the native side keeps, and how", () => {
  it("refuses entries over 64 KiB, or that aren't an object of at most 64 objects of strings", () => {
    expect(moduleSwift).toMatch(/private let entriesByteLimit = 64 \* 1024\b/);
    expect(moduleSwift).toMatch(/private let entriesLimit = 64\b/);
    const setEntries = block(moduleSwift, /AsyncFunction\("setEntries"\)/);
    expect(setEntries).toMatch(/json\.utf8\.count <= entriesByteLimit/);
    expect(setEntries).toMatch(/as\? \[String: Any\]/);
    expect(setEntries).toMatch(/\.count <= entriesLimit/);
    expect(setEntries).toMatch(/is String/);
    expect(setEntries).toMatch(/throw InvalidEntriesException\(\)/);
    expect(moduleSwift).toMatch(/final class InvalidEntriesException\s*:\s*Exception\b/);
  });

  it("has the widget skip an oversized store and read only its own entry, so one bad entry hides no other", () => {
    expect(widgetSwift).toMatch(/private let entriesByteLimit = 64 \* 1024\b/);
    const entry = block(widgetSwift, /static func entry\(forKey key: String\) -> Entry\?/);
    expect(entry).toMatch(/json\.utf8\.count <= entriesByteLimit/);
    expect(entry).toMatch(/\[key\] as\? \[String: Any\]/);
    expect(widgetSwift).not.toMatch(/decode\(\[String: Entry\]/);
  });

  it("starts and updates only an activity that is active or stale, never one that ended", () => {
    const live = block(moduleSwift, /func liveActivities\(\)/);
    expect(live).toMatch(/\.activityState == \.active \|\| \$0\.activityState == \.stale/);
    expect(block(moduleSwift, /AsyncFunction\("start"\)/)).toMatch(/liveActivities\(\)\.first/);
    expect(block(moduleSwift, /AsyncFunction\("update"\)/)).toMatch(/in liveActivities\(\)/);
    expect(moduleSwift).not.toMatch(/\.activities\.first/);
  });

  it("keeps at most 16 waiting denies, each stamped, and drops any older than 10 minutes", () => {
    const perform = block(denySwift, /func perform\(\)/);
    expect(perform).toMatch(/hasPrefix\("deny\."\)/);
    expect(perform).toMatch(/>= 16/);
    expect(perform).toMatch(/"at": Date\(\)\.timeIntervalSince1970/);
    expect(perform.indexOf(">= 16")).toBeLessThan(perform.indexOf("defaults.set("));
    expect(moduleSwift).toMatch(/private let denyMaxAge: TimeInterval = 10 \* 60\b/);
    expect(block(moduleSwift, /private func drainDenies\(\)/)).toMatch(/denyMaxAge/);
  });

  it("drains on one serial queue, so no key is emitted twice", () => {
    expect(moduleSwift).toMatch(/private let denyQueue = DispatchQueue\(label: "dev\.octet\.mobile\.deny"\)/);
    // drainDenies runs only from the queue.
    const calls = [...moduleSwift.matchAll(/drainDenies\(\)/g)].length;
    const queued = [...moduleSwift.matchAll(/denyQueue\.async \{ \[weak self\] in self\?\.drainDenies\(\) \}/g)].length;
    expect(queued).toBe(1);
    expect(calls).toBe(2); // the declaration and the one queued call
    expect(moduleSwift).toMatch(/scheduleDrain\(\)/);
  });

  it("adds and removes the Darwin observer while holding the lock", () => {
    const hear = block(moduleSwift, /private func hearDenies\(_ on: Bool\)/);
    expect(hear).toMatch(/lock\.lock\(\)\s*\n\s*defer \{ lock\.unlock\(\) \}/);
    expect(hear).toContain("CFNotificationCenterAddObserver");
    expect(hear).toContain("CFNotificationCenterRemoveObserver");
    expect(hear).not.toContain("lock.unlock()\n");
  });

  it("names the activity in every pushToken event, so an old activity's end never clears the live one's token", () => {
    const sends = [...moduleSwift.matchAll(/sendEvent\("pushToken", (\[[^\]]*\])\)/g)].map((m) => m[1]);
    expect(sends.length).toBe(3);
    for (const payload of sends) expect(payload).toMatch(/"activity": activity\.id\]$/);
  });

  it("watches only a live activity, stops on cancel, and forgets each task when its loop ends", () => {
    const watch = block(moduleSwift, /private func watch\(_ activity: Activity<OctetActivity>\)/);
    expect(watch).toMatch(/guard activity\.activityState == \.active \|\| activity\.activityState == \.stale else \{\s*sendEvent\("pushToken", \["token": nil, "activity": activity\.id\]\)/);
    for (const loop of moduleSwift.matchAll(/for await [^\n]*\{\n([^\n]*)/g)) {
      expect(loop[1]).toMatch(/if Task\.isCancelled \{ break \}/);
    }
    expect([...moduleSwift.matchAll(/for await /g)].length).toBe(4);
    expect([...moduleSwift.matchAll(/self\?\.forget\(key, id\)/g)].length).toBe(4);
  });
});

describe("the widget's buttons and links", () => {
  it("percent-encodes every part of the Allow link with the RFC 3986 unreserved set", () => {
    expect(widgetSwift).toContain(
      'CharacterSet(\n  charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")',
    );
    expect(block(widgetSwift, /private func linkPart\(_ text: String\) -> String/)).toContain(
      "addingPercentEncoding(withAllowedCharacters: unreserved)",
    );
    expect(widgetSwift).toMatch(
      /"octet:\/\/approve\/"\s*\+ \[state\.mac, state\.session, approval\]\.map\(linkPart\)\.joined\(separator: "\/"\)/,
    );
    // The link is built in that one place.
    expect([...widgetSwift.matchAll(/octet:\/\/approve/g)].length).toBe(1);
  });

  it("gives Allow and Deny a hit area at least 44 points tall", () => {
    const actions = block(widgetSwift, /private struct Actions: View/);
    expect([...actions.matchAll(/minHeight: 44\b/g)].length).toBe(2);
    expect(block(actions, /Button\(intent: deny\)/)).toMatch(/minHeight: 44\b/);
    expect(block(actions, /Link\(destination: shown\.allowURL\)/)).toMatch(/minHeight: 44\b/);
  });
});
