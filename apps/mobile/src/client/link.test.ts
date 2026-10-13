import { hubHost, isHubUrl, parsePairLink } from "./link";

const SECRET = "0123456789abcdef".repeat(4);

describe("parsePairLink", () => {
  it("parse_pair_link_accepts_only_https_with_a_hex_secret", () => {
    expect(parsePairLink(`https://mac.tail1234.ts.net/#pair=${SECRET}`)).toEqual({
      url: "wss://mac.tail1234.ts.net/",
      secret: SECRET,
    });
    // No path slash, surrounding whitespace, a port, an upper-case host and secret.
    expect(parsePairLink(`  https://mac.example.ts.net#pair=${SECRET}\n`)?.url).toBe("wss://mac.example.ts.net/");
    expect(parsePairLink(`https://100.64.0.7:8443/#pair=${SECRET}`)?.url).toBe("wss://100.64.0.7:8443/");
    expect(parsePairLink(`https://MAC.Example.NET/#pair=${SECRET.toUpperCase()}`)).toEqual({
      url: "wss://mac.example.net/",
      secret: SECRET,
    });

    const refused = [
      `http://mac.ts.net/#pair=${SECRET}`, // not https
      `ws://mac.ts.net/#pair=${SECRET}`,
      `wss://mac.ts.net/#pair=${SECRET}`,
      `octet://mac.ts.net/#pair=${SECRET}`,
      `javascript:alert(1)//https://mac.ts.net/#pair=${SECRET}`,
      `https://mac.ts.net/#pair=${SECRET.slice(1)}`, // 63 digits
      `https://mac.ts.net/#pair=${SECRET}0`, // 65 digits
      `https://mac.ts.net/#pair=${"g".repeat(64)}`, // not hex
      `https://mac.ts.net/#pair=`,
      `https://mac.ts.net/#${SECRET}`,
      `https://mac.ts.net/`,
      `https://mac.ts.net/#pair=${SECRET}&x=1`,
      `https://mac.ts.net/?a=b#pair=${SECRET}`,
      `https://mac.ts.net/path#pair=${SECRET}`,
      `https://user:pw@mac.ts.net/#pair=${SECRET}`,
      `https://mac.ts.net@evil.example/#pair=${SECRET}`,
      `https:///#pair=${SECRET}`,
      `https://mac..ts.net/#pair=${SECRET}`,
      `https://-mac.ts.net/#pair=${SECRET}`,
      `https://mac.ts.net:0/#pair=${SECRET}`,
      `https://mac.ts.net:99999/#pair=${SECRET}`,
      `https://mac.ts.net/ x#pair=${SECRET}`,
      `see https://mac.ts.net/#pair=${SECRET}`,
      "",
    ];
    for (const text of refused) expect([text, parsePairLink(text)]).toEqual([text, null]);
  });
});

describe("isHubUrl", () => {
  it("accepts what parsePairLink makes, and nothing else", () => {
    for (const link of [
      `https://mac.tail1234.ts.net/#pair=${SECRET}`,
      `https://100.64.0.7:8443/#pair=${SECRET}`,
      `https://MAC.example.net:0443#pair=${SECRET}`,
    ]) {
      const url = parsePairLink(link)?.url ?? "";
      expect(isHubUrl(url)).toBe(true);
    }
    const refused = [
      "",
      "https://mac.ts.net/",
      "ws://mac.ts.net/",
      "wss://mac.ts.net", // no trailing slash
      "wss://mac.ts.net/path",
      "wss://mac.ts.net/?x=1",
      "wss://user@mac.ts.net/",
      "wss://mac.ts.net:0/",
      "wss://mac.ts.net:70000/",
      "wss://MAC.ts.net/", // not the saved form
      "wss://mac.ts.net:08443/",
      "wss://-bad.ts.net/",
      "wss://mac.ts.net/\n",
      "javascript:alert(1)",
    ];
    for (const url of refused) expect(isHubUrl(url)).toBe(false);
  });

  it("hubHost shows the host and port", () => {
    expect(hubHost("wss://mac.tail1234.ts.net/")).toBe("mac.tail1234.ts.net");
    expect(hubHost("wss://100.64.0.7:8443/")).toBe("100.64.0.7:8443");
  });
});
