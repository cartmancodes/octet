import { utf8Bytes } from "./send";

describe("utf8Bytes", () => {
  it("counts each width", () => {
    expect(utf8Bytes("a")).toBe(1);
    expect(utf8Bytes("é")).toBe(2);
    expect(utf8Bytes("€")).toBe(3);
    expect(utf8Bytes("😀")).toBe(4); // a surrogate pair
  });

  it("counts a lone surrogate as 3 bytes (as TextEncoder's U+FFFD) and does not skip the unit after it", () => {
    expect(utf8Bytes("\ud800")).toBe(3);
    expect(utf8Bytes("\udc00")).toBe(3);
    expect(utf8Bytes("\ud800é")).toBe(3 + 2);
    expect(utf8Bytes("\ud800\ud800")).toBe(6);
    expect(utf8Bytes("\udc00𐀀")).toBe(3 + 4);
    for (const text of ["\ud800é", "\ud800\ud800", "a\udc00b", "x\ud83dy😀"]) {
      expect(utf8Bytes(text)).toBe(new TextEncoder().encode(text).length);
    }
  });
});
