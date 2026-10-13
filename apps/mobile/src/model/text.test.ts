import { TEXT, field, sanitize } from "./text";

describe("cleaning hub text", () => {
  it("drops the directional marks, which reorder what the phone draws (1‏ 22 shows as 22 1)", () => {
    expect(sanitize("kill 1‏ 22", TEXT).out).toBe("kill 1 22");
    expect(sanitize("a‎b؜c", TEXT).out).toBe("abc");
    expect(field("rm -rf‏ /tmp‎/x؜", 100)).toBe("rm -rf /tmp/x");
  });

  it("still drops the overrides and isolates", () => {
    expect(sanitize("a‮b⁦c⁩", TEXT).out).toBe("abc");
  });
});
