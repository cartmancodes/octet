import { MAC_ID, cleanMacName, keyOf, linksOf, macNameOf, macOfHost, newMacId, rawOf, splitKey } from "./macs";

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

describe("macOfHost", () => {
  const pairings = [
    { id: "m00000001", hubUrl: "wss://Studio.tail1.ts.net/" },
    { id: "m00000002", hubUrl: "wss://laptop.tail1.ts.net:8443/" },
  ];

  it("finds the pairing whose hub host matches, ignoring case", () => {
    expect(macOfHost(pairings, "studio.tail1.ts.net")).toBe("m00000001");
    expect(macOfHost(pairings, "STUDIO.TAIL1.TS.NET")).toBe("m00000001");
    expect(macOfHost(pairings, "Laptop.Tail1.ts.net")).toBe("m00000002");
  });

  it("is null for a host no pairing has, and for junk", () => {
    expect(macOfHost(pairings, "other.tail1.ts.net")).toBeNull();
    expect(macOfHost(pairings, "")).toBeNull();
    expect(macOfHost(pairings, "studio")).toBeNull();
    expect(macOfHost([{ id: "m1", hubUrl: "not a url" }], "not a url")).toBeNull();
  });
});

describe("macNameOf", () => {
  it("is the first DNS label of the hub host", () => {
    expect(macNameOf("wss://studio.tail1.ts.net/")).toBe("studio");
    expect(macNameOf("wss://studio.tail1.ts.net/hub?x=1")).toBe("studio");
    expect(macNameOf("wss://studio-mac.tail1234.ts.net:8443/")).toBe("studio-mac");
  });

  it("keeps an IP address whole, and is Mac with no address", () => {
    expect(macNameOf("ws://100.64.0.7:7777/")).toBe("100.64.0.7");
    expect(macNameOf(null)).toBe("Mac");
    expect(macNameOf("")).toBe("Mac");
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
