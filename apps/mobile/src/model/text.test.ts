import { TEXT, field, sanitize } from "./text";

describe("cleaning hub text", () => {
  it("drops the directional marks, which reorder what the phone draws (1‏ 22 shows as 22 1)", () => {
    expect(sanitize("kill 1‏ 22", TEXT).out).toBe("kill 1 22");
    expect(sanitize("a‎b؜c", TEXT).out).toBe("abc");
    expect(field("rm -rf‏ /tmp‎/x؜", 100)).toBe("rm -rf /tmp/x");
  });

  it("drops zero-width characters, which can pad a command with lines that look empty", () => {
    expect(sanitize("a\u200bb\u2060c\ufeffd", TEXT).out).toBe("abcd");
    expect(field("rm\u200b -rf", 100)).toBe("rm -rf");
  });

  it("keeps the joiners that emoji and scripts need (ZWJ, ZWNJ)", () => {
    const family = "\u{1F468}\u200d\u{1F469}\u200d\u{1F467}";
    expect(sanitize(family, TEXT).out).toBe(family);
    const persian = "\u0645\u06cc\u200c\u062e\u0648\u0627\u0647\u0645";
    expect(sanitize(persian, TEXT).out).toBe(persian);
  });

  it("still drops the overrides and isolates", () => {
    expect(sanitize("a‮b⁦c⁩", TEXT).out).toBe("abc");
  });

  it("turns line and paragraph separators and lone carriage returns into newlines, and CRLF into one", () => {
    expect(sanitize("a\u2028b\u2029c\rd\r\ne", TEXT).out).toBe("a\nb\nc\nd\ne");
    expect(sanitize("\r\r\n", TEXT).out).toBe("\n\n");
    expect(field(`ls${"\u2028".repeat(15)}rm -rf /`, 100)).toBe(`ls${"\n".repeat(15)}rm -rf /`);
  });

  it("keeps CRLF as one newline when a stream splits it", () => {
    const first = sanitize("a\r", TEXT);
    expect(first.out).toBe("a\n");
    expect(sanitize("\nb", first.esc).out).toBe("b");
    expect(sanitize("c", sanitize("\r", TEXT).esc).out).toBe("c");
  });
});
