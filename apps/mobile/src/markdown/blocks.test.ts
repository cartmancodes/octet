import { createOctetStore } from "../store";
import { MAX_BLOCKS, PARSE_CACHE_BYTES, cachedBytes, clearParsed, doneBlocks, streamBlocks } from "./blocks";
import { parse } from "./parse";

afterEach(() => clearParsed());

/** Replies that cross every kind of block, fences with blank lines inside them included. */
const CORPUS = [
  "Hello **world**\n\nSecond paragraph\nstill second\n\n# Title\n\n- a\n- b\n\n  continued\n\n1. one\n2. two\n",
  "```ts\nconst a = 1;\n\n\nconst b = 2;\n```\n\nafter the fence\n\n~~~\nopen fence\n\nnever closed",
  "> quote\nlazy line\n\n> again\n\n```a```\n\ntext\r\n\r\nwindows lines\r\n",
  "````\n```\nnot a close\n\n````\n\n- item\n\n```\n\n\n",
  "",
  "\n\n\n",
];

describe("a streaming reply's blocks", () => {
  it("are the blocks of the whole text, however the text grew", () => {
    for (const text of CORPUS) {
      for (const step of [1, 2, 3, 7, 16]) {
        clearParsed();
        let shown: ReturnType<typeof streamBlocks> | null = null;
        for (let end = 0; end <= text.length; end += step) shown = streamBlocks(text.slice(0, end));
        shown = streamBlocks(text);
        expect([text, step, shown.blocks]).toEqual([text, step, parse(text)]);
        expect(shown.total).toBe(parse(text).length);
      }
    }
  });

  it("parse only what came since the last whole block: a chunk costs the same however long the reply", () => {
    const head = Array.from({ length: 16_000 }, (_, i) => `p${i}`).join("\n\n");
    let text = head;
    streamBlocks(text);
    const started = performance.now();
    let last = streamBlocks(text);
    for (let i = 0; i < 100; i++) {
      text += ` more${i}`;
      last = streamBlocks(text);
    }
    const took = performance.now() - started;
    expect(took).toBeLessThan(200); // a whole parse of the head each chunk takes far longer
    expect(last.total).toBe(16_000);
    expect(last.blocks.length).toBeLessThanOrEqual(MAX_BLOCKS);
    expect(last.more).toBe(16_000 - MAX_BLOCKS);
  });

  it("keep the same drawn blocks while only the hidden tail grows", () => {
    let text = Array.from({ length: 1_000 }, (_, i) => `p${i}`).join("\n\n");
    const before = streamBlocks(text);
    text += "\n\nanother";
    const after = streamBlocks(text);
    expect(after.blocks).toBe(before.blocks);
    expect(after.more).toBe(before.more + 1);
  });

  it("start over when the reply's start was dropped", () => {
    streamBlocks("old start\n\nold middle\n\n");
    expect(streamBlocks("[earlier output is on the Mac]\nnew\n\nend").blocks).toEqual(parse("[earlier output is on the Mac]\nnew\n\nend"));
  });
});

describe("a finished reply's blocks", () => {
  it("draw at most MAX_BLOCKS, and count the rest", () => {
    expect(MAX_BLOCKS).toBe(400);
    const text = "```\n".repeat(16_000);
    const shown = doneBlocks(text);
    expect(shown.total).toBe(parse(text).length);
    expect(shown.blocks).toHaveLength(MAX_BLOCKS);
    expect(shown.more).toBe(shown.total - MAX_BLOCKS);
    const small = doneBlocks("a\n\nb");
    expect(small).toMatchObject({ total: 2, more: 0 });
  });

  it("are parsed once per text", () => {
    expect(doneBlocks("same\n\ntext")).toBe(doneBlocks("same\n\ntext"));
  });

  it(`are cached up to ${PARSE_CACHE_BYTES} bytes in all, and forgotten on a reset`, () => {
    expect(PARSE_CACHE_BYTES).toBe(2 * 1024 * 1024);
    for (let i = 0; i < 64; i++) doneBlocks(`${i}`.padEnd(64 * 1024, "x"));
    expect(cachedBytes()).toBeGreaterThan(0);
    expect(cachedBytes()).toBeLessThanOrEqual(PARSE_CACHE_BYTES);
    // Unpairing (or pairing with another Mac) resets the store, which forgets the cache.
    createOctetStore().getState().reset();
    expect(cachedBytes()).toBe(0);
  });
});
