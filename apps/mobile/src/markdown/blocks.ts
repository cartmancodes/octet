// The blocks a reply draws, bounded whatever the reply: at most `MAX_BLOCKS` of them, and a
// count of the rest. A finished reply is parsed once per text, in a cache bounded by bytes. A
// streaming reply is parsed as it grows: the blocks before its last whole one are kept, and
// only what came since is parsed again. Plain TypeScript; `Markdown.tsx` draws the result.
import { utf8Length } from "../client/frames";
import { parse, type Block } from "./parse";

/** Blocks one reply draws; the rest are on the Mac. A reply's text is bounded in bytes, but a
 * paragraph can be three of them, and every block is a view. */
export const MAX_BLOCKS = 400;
/** The most text whose blocks are kept for finished replies. */
export const PARSE_CACHE_BYTES = 2 * 1024 * 1024;
/** Streaming replies whose parsed start is kept: the ones on screen, a few at a time. */
const STREAMS_KEPT = 4;

/** What a reply draws: its first blocks, how many it has, and how many are not drawn. */
export interface Shown {
  blocks: Block[];
  total: number;
  more: number;
}

const shown = (all: Block[]): Shown =>
  all.length > MAX_BLOCKS
    ? { blocks: all.slice(0, MAX_BLOCKS), total: all.length, more: all.length - MAX_BLOCKS }
    : { blocks: all, total: all.length, more: 0 };

// Finished replies

/** FlashList recycles a row's component for another reply as the list scrolls, which would
 * otherwise parse each reply again as it comes back into view. Most recent last. */
const PARSED = new Map<string, { shown: Shown; bytes: number }>();
let parsedBytes = 0;

/** A finished reply's blocks, parsed once per text. */
export function doneBlocks(text: string): Shown {
  const hit = PARSED.get(text);
  if (hit) {
    PARSED.delete(text);
    PARSED.set(text, hit);
    return hit.shown;
  }
  const made = shown(parse(text));
  const bytes = utf8Length(text);
  if (bytes > PARSE_CACHE_BYTES) return made;
  PARSED.set(text, { shown: made, bytes });
  parsedBytes += bytes;
  for (const [old, { bytes: size }] of PARSED) {
    if (parsedBytes <= PARSE_CACHE_BYTES) break;
    PARSED.delete(old);
    parsedBytes -= size;
  }
  return made;
}

// Streaming replies

interface Stream {
  /** The text up to `head`: whole blocks, ending at the start of a line after a blank one. */
  headText: string;
  headBlocks: Block[];
  shown: Shown;
}
const STREAMS: Stream[] = [];

/** Fences open and close as `parse` sees them: `^ {0,3}` then three or more backticks or tildes. */
const FENCE = /^ {0,3}(`{3,}|~{3,})/;

/**
 * Where `text` can be cut, at or after `from` (a line start where `parse` holds nothing open):
 * the start of the line after the last whole blank line outside a fence. `parse` closes every
 * block at a blank line outside a fence, so the blocks of the text before the cut, then those of
 * the text after it, are the blocks of the whole. A line still growing (no newline yet) is
 * never a cut.
 */
function cutAt(text: string, from: number): number {
  let cut = from;
  let fence: string | null = null;
  let i = from;
  for (;;) {
    const nl = text.indexOf("\n", i);
    if (nl < 0) return cut;
    const raw = text.slice(i, nl);
    const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
    if (fence !== null) {
      const close = FENCE.exec(line);
      const mark = close?.[1];
      if (mark && mark[0] === fence[0] && mark.length >= fence.length && line.slice(close[0].length).trim() === "") fence = null;
    } else if (line.trim() === "") {
      cut = nl + 1;
    } else {
      const open = FENCE.exec(line);
      const mark = open?.[1];
      // A backtick fence's info string has no backtick: "```a```" is inline code.
      if (mark && (mark[0] !== "`" || !line.slice(open[0].length).includes("`"))) fence = mark;
    }
    i = nl + 1;
  }
}

/** A streaming reply's blocks. A chunk parses only the text since the reply's last whole block,
 * so it costs the same however long the reply has grown. */
export function streamBlocks(text: string): Shown {
  // The longest kept start this text still begins with; none when the start was dropped.
  let best = -1;
  for (let i = 0; i < STREAMS.length; i++) {
    const s = STREAMS[i];
    if (s && s.headText !== "" && text.startsWith(s.headText) && s.headText.length > (STREAMS[best]?.headText.length ?? 0)) best = i;
  }
  const prev = best >= 0 ? STREAMS.splice(best, 1)[0] : undefined;
  let head = prev?.headText.length ?? 0;
  let headBlocks = prev?.headBlocks ?? [];
  const cut = cutAt(text, head);
  if (cut > head) {
    headBlocks = headBlocks.concat(parse(text.slice(head, cut)));
    head = cut;
  }
  let out: Shown;
  if (prev && prev.headBlocks.length >= MAX_BLOCKS) {
    // The drawn blocks were all in the start, which only grew: they are the very same.
    const total = headBlocks.length + parse(text.slice(head)).length;
    out = { blocks: prev.shown.blocks, total, more: total - MAX_BLOCKS };
  } else {
    out = shown(headBlocks.concat(parse(text.slice(head))));
  }
  STREAMS.push({ headText: text.slice(0, head), headBlocks, shown: out });
  if (STREAMS.length > STREAMS_KEPT) STREAMS.shift();
  return out;
}

/** Bytes of text whose blocks the finished-reply cache holds. */
export const cachedBytes = (): number => parsedBytes;

/** Forgets every parsed reply: on a reset of the store (unpair, or another Mac). */
export function clearParsed(): void {
  PARSED.clear();
  parsedBytes = 0;
  STREAMS.length = 0;
}
