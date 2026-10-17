// The Live Activity wrapper in jest: there is no native module here, exactly as on Android and in
// Expo Go, so every call must be a safe no-op. Nothing native is mocked.
import { readFileSync } from "node:fs";
import path from "node:path";

import {
  ACTIVITY_ATTRIBUTES_TYPE,
  ACTIVITY_KINDS,
  ACTIVITY_STATE_FIELDS,
  APP_GROUP,
  addDenyListener,
  addPushToStartTokenListener,
  addPushTokenListener,
  asDenyEvent,
  asTokenEvent,
  end,
  entryKey,
  isSupported,
  setEntries,
  start,
  update,
  type ActivityState,
  type DenyEvent,
} from "./index";
import native from "./src/OctetLiveModule";

const state: ActivityState = {
  kind: "approval",
  session: "6101-1790000000",
  mac: "mac.tail1234.ts.net",
  since: 1790000059,
  approvals: 2,
};

describe("the wrapper without a native module", () => {
  it("finds no native module here, so these tests exercise the fallback", () => {
    expect(native).toBeNull();
  });

  it("is not supported", () => {
    expect(isSupported()).toBe(false);
  });

  it("starts, updates and ends as no-ops that resolve", async () => {
    await expect(start(state)).resolves.toBeUndefined();
    await expect(update({ ...state, kind: "running", approvals: 0 })).resolves.toBeUndefined();
    await expect(end()).resolves.toBeUndefined();
  });

  it("writes no App Group entries, and resolves", async () => {
    await expect(
      setEntries({ [entryKey(state.mac, state.session)]: { name: "fix the build", command: "npm test", approval: "a1" } }),
    ).resolves.toBeUndefined();
    await expect(setEntries({})).resolves.toBeUndefined();
  });

  it("subscribes to nothing, and every remover does nothing", () => {
    const heard = jest.fn();
    const subs = [addPushTokenListener(heard), addPushToStartTokenListener(heard), addDenyListener(heard)];
    for (const sub of subs) {
      expect(() => sub.remove()).not.toThrow();
      expect(() => sub.remove()).not.toThrow();
    }
    expect(heard).not.toHaveBeenCalled();
  });
});

describe("the App Group key", () => {
  it("is the content state's mac, a slash, and its raw session", () => {
    expect(entryKey("mac.tail1234.ts.net", "6101-1790000000")).toBe("mac.tail1234.ts.net/6101-1790000000");
  });

  it("lowercases the host and drops a trailing dot, as the widget does, so a host's case loses nothing", () => {
    expect(entryKey("Mac.Tail1234.TS.net.", "6101-1790000000")).toBe("mac.tail1234.ts.net/6101-1790000000");
  });

  it("is a slash and the session when the hub published no host", () => {
    expect(entryKey("", "6101-1790000000")).toBe("/6101-1790000000");
  });

  it("is the group the app and the widget share", () => {
    expect(APP_GROUP).toBe("group.dev.octet.mobile");
  });
});

describe("events from the native side", () => {
  it("passes a deny with a host, a raw session and an approval id", () => {
    const deny: DenyEvent = { mac: "mac.tail1234.ts.net", session: "6101-1790000000", approval: "a1" };
    expect(asDenyEvent({ ...deny })).toEqual(deny);
    expect(asDenyEvent({ ...deny, mac: "" })).toEqual({ ...deny, mac: "" });
  });

  it("drops a deny that is malformed or names no session or approval", () => {
    for (const bad of [
      null,
      undefined,
      "deny",
      {},
      { mac: "m", session: "s" },
      { mac: "m", session: "", approval: "a" },
      { mac: "m", session: "s", approval: "" },
      { mac: 1, session: "s", approval: "a" },
      { mac: "m", session: "s", approval: 7 },
    ]) {
      expect(asDenyEvent(bad)).toBeNull();
    }
  });

  it("passes a lowercase hex token, or null for a token that is gone, and drops anything else", () => {
    expect(asTokenEvent({ token: "0a1b2c" })).toEqual({ token: "0a1b2c" });
    expect(asTokenEvent({ token: null })).toEqual({ token: null });
    // Which activity it is: the token of one activity, or its end, never another's.
    expect(asTokenEvent({ token: "0a1b2c", activity: "A1B2-C3" })).toEqual({ token: "0a1b2c", activity: "A1B2-C3" });
    expect(asTokenEvent({ token: null, activity: "A1B2-C3" })).toEqual({ token: null, activity: "A1B2-C3" });
    expect(asTokenEvent({ token: "0a1b2c", activity: 5 })).toEqual({ token: "0a1b2c" });
    expect(asTokenEvent({ token: "0a1b2c", activity: "x".repeat(129) })).toEqual({ token: "0a1b2c" });
    // iOS 26.5 issues 128-byte tokens (256 hex digits); any up to 1024 digits passes.
    const ios265 = "0123456789abcdef".repeat(16);
    expect(ios265).toHaveLength(256);
    expect(asTokenEvent({ token: ios265, activity: "A1B2-C3" })).toEqual({ token: ios265, activity: "A1B2-C3" });
    expect(asTokenEvent({ token: "a".repeat(1024) })).toEqual({ token: "a".repeat(1024) });
    for (const bad of [null, {}, { token: "" }, { token: "0A1B" }, { token: "xyz" }, { token: 5 }, { token: "a".repeat(1025) }]) {
      expect(asTokenEvent(bad)).toBeNull();
    }
  });
});

describe("the content state matches the hub's payloads", () => {
  // The hub's own fixture (crates/octet-hub/tests/data/activity-payload.json), read from disk so a
  // change on either side fails here.
  const repo = path.resolve(__dirname, "..", "..", "..", "..");
  type Aps = { "content-state": Record<string, unknown>; "attributes-type"?: unknown; attributes?: unknown };
  const payloads = JSON.parse(
    readFileSync(path.join(repo, "crates", "octet-hub", "tests", "data", "activity-payload.json"), "utf8"),
  ) as Record<string, { aps: Aps }>;

  it("has a start, an update and an end", () => {
    expect(Object.keys(payloads).sort()).toEqual(["end", "start", "update"]);
  });

  it.each(["start", "update", "end"])("%s carries exactly the wrapper's fields, with the same types", (event) => {
    const contentState = payloads[event]?.aps["content-state"] ?? {};
    expect(Object.keys(contentState).sort()).toEqual(Object.keys(ACTIVITY_STATE_FIELDS).sort());
    for (const [field, type] of Object.entries(ACTIVITY_STATE_FIELDS)) {
      expect([field, typeof contentState[field]]).toEqual([field, type]);
    }
    expect(ACTIVITY_KINDS).toContain(contentState.kind);
  });

  it("starts the attributes type the widget declares, with no attributes", () => {
    expect(payloads.start?.aps["attributes-type"]).toBe(ACTIVITY_ATTRIBUTES_TYPE);
    expect(ACTIVITY_ATTRIBUTES_TYPE).toBe("OctetActivity");
    expect(payloads.start?.aps.attributes).toEqual({});
  });

  it("names the kinds running, approval and done", () => {
    expect([...ACTIVITY_KINDS].sort()).toEqual(["approval", "done", "running"]);
  });
});
