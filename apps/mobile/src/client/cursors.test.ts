import { Cursors } from "./cursors";

describe("Cursors", () => {
  it("accepts a (journal, seq) once and remembers the last as the cursor", () => {
    const cursors = new Cursors();
    expect(cursors.cursor("s")).toBeNull();
    expect(cursors.accept("s", "a.jsonl", 1)).toBe(true);
    expect(cursors.accept("s", "a.jsonl", 2)).toBe(true);
    expect(cursors.accept("s", "a.jsonl", 2)).toBe(false);
    expect(cursors.accept("s", "a.jsonl", 1)).toBe(false);
    expect(cursors.cursor("s")).toEqual({ journal: "a.jsonl", seq: 2 });
  });

  it("keeps sessions apart", () => {
    const cursors = new Cursors();
    cursors.accept("s", "a", 5);
    expect(cursors.accept("t", "a", 5)).toBe(true);
    expect(cursors.cursor("t")).toEqual({ journal: "a", seq: 5 });
  });

  it("starts at seq 0 and tolerates gaps", () => {
    const cursors = new Cursors();
    expect(cursors.accept("s", "a", 0)).toBe(true);
    expect(cursors.accept("s", "a", 7)).toBe(true);
    expect(cursors.accept("s", "a", 3)).toBe(false);
  });

  it("keeps the cursor on the newest journal when a late record of an older one arrives once", () => {
    const cursors = new Cursors();
    cursors.apply("s", "start", ["a", "b"]);
    cursors.accept("s", "a", 4);
    cursors.accept("s", "b", 1);
    expect(cursors.accept("s", "a", 5)).toBe(true); // the old session's last words
    expect(cursors.accept("s", "a", 5)).toBe(false);
    expect(cursors.cursor("s")).toEqual({ journal: "b", seq: 1 });
  });

  it("treats a journal it was never told of as the newest", () => {
    const cursors = new Cursors();
    cursors.apply("s", "cursor", ["a"]);
    cursors.accept("s", "a", 9);
    expect(cursors.accept("s", "b", 1)).toBe(true);
    expect(cursors.cursor("s")).toEqual({ journal: "b", seq: 1 });
    expect(cursors.accept("s", "a", 10)).toBe(true);
    expect(cursors.cursor("s")).toEqual({ journal: "b", seq: 1 });
  });

  it("restores a saved cursor and drops what it covers", () => {
    const cursors = new Cursors({ s: { journal: "b", seq: 8 } });
    expect(cursors.cursor("s")).toEqual({ journal: "b", seq: 8 });
    expect(cursors.accept("s", "b", 8)).toBe(false);
    expect(cursors.accept("s", "b", 9)).toBe(true);
  });

  it("a replay from the start clears the cursor and the dedupe state", () => {
    const cursors = new Cursors();
    cursors.accept("s", "a", 3);
    cursors.apply("s", "start", ["a"]);
    expect(cursors.cursor("s")).toBeNull();
    expect(cursors.accept("s", "a", 1)).toBe(true);
  });

  it("a replay from the cursor changes nothing but forgets journals that are gone", () => {
    const cursors = new Cursors();
    cursors.apply("s", "start", ["a", "b"]);
    cursors.accept("s", "a", 3);
    cursors.accept("s", "b", 2);
    cursors.apply("s", "cursor", ["b", "c"]);
    expect(cursors.cursor("s")).toEqual({ journal: "b", seq: 2 });
    expect(cursors.accept("s", "b", 2)).toBe(false);
    expect(cursors.size("s")).toBe(1);
  });

  it("holds a bounded number of journals per session", () => {
    const cursors = new Cursors();
    for (let i = 0; i < 500; i++) cursors.accept("s", `j${i}`, 1);
    expect(cursors.size("s")).toBeLessThanOrEqual(64);
    expect(cursors.cursor("s")).toEqual({ journal: "j499", seq: 1 });
  });

  it("forgets a session", () => {
    const cursors = new Cursors();
    cursors.accept("s", "a", 1);
    cursors.forget("s");
    expect(cursors.cursor("s")).toBeNull();
    expect(cursors.snapshot()).toEqual({});
  });
});
