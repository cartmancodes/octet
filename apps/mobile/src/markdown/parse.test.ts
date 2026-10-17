import { inline, parse, type Block, type Span } from "./parse";

const p = (...spans: Span[]): Block => ({ type: "paragraph", spans });
const t = (text: string, extra: Omit<Span, "text"> = {}): Span => ({ text, ...extra });

/** Milliseconds `fn` takes, the best of a few runs (a cold JIT is not the parser's cost). */
function timed(fn: () => void): number {
  let best = Infinity;
  for (let i = 0; i < 3; i++) {
    const start = performance.now();
    fn();
    best = Math.min(best, performance.now() - start);
  }
  return best;
}

describe("markdown_is_inert", () => {
  describe("blocks", () => {
    it("splits paragraphs on blank lines and keeps line breaks inside one", () => {
      expect(parse("one\ntwo\n\nthree")).toEqual([p(t("one\ntwo")), p(t("three"))]);
    });

    it("reads headings of one to six hashes followed by a space", () => {
      expect(parse("# Title\n### Sub *x*\n####### seven\n#tag")).toEqual([
        { type: "heading", level: 1, spans: [t("Title")] },
        { type: "heading", level: 3, spans: [t("Sub "), t("x", { italic: true })] },
        p(t("####### seven\n#tag")),
      ]);
    });

    it("drops a heading's closing hashes", () => {
      expect(parse("## Title ##\n# C#")).toEqual([
        { type: "heading", level: 2, spans: [t("Title")] },
        { type: "heading", level: 1, spans: [t("C#")] },
      ]);
    });

    it("keeps a fenced code block verbatim, with its language, markdown and HTML untouched", () => {
      expect(parse("before\n```rust\nlet x = **y**;\n<b>z</b>\n```\nafter")).toEqual([
        p(t("before")),
        { type: "code", lang: "rust", text: "let x = **y**;\n<b>z</b>" },
        p(t("after")),
      ]);
    });

    it("an unclosed fence runs to the end, as the terminal UI draws it", () => {
      expect(parse("```\nstill code\n\nmore")).toEqual([{ type: "code", lang: "", text: "still code\n\nmore" }]);
    });

    it("a fence closes only on a run at least as long as its opener", () => {
      expect(parse("````\n```\ninner\n````")).toEqual([{ type: "code", lang: "", text: "```\ninner" }]);
    });

    it("reads bullet and numbered lists, with nesting by indent", () => {
      expect(parse("- one\n- two **b**\n  - inner\n\n1. first\n2) second\n10. tenth")).toEqual([
        {
          type: "list",
          ordered: false,
          items: [
            { depth: 0, marker: "•", spans: [t("one")] },
            { depth: 0, marker: "•", spans: [t("two "), t("b", { bold: true })] },
            { depth: 1, marker: "•", spans: [t("inner")] },
          ],
        },
        {
          type: "list",
          ordered: true,
          items: [
            { depth: 0, marker: "1.", spans: [t("first")] },
            { depth: 0, marker: "2.", spans: [t("second")] },
            { depth: 0, marker: "10.", spans: [t("tenth")] },
          ],
        },
      ]);
    });

    it("a line under a list item that is not an item continues it", () => {
      expect(parse("- one\n  carries on")).toEqual([
        { type: "list", ordered: false, items: [{ depth: 0, marker: "•", spans: [t("one\ncarries on")] }] },
      ]);
    });

    it("reads block quotes", () => {
      expect(parse("> quoted *it*\n> more\n\nplain")).toEqual([
        { type: "quote", spans: [t("quoted "), t("it", { italic: true }), t("\nmore")] },
        p(t("plain")),
      ]);
    });

    it("an empty text is no blocks", () => {
      expect(parse("")).toEqual([]);
      expect(parse("\n\n  \n")).toEqual([]);
    });
  });

  describe("spans", () => {
    it("reads bold, italic and inline code", () => {
      expect(inline("a **b** *c* _d_ `e`")).toEqual([
        t("a "),
        t("b", { bold: true }),
        t(" "),
        t("c", { italic: true }),
        t(" "),
        t("d", { italic: true }),
        t(" "),
        t("e", { code: true }),
      ]);
    });

    it("nests bold and italic", () => {
      expect(inline("**bold *both* bold**")).toEqual([
        t("bold ", { bold: true }),
        t("both", { bold: true, italic: true }),
        t(" bold", { bold: true }),
      ]);
    });

    it("leaves underscores inside words alone", () => {
      expect(inline("snake_case_name and __init__")).toEqual([t("snake_case_name and "), t("init", { bold: true })]);
    });

    it("leaves an unmatched delimiter as text", () => {
      expect(inline("**open and * lone")).toEqual([t("**open and * lone")]);
      expect(inline("2 * 3 * 4")).toEqual([t("2 * 3 * 4")]);
    });

    it("code spans take longer backtick runs and nothing inside them is markup", () => {
      expect(inline("``a ` **b**`` then `c`")).toEqual([t("a ` **b**", { code: true }), t(" then "), t("c", { code: true })]);
      expect(inline("`unclosed **b**")).toEqual([t("`unclosed "), t("b", { bold: true })]);
    });

    it("a link is its text with the URL beside it, as data", () => {
      expect(inline("see [the docs](https://example.com/a?b=1) now")).toEqual([
        t("see "),
        t("the docs", { url: "https://example.com/a?b=1" }),
        t(" now"),
      ]);
      expect(inline("[x](javascript:alert(1))")).toEqual([t("x", { url: "javascript:alert(1" }), t(")")]);
    });

    it("a bracket that does not make a link is text", () => {
      expect(inline("[a] (b) [c](d e)")).toEqual([t("[a] (b) [c](d e)")]);
    });

    it("backslash escapes punctuation", () => {
      expect(inline("\\*not\\* \\[x](y)")).toEqual([t("*not* [x](y)")]);
    });

    it("HTML is text", () => {
      const html = '<b>hi</b><script>alert(1)</script><img src=x onerror="y">';
      expect(parse(html)).toEqual([p(t(html))]);
    });
  });

  describe("pathological input parses in linear time", () => {
    const cases: [string, () => string][] = [
      ["10k nested stars", () => "*a ".repeat(10_000) + " a*".repeat(10_000)],
      ["10k stars in a run", () => "*".repeat(10_000)],
      ["10k openers that never close", () => "*a".repeat(10_000)],
      ["10k bold openers and an italic closer each", () => "**a *b".repeat(10_000)],
      ["1 MB of backticks", () => "`".repeat(1_000_000)],
      ["1 MB of backtick runs of every length", () => {
        let s = "";
        for (let n = 1; s.length < 1_000_000; n++) s += "`".repeat(n) + "x";
        return s;
      }],
      ["10k brackets before one link", () => "[".repeat(10_000) + "a](u)"],
      ["10k links that never close", () => "[a](".repeat(10_000)],
      ["10k list items", () => "- item\n".repeat(10_000)],
      ["10k fences", () => "```\n".repeat(10_000)],
      ["10k quote markers", () => ">".repeat(10_000)],
      ["a heading of 100k spaces", () => "# a" + " ".repeat(100_000) + "x"],
      ["a heading of 50k closing hashes", () => "# a" + " #".repeat(50_000) + " x"],
      ["100k underscores between words", () => "a_".repeat(100_000)],
    ];
    it.each(cases)("%s", (_name, make) => {
      const input = make();
      expect(timed(() => parse(input))).toBeLessThan(400);
    });

    it("a 1 MB backtick run inside a line is plain text, whole", () => {
      const input = "a " + "`".repeat(1_000_000);
      const blocks = parse(input);
      expect(blocks).toHaveLength(1);
      const [block] = blocks;
      expect(block?.type === "paragraph" ? block.spans.map((s) => s.text).join("") : null).toBe(input);
    });

    it("twice the input takes about twice the time, not four times", () => {
      const make = (n: number) => "*a [b](".repeat(n) + "`".repeat(n) + "_c".repeat(n);
      const small = timed(() => parse(make(20_000)));
      const large = timed(() => parse(make(80_000)));
      // 4x the input: a quadratic parser takes 16x; allow generous noise.
      expect(large).toBeLessThan(Math.max(small, 1) * 10);
    });
  });
});
