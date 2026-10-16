import { SOLO_MAC, keyOf, macNameOf, macOfHost, rawOf, soloLinks, splitKey } from "./macs";

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

  it("use the one Mac's id until pairings give real ones", () => {
    expect(SOLO_MAC).toBe("m0");
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

describe("soloLinks", () => {
  it("names the one Mac while there is a client, and none without", () => {
    const client = { url: "wss://x/" } as never;
    const links = soloLinks(client);
    expect(links.macs()).toEqual([SOLO_MAC]);
    expect(links.client(SOLO_MAC)).toBe(client);
    expect(links.client("m1")).toBeNull();
    expect(soloLinks(null).macs()).toEqual([]);
    expect(soloLinks(null).client(SOLO_MAC)).toBeNull();
  });
});
