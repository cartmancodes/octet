import { TEXT, field, sanitize } from "./text";

describe("cleaning hub text", () => {
  it("drops the directional marks, which reorder what the phone draws (1‏ 22 shows as 22 1)", () => {
    expect(sanitize("kill 1‏ 22", TEXT).out).toBe("kill 1 22");
    expect(sanitize("a‎b؜c", TEXT).out).toBe("abc");
    expect(field("rm -rf‏ /tmp‎/x؜", 100)).toBe("rm -rf /tmp/x");
  });

  it("drops zero-width characters, which can pad a command with lines that look empty", () => {
    expect(sanitize("a\u200bb\u200cc\u200dd\u2060e\ufefff", TEXT).out).toBe("abcdef");
    expect(field("rm\u200b -rf", 100)).toBe("rm -rf");
  });

  it("still drops the overrides and isolates", () => {
    expect(sanitize("a‮b⁦c⁩", TEXT).out).toBe("abc");
  });
});
