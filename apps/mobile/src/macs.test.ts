import { channelOf } from "./client/fakes";
import { MAC_ID, cleanMacName, isChannelId, keyFor, keyOf, linksOf, machineWords, macOfChannel, newMacId, rawOf, splitKey, tapKey } from "./macs";

describe("session keys", () => {
  it("round-trip a Mac and a raw id", () => {
    const key = keyOf("m1a2b3c4d", "123-456");
    expect(key).toBe("m1a2b3c4d/123-456");
    expect(splitKey(key)).toEqual({ mac: "m1a2b3c4d", raw: "123-456" });
    expect(rawOf(key)).toBe("123-456");
  });

  it("split on the first slash: a raw id may hold one, a Mac id never does", () => {
    const raw = "a/../b?x=1";
    expect(splitKey(keyOf("m2", raw))).toEqual({ mac: "m2", raw });
    expect(splitKey(keyOf("m2", "/lead"))).toEqual({ mac: "m2", raw: "/lead" });
  });

  it("are null for anything that is not a key", () => {
    for (const bad of ["", "123-456", "/123-456", "m1/", "/"]) expect(splitKey(bad)).toBeNull();
    expect(rawOf("123-456")).toBeNull();
  });
});

const STUDIO = channelOf("studio");
const MINI = channelOf("mini");

describe("isChannelId", () => {
  it("is 64 lowercase hex digits, and nothing else", () => {
    expect(isChannelId(STUDIO)).toBe(true);
    expect(isChannelId("0".repeat(64))).toBe(true);
    for (const bad of [
      "",
      STUDIO.toUpperCase(),
      STUDIO.slice(1),
      `${STUDIO}0`,
      `${STUDIO.slice(1)}g`,
      ` ${STUDIO.slice(1)}`,
      `${STUDIO.slice(1)}\n`,
      "studio.tail1.ts.net",
      "wss://relay.example.test/",
    ]) {
      expect(isChannelId(bad)).toBe(false);
    }
  });
});

describe("macOfChannel", () => {
  const pairings = [
    { id: "m00000001", channel: STUDIO },
    { id: "m00000002", channel: MINI },
  ];

  it("is the pairing whose channel id it is", () => {
    expect(macOfChannel(pairings, STUDIO)).toBe("m00000001");
    expect(macOfChannel(pairings, MINI)).toBe("m00000002");
  });

  it("is null for a channel no pairing has, a channel id in another case, a hostname and junk", () => {
    expect(macOfChannel(pairings, channelOf("air"))).toBeNull();
    expect(macOfChannel(pairings, MINI.toUpperCase())).toBeNull();
    expect(macOfChannel(pairings, "studio.tail1.ts.net")).toBeNull();
    expect(macOfChannel(pairings, "")).toBeNull();
    expect(macOfChannel([{ id: "m1", channel: "studio" }], "studio")).toBeNull();
  });
});

describe("keyFor", () => {
  const studio = { id: "m00000001", channel: STUDIO };
  const mini = { id: "m00000002", channel: MINI };

  it("is the phone's key of a raw session on the pairing of the channel the hub sent", () => {
    expect(keyFor([studio, mini], MINI, "41-1700000000")).toBe("m00000002/41-1700000000");
    expect(keyFor([studio, mini], STUDIO, "41-1700000000")).toBe("m00000001/41-1700000000");
  });

  it("names no Mac without a channel, even with only one pairing", () => {
    expect(keyFor([mini], null, "1-2")).toBeNull();
    expect(keyFor([studio, mini], null, "1-2")).toBeNull();
    expect(keyFor([], null, "1-2")).toBeNull();
  });

  it("null for a channel no pairing has, and for a hostname", () => {
    expect(keyFor([studio, mini], channelOf("air"), "1-2")).toBeNull();
    expect(keyFor([studio], "studio.tail1.ts.net", "1-2")).toBeNull();
    expect(keyFor([studio], "", "1-2")).toBeNull();
  });

  it("null for a raw id that is not the hub's: never a key that splits elsewhere", () => {
    for (const raw of ["", "a", "1-2/3", "../1-2", "m00000002/1-2", "1-2 ", `${"1".repeat(128)}-2`]) {
      expect(keyFor([studio], STUDIO, raw)).toBeNull();
    }
  });
});

describe("tap_routes_by_mac", () => {
  const studio = { id: "m00000001", channel: STUDIO };
  const mini = { id: "m00000002", channel: MINI };
  const live = () => false;

  it("a_tap_naming_a_channel_opens_that_macs_session", () => {
    expect(tapKey({ channel: MINI, session: "1-2" }, [studio, mini], live)).toBe("m00000002/1-2");
    expect(tapKey({ channel: STUDIO, session: "1-2" }, [studio, mini], live)).toBe("m00000001/1-2");
  });

  it("a known channel whose session has ended opens the Inbox", () => {
    const ended = (key: string) => key === "m00000002/1-2";
    expect(tapKey({ channel: MINI, session: "1-2" }, [studio, mini], ended)).toBeNull();
    expect(tapKey({ channel: STUDIO, session: "1-2" }, [studio, mini], ended)).toBe("m00000001/1-2");
  });

  it("an unknown channel, or none, opens the Inbox, even with one Mac", () => {
    expect(tapKey({ channel: channelOf("air"), session: "1-2" }, [studio, mini], live)).toBeNull();
    expect(tapKey({ channel: channelOf("air"), session: "1-2" }, [studio], live)).toBeNull();
    expect(tapKey({ channel: null, session: "1-2" }, [studio, mini], live)).toBeNull();
    expect(tapKey({ channel: null, session: "1-2" }, [mini], live)).toBeNull();
  });

  it("no session, or no Mac paired, opens the Inbox", () => {
    expect(tapKey({ channel: MINI, session: null }, [studio, mini], live)).toBeNull();
    expect(tapKey({ channel: MINI, session: "1-2" }, [], live)).toBeNull();
  });
});

describe("machineWords", () => {
  it("online when ready; offline for a pairing with no link; else asleep once tried, connecting… before", () => {
    expect(machineWords("ready", true)).toBe("online");
    expect(machineWords("offline", false)).toBe("offline");
    expect(machineWords("offline", true)).toBe("asleep");
    expect(machineWords("connecting", true)).toBe("asleep");
    expect(machineWords("connecting", false)).toBe("connecting…");
    expect(machineWords("authenticating", false)).toBe("connecting…");
  });
});

describe("linksOf", () => {
  it("names each Mac with a client, in order, and no other", () => {
    const a = { url: "wss://a/" } as never;
    const b = { url: "wss://b/" } as never;
    const links = linksOf(new Map([["m00000001", a], ["m00000002", b]]));
    expect(links.macs()).toEqual(["m00000001", "m00000002"]);
    expect(links.client("m00000002")).toBe(b);
    expect(links.client("m00000003")).toBeNull();
    expect(linksOf(new Map()).macs()).toEqual([]);
  });
});

describe("newMacId", () => {
  afterEach(() => jest.restoreAllMocks());

  it("is m and 8 lowercase hex characters, from the CSPRNG, and not the same twice", () => {
    const spy = jest.spyOn(globalThis.crypto, "getRandomValues");
    const ids = new Set(Array.from({ length: 50 }, () => newMacId()));
    expect(ids.size).toBe(50);
    for (const id of ids) expect(id).toMatch(MAC_ID);
    expect(MAC_ID.test("m0123abcd")).toBe(true);
    for (const bad of ["m0", "m0123ABCD", "x0123abcd", "m0123abcde", "m0123abc/"]) expect(MAC_ID.test(bad)).toBe(false);
    expect(spy).toHaveBeenCalledTimes(50);
  });

  it("falls back to the native UUID, then to time and Math.random, where the runtime has no getRandomValues", () => {
    const g = globalThis as { crypto?: unknown; expo?: { uuidv4?: () => string } };
    const crypto = g.crypto;
    const uuid = jest.fn(() => "9F86D081-884C-4D63-9E8B-0123456789AB");
    const expo = g.expo;
    try {
      Object.defineProperty(globalThis, "crypto", { value: {}, configurable: true });
      g.expo = { uuidv4: uuid };
      expect(newMacId()).toBe("m9f86d081");
      // With neither, time, a counter and Math.random still make a well-formed id, never the
      // same one twice in a row, and never a throw.
      g.expo = {};
      const ids = Array.from({ length: 20 }, () => newMacId());
      for (const id of ids) expect(id).toMatch(MAC_ID);
      expect(new Set(ids).size).toBe(20);
      jest.spyOn(Date, "now").mockReturnValue(5);
      jest.spyOn(Math, "random").mockReturnValue(0.5);
      expect(newMacId()).not.toBe(newMacId()); // the counter alone tells them apart
    } finally {
      Object.defineProperty(globalThis, "crypto", { value: crypto, configurable: true });
      g.expo = expo;
    }
  });
});

describe("cleanMacName", () => {
  it("drops control and bidi characters, folds spaces, and keeps 40 characters", () => {
    expect(cleanMacName("  Studio\u0007 \n Mac ")).toBe("Studio Mac");
    expect(cleanMacName("a\u202Eb")).toBe("ab");
    expect(cleanMacName("z".repeat(41))).toBe("z".repeat(40));
    expect(cleanMacName("   ")).toBeNull();
    expect(cleanMacName(5)).toBeNull();
  });
});
