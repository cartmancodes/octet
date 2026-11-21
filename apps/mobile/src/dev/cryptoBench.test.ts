import { runCryptoBench } from "./cryptoBench";

describe("runCryptoBench", () => {
  it("returns a positive handshake time and rate", () => {
    const { handshakeMs, megabytesPerSecond } = runCryptoBench();
    expect(handshakeMs).toBeGreaterThan(0);
    expect(Number.isFinite(handshakeMs)).toBe(true);
    expect(megabytesPerSecond).toBeGreaterThan(0);
    expect(Number.isFinite(megabytesPerSecond)).toBe(true);
  });
});
