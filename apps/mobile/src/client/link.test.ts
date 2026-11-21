import { NOT_A_LINK, fromHex, isRelayUrl, pairLinkProblem, parsePairLink, toHex } from "./link";

const HUB_KEY = "07".repeat(32);
const CHANNEL = "0a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728ff";
const SECRET = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

/** A pairing link as the hub makes it (crates/octet-hub/src/link.rs): `octet://pair/` and the
 * unpadded base64url of the version byte, the hub's key, the channel, the secret and the relay's
 * address. */
function link(address: string, version = 1, parts: { hubKey?: string; channel?: string; secret?: string } = {}): string {
  const payload = Buffer.concat([
    Buffer.of(version),
    Buffer.from(parts.hubKey ?? HUB_KEY, "hex"),
    Buffer.from(parts.channel ?? CHANNEL, "hex"),
    Buffer.from(parts.secret ?? SECRET, "hex"),
    Buffer.from(address, "latin1"),
  ]);
  return `octet://pair/${payload.toString("base64url")}`;
}

describe("parsePairLink", () => {
  it("a_good_link_with_and_without_a_port", () => {
    expect(parsePairLink(link("relay.example"), false)).toEqual({
      relay: "wss://relay.example/",
      hubKey: HUB_KEY,
      channel: CHANNEL,
      secret: SECRET,
    });
    expect(parsePairLink(link("relay.example:8443"), false)?.relay).toBe("wss://relay.example:8443/");
    // Surrounding whitespace (a paste) and an upper-case host, which is kept as the lower case.
    expect(parsePairLink(`  ${link("Relay.Example")}\n`, false)?.relay).toBe("wss://relay.example/");
    expect(pairLinkProblem(link("relay.example"), false)).toBeNull();
  });

  it("version_2_is_refused", () => {
    expect(parsePairLink(link("relay.example", 2), false)).toBeNull();
    expect(parsePairLink(link("relay.example", 2), true)).toBeNull();
    expect(pairLinkProblem(link("relay.example", 2), false)).toBe(NOT_A_LINK);
  });

  it("97_bytes_with_no_address_are_refused", () => {
    expect(parsePairLink(link(""), false)).toBeNull();
    expect(parsePairLink(link(""), true)).toBeNull();
  });

  it("an_address_with_a_path_a_space_or_254_bytes_is_refused", () => {
    const labels = (count: number) => Array.from({ length: count }, () => "a".repeat(62)).join(".");
    const longest = `${labels(4)}.${"b".repeat(253 - 4 * 63)}`;
    expect(longest).toHaveLength(253);
    expect(parsePairLink(link(longest), false)?.relay).toBe(`wss://${longest}/`);
    for (const address of [
      "relay.example/path",
      "relay.example/",
      "re lay.example",
      "relay.example ",
      `${longest}b`, // 254 bytes
      "relay.example?x=1",
      "relay.example#top",
      "me@relay.example",
      "relay.example:",
      "relay.example:0",
      "relay.example:65536",
      "relay.example:80:80",
      "relay.example:+80",
      "[::1]:8443",
      "relay..example",
      "-relay.example",
      "relay-.example",
      `${"a".repeat(64)}.example`,
      "relay.exämple",
    ]) {
      expect([address, parsePairLink(link(address), true)]).toEqual([address, null]);
    }
  });

  it("an_https_link_is_refused", () => {
    for (const text of [
      `https://relay.example/#pair=${SECRET}`,
      `https://mac.tail1234.ts.net/#pair=${SECRET}`,
      link("relay.example").replace("octet://", "https://"),
      link("relay.example").replace("octet://pair/", "octet://approve/"),
      link("relay.example").replace("octet://pair/", "octet:/pair/"),
      `see ${link("relay.example")}`,
      "",
      "not a link",
    ]) {
      expect([text, parsePairLink(text, true)]).toEqual([text, null]);
      expect([text, pairLinkProblem(text, true)]).toEqual([text, NOT_A_LINK]);
    }
    expect(NOT_A_LINK).toBe("That isn't an Octet pairing link");
  });

  it("a_version_0_link_is_taken_only_when_development", () => {
    const plain = link("127.0.0.1:9000", 0);
    expect(parsePairLink(plain, false)).toBeNull();
    expect(pairLinkProblem(plain, false)).toBe(NOT_A_LINK);
    expect(parsePairLink(plain, true)).toEqual({ relay: "ws://127.0.0.1:9000/", hubKey: HUB_KEY, channel: CHANNEL, secret: SECRET });
    // A version 1 link is wss whichever build reads it.
    expect(parsePairLink(link("127.0.0.1:9000"), true)?.relay).toBe("wss://127.0.0.1:9000/");
  });

  it("takes only the canonical unpadded base64url", () => {
    const good = link("relay.example");
    const body = good.slice("octet://pair/".length);
    const standard = Buffer.from(body, "base64url").toString("base64");
    for (const text of [
      `${good}=`, // padding
      `octet://pair/${standard}`, // + and / in place of - and _ (and padding), when there are any
      `${good.slice(0, -1)}`, // cut short: a byte lost
      `${good}A`, // a dangling sextet
      `${good.slice(0, 40)} ${good.slice(40)}`, // a space inside
      `${good.slice(0, 40)}.${good.slice(40)}`,
    ]) {
      if (text === `octet://pair/${standard}` && standard === body) continue;
      expect(parsePairLink(text, true)).toBeNull();
    }
    // Trailing bits that are not zero: the same bytes would decode, but the text is not the one the
    // hub makes.
    const last = body.at(-1) ?? "A";
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    const bits = (body.length * 6) % 8; // the bits of the last sextet past the last whole byte
    if (bits > 0) {
      const bumped = alphabet[alphabet.indexOf(last) + 1] ?? "A";
      expect(parsePairLink(`octet://pair/${body.slice(0, -1)}${bumped}`, true)).toBeNull();
    }
  });

  it("gives the parts as lowercase hex, whatever bytes they are", () => {
    const parts = { hubKey: "ab".repeat(32), channel: "00".repeat(32), secret: "fe".repeat(32) };
    expect(parsePairLink(link("relay.example", 1, parts), false)).toEqual({ relay: "wss://relay.example/", ...parts });
  });
});

describe("isRelayUrl", () => {
  it("accepts what parsePairLink makes, and nothing else", () => {
    for (const address of ["relay.example", "relay.example:8443", "Relay.Example:0443"]) {
      expect(isRelayUrl(parsePairLink(link(address), false)?.relay ?? "", false)).toBe(true);
    }
    const plain = parsePairLink(link("127.0.0.1:9000", 0), true)?.relay ?? "";
    expect(isRelayUrl(plain, true)).toBe(true);
    expect(isRelayUrl(plain, false)).toBe(false);
    for (const url of [
      "",
      "https://relay.example/",
      "wss://relay.example", // no trailing slash
      "wss://relay.example/path",
      "wss://relay.example/?x=1",
      "wss://user@relay.example/",
      "wss://relay.example:0/",
      "wss://relay.example:70000/",
      "wss://RELAY.example/", // not the saved form
      "wss://relay.example:08443/",
      "wss://-bad.example/",
      "wss://relay.example/\n",
      "javascript:alert(1)",
    ]) {
      expect([url, isRelayUrl(url, true)]).toEqual([url, false]);
    }
  });
});

describe("hex", () => {
  it("round-trips, and takes only lowercase hex of whole bytes", () => {
    const bytes = Uint8Array.of(0, 1, 0xab, 0xff);
    expect(toHex(bytes)).toBe("0001abff");
    expect(fromHex("0001abff")).toEqual(bytes);
    for (const bad of ["0", "0g", "AB", " 00", "0x00"]) expect(fromHex(bad)).toBeNull();
    expect(fromHex("")).toEqual(new Uint8Array(0));
  });
});
