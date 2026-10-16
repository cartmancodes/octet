import { linksOf } from "../macs";
import { sendTo, utf8Bytes } from "./send";

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

describe("sendTo", () => {
  const client = () => ({ request: jest.fn(async (body: unknown) => ({ type: "ack", body })) });

  it("sends a body with no session to the Mac named, and one with a session to its Mac, the key stripped", async () => {
    const a = client();
    const b = client();
    const links = linksOf(new Map([["m1", a], ["m2", b]] as never));
    await sendTo(links, "m2", { type: "recent" });
    expect(b.request).toHaveBeenCalledWith({ type: "recent" });
    await sendTo(links, "m2", { type: "prompt", session: "m2/x", text: "hi" });
    expect(b.request).toHaveBeenLastCalledWith({ type: "prompt", session: "x", text: "hi" });
    expect(a.request).not.toHaveBeenCalled();
  });

  it("refuses, unsent, a session of another Mac, and a Mac with no link", async () => {
    const a = client();
    const links = linksOf(new Map([["m1", a]] as never));
    await expect(sendTo(links, "m1", { type: "prompt", session: "m2/x", text: "hi" })).rejects.toMatchObject({ code: "unsent" });
    await expect(sendTo(links, "m9", { type: "recent" })).rejects.toMatchObject({ code: "unsent" });
    expect(a.request).not.toHaveBeenCalled();
  });
});
