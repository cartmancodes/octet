// The Markdown a reply is drawn with: a small subset (plan ruling 1), parsed in time linear in
// its input, into plain data. Nothing here interprets the text: there is no HTML, a link is its
// text and its URL as strings, and code is kept verbatim. Plain TypeScript; `Markdown.tsx` draws it.
//
// Blocks: paragraphs, headings (# to ######), fenced code (``` or ~~~), bullet and numbered
// lists, block quotes. Spans: **bold** and __bold__, *italic* and _italic_ (not inside words),
// `code` (any backtick run length), [text](url), and backslash escapes.

/** A run of text with one style. A link is its text with `url` beside it, never something to open. */
export interface Span {
  text: string;
  bold?: true;
  italic?: true;
  code?: true;
  url?: string;
}

export interface ListItem {
  /** Nesting, from the item's indent: 0 to `MAX_DEPTH`. */
  depth: number;
  /** "•", or the item's number and a dot. */
  marker: string;
  spans: Span[];
}

export type Block =
  | { type: "paragraph"; spans: Span[] }
  | { type: "heading"; level: number; spans: Span[] }
  | { type: "code"; lang: string; text: string }
  | { type: "list"; ordered: boolean; items: ListItem[] }
  | { type: "quote"; spans: Span[] };

const MAX_DEPTH = 4;
const LANG_CHARS = 32;
/** A URL longer than this is not taken as a link's (its text stays as written). */
const URL_CHARS = 2_048;

// Each pattern below is anchored and bounded, so a line is matched in time linear in its length.
const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+|$)/;
const FENCE = /^ {0,3}(`{3,}|~{3,})/;
const BULLET = /^( *)[-*+][ \t]+/;
const NUMBER = /^( *)(\d{1,9})[.)][ \t]+/;
const QUOTE = /^ {0,3}> ?/;

/** A heading's text without its closing hashes ("## Title ##"), by hand: a regex for it backtracks. */
function headingText(rest: string): string {
  const text = rest.trimEnd();
  let end = text.length;
  while (end > 0 && text[end - 1] === "#") end--;
  if (end === 0) return "";
  return end < text.length && (text[end - 1] === " " || text[end - 1] === "\t") ? text.slice(0, end).trim() : text.trim();
}

interface Item {
  depth: number;
  marker: string;
  ordered: boolean;
  lines: string[];
}

/** `text` as blocks. */
export function parse(text: string): Block[] {
  const blocks: Block[] = [];
  const lines = text.split("\n");
  let para: string[] = [];
  let quote: string[] = [];
  let items: Item[] = [];
  let fence: { mark: string; lang: string; lines: string[] } | null = null;

  const flushPara = () => {
    if (para.length > 0) blocks.push({ type: "paragraph", spans: inline(para.join("\n")) });
    para = [];
  };
  const flushQuote = () => {
    if (quote.length > 0) blocks.push({ type: "quote", spans: inline(quote.join("\n")) });
    quote = [];
  };
  const flushList = () => {
    let run: ListItem[] = [];
    let ordered = false;
    for (const item of items) {
      if (run.length > 0 && item.ordered !== ordered && item.depth === 0) {
        blocks.push({ type: "list", ordered, items: run });
        run = [];
      }
      if (run.length === 0) ordered = item.ordered;
      run.push({ depth: item.depth, marker: item.marker, spans: inline(item.lines.join("\n")) });
    }
    if (run.length > 0) blocks.push({ type: "list", ordered, items: run });
    items = [];
  };
  const flush = () => {
    flushPara();
    flushQuote();
    flushList();
  };

  for (const raw of lines) {
    const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
    if (fence) {
      const close = FENCE.exec(line);
      const mark = close?.[1];
      if (mark && mark[0] === fence.mark[0] && mark.length >= fence.mark.length && line.slice(close[0].length).trim() === "") {
        blocks.push({ type: "code", lang: fence.lang, text: fence.lines.join("\n") });
        fence = null;
      } else {
        fence.lines.push(line);
      }
      continue;
    }
    if (line.trim() === "") {
      flush();
      continue;
    }
    const open = FENCE.exec(line);
    if (open?.[1]) {
      const info = line.slice(open[0].length).trim();
      // A backtick fence's info string has no backtick: "```a```" is inline code.
      if (open[1][0] !== "`" || !info.includes("`")) {
        flush();
        fence = { mark: open[1], lang: (info.split(/\s/, 1)[0] ?? "").slice(0, LANG_CHARS), lines: [] };
        continue;
      }
    }
    const heading = HEADING.exec(line);
    if (heading?.[1]) {
      flush();
      blocks.push({ type: "heading", level: heading[1].length, spans: inline(headingText(line.slice(heading[0].length))) });
      continue;
    }
    const q = QUOTE.exec(line);
    if (q) {
      flushPara();
      flushList();
      quote.push(line.slice(q[0].length));
      continue;
    }
    const bullet = BULLET.exec(line);
    const number = bullet ? null : NUMBER.exec(line);
    const item = bullet ?? number;
    if (item) {
      flushPara();
      flushQuote();
      const indent = item[1]?.length ?? 0;
      items.push({
        depth: Math.min(MAX_DEPTH, Math.floor(indent / 2)),
        marker: number?.[2] !== undefined ? `${Number(number[2])}.` : "•",
        ordered: number !== null,
        lines: [line.slice(item[0].length)],
      });
      continue;
    }
    const last = items[items.length - 1];
    if (last) {
      last.lines.push(line.trim());
      continue;
    }
    if (quote.length > 0) {
      quote.push(line); // a lazy continuation of the quote
      continue;
    }
    para.push(line);
  }
  if (fence) blocks.push({ type: "code", lang: fence.lang, text: fence.lines.join("\n") });
  flush();
  return blocks;
}

// Inline spans, in one pass over tokens. Emphasis delimiters are matched with one stack per
// kind, each closer against the top of its own stack, so nothing is searched twice; a matched
// pair adds its style to the tokens between, by counts swept once at the end. Crossed pairs
// (`*a **b* c**`) just overlap, which flat spans draw fine.

type Kind = "*" | "**" | "_" | "__";

interface Token {
  text: string;
  code?: true;
  url?: string;
  /** An emphasis delimiter not (yet) matched. */
  delim?: Kind;
  /** For a matched opener: the style it starts; its closer ends it. */
  starts?: "bold" | "italic";
  ends?: "bold" | "italic";
}

const PUNCT = /[!-/:-@[-`{-~]/;
const SPACE = /\s/;
const WORD = /[\p{L}\p{N}]/u;

/** For each index, the next index at or after it holding `ch`, or -1. One pass from the end. */
function nextOf(text: string, ch: string): Int32Array {
  const out = new Int32Array(text.length + 1).fill(-1);
  for (let i = text.length - 1; i >= 0; i--) out[i] = text[i] === ch ? i : (out[i + 1] ?? -1);
  return out;
}

/** For each index, the next whitespace at or after it, or the text's length. */
function nextSpace(text: string): Int32Array {
  const out = new Int32Array(text.length + 1).fill(text.length);
  for (let i = text.length - 1; i >= 0; i--) out[i] = SPACE.test(text[i] ?? "") ? i : (out[i + 1] ?? text.length);
  return out;
}

/** Where each backtick run starts, by run length, so a closer is found by walking forward once. */
function backtickRuns(text: string): Map<number, number[]> {
  const runs = new Map<number, number[]>();
  let i = 0;
  while (i < text.length) {
    if (text[i] !== "`") {
      i++;
      continue;
    }
    let j = i;
    while (text[j] === "`") j++;
    const list = runs.get(j - i);
    if (list) list.push(i);
    else runs.set(j - i, [i]);
    i = j;
  }
  return runs;
}

/** `text`'s spans. */
export function inline(text: string): Span[] {
  const tokens: Token[] = [];
  let plain = "";
  const push = (token: Token) => {
    if (plain !== "") tokens.push({ text: plain });
    plain = "";
    tokens.push(token);
  };

  // Lazily built: most texts have no code spans or links.
  let runs: Map<number, number[]> | null = null;
  const cursor = new Map<number, number>();
  let closeBracket: Int32Array | null = null;
  let closeParen: Int32Array | null = null;
  let spaces: Int32Array | null = null;

  const stacks: Record<Kind, number[]> = { "*": [], "**": [], _: [], __: [] };

  let i = 0;
  while (i < text.length) {
    const c = text[i] ?? "";
    if (c === "\\" && i + 1 < text.length && PUNCT.test(text[i + 1] ?? "")) {
      plain += text[i + 1];
      i += 2;
      continue;
    }
    if (c === "`") {
      let j = i;
      while (text[j] === "`") j++;
      const n = j - i;
      runs ??= backtickRuns(text);
      const list = runs.get(n) ?? [];
      // The next run of exactly `n` after this one: the per-length cursor only moves forward.
      let k = cursor.get(n) ?? 0;
      while (k < list.length && (list[k] ?? 0) <= i) k++;
      cursor.set(n, k);
      const close = list[k];
      if (close === undefined) {
        plain += text.slice(i, j);
        i = j;
        continue;
      }
      let body = text.slice(j, close).replace(/\n/g, " ");
      if (body.length > 2 && body.startsWith(" ") && body.endsWith(" ") && body.trim() !== "") body = body.slice(1, -1);
      push({ text: body, code: true });
      i = close + n;
      continue;
    }
    if (c === "[") {
      closeBracket ??= nextOf(text, "]");
      const end = closeBracket[i + 1] ?? -1;
      if (end > i && text[end + 1] === "(") {
        closeParen ??= nextOf(text, ")");
        spaces ??= nextSpace(text);
        const paren = closeParen[end + 2] ?? -1;
        if (paren > end + 1 && paren - end - 2 <= URL_CHARS && (spaces[end + 2] ?? 0) >= paren) {
          const label = text.slice(i + 1, end);
          push({ text: label === "" ? text.slice(end + 2, paren) : label, url: text.slice(end + 2, paren) });
          i = paren + 1;
          continue;
        }
      }
      plain += c;
      i++;
      continue;
    }
    if (c === "*" || c === "_") {
      let j = i;
      while (text[j] === c) j++;
      const n = j - i;
      if (n > 2) {
        plain += text.slice(i, j);
        i = j;
        continue;
      }
      const kind = (c.repeat(n) as Kind);
      const before = text[i - 1] ?? " ";
      const after = text[j] ?? " ";
      let canOpen = !SPACE.test(after);
      let canClose = !SPACE.test(before);
      if (c === "_") {
        canOpen &&= !WORD.test(before);
        canClose &&= !WORD.test(after);
      }
      const stack = stacks[kind];
      if (canClose && stack.length > 0) {
        const opener = stack.pop() ?? 0;
        const style = n === 2 ? "bold" : "italic";
        const start = tokens[opener];
        if (start) {
          start.delim = undefined;
          start.text = "";
          start.starts = style;
        }
        push({ text: "", ends: style });
        i = j;
        continue;
      }
      if (canOpen) {
        push({ text: kind, delim: kind });
        stack.push(tokens.length - 1);
        i = j;
        continue;
      }
      plain += text.slice(i, j);
      i = j;
      continue;
    }
    plain += c;
    i++;
  }
  if (plain !== "") tokens.push({ text: plain });

  // One sweep: the styles open at each token, and spans of like style merged.
  const spans: Span[] = [];
  let bold = 0;
  let italic = 0;
  for (const token of tokens) {
    if (token.starts === "bold") bold++;
    if (token.starts === "italic") italic++;
    if (token.ends === "bold") bold--;
    if (token.ends === "italic") italic--;
    if (token.text === "") continue;
    const span: Span = { text: token.text };
    if (bold > 0) span.bold = true;
    if (italic > 0) span.italic = true;
    if (token.code) span.code = true;
    if (token.url !== undefined) span.url = token.url;
    const prev = spans[spans.length - 1];
    if (prev && !prev.code && !span.code && prev.url === undefined && span.url === undefined && prev.bold === span.bold && prev.italic === span.italic) {
      prev.text += span.text;
    } else {
      spans.push(span);
    }
  }
  return spans;
}
