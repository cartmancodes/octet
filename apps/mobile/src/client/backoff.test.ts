import { Backoff } from "./backoff";

describe("Backoff", () => {
  it("doubles from 0.5 s to a 30 s ceiling", () => {
    const backoff = new Backoff(() => 1);
    const delays = Array.from({ length: 9 }, () => backoff.next());
    expect(delays).toEqual([500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
  });

  it("jitters each delay down to half of it", () => {
    expect(new Backoff(() => 0).next()).toBe(250);
    const backoff = new Backoff(() => 0.5);
    expect([backoff.next(), backoff.next()]).toEqual([375, 750]);
  });

  it("never leaves the 0.25 s to 30 s band, whatever the dice say", () => {
    const backoff = new Backoff();
    for (let i = 0; i < 200; i++) {
      const delay = backoff.next();
      expect(delay).toBeGreaterThanOrEqual(250);
      expect(delay).toBeLessThanOrEqual(30000);
    }
  });

  it("starts over after reset", () => {
    const backoff = new Backoff(() => 1);
    backoff.next();
    backoff.next();
    backoff.reset();
    expect(backoff.next()).toBe(500);
  });
});
