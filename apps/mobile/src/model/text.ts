// Text from the hub is untrusted: it is cleaned of terminal escapes and control characters (the
// way the TUI's `clean` does, crates/octet-tui/src/text.rs) and clipped before it enters the
// store. Everything here is plain TypeScript, with exact UTF-8 byte counts.
import { utf8Length } from "../client/frames";

/** Put in front of text whose start was dropped. */
export const EARLIER = "[earlier output is on the Mac]\n";
export const EARLIER_BYTES = utf8Length(EARLIER);

/** Where an escape string is, between calls: escapes can cross streaming records. */
export interface Esc {
  mode: "text" | "escape" | "intermediate" | "csi" | "string" | "stringEscape";
  /** Characters of the string dropped so far. */
  len: number;
}
export const TEXT: Esc = { mode: "text", len: 0 };

/** The most of one escape string dropped before it is taken as unterminated. */
const STRING_LIMIT = 4096;

/** Only printable ASCII and newlines: nothing to clean. */
const PLAIN = /^[\x20-\x7e\n]*$/;

const isBidi = (cp: number): boolean => (cp >= 0x202a && cp <= 0x202e) || (cp >= 0x2066 && cp <= 0x2069);
const isControl = (cp: number): boolean => cp < 0x20 || (cp >= 0x7f && cp <= 0x9f);
const isSurrogate = (cp: number): boolean => cp >= 0xd800 && cp <= 0xdfff;

/**
 * `text` without terminal escapes, control characters (but newlines), bidi overrides and lone
 * surrogates, tabs as four spaces; and where the escape state ends. Lone surrogates cannot be
 * in a Rust string, and dropping them keeps byte counts exact.
 */
export function sanitize(text: string, from: Esc): { out: string; esc: Esc } {
  if (from.mode === "text" && PLAIN.test(text)) return { out: text, esc: from };
  let { mode, len } = from;
  let out = "";
  for (const c of text) {
    const cp = c.codePointAt(0) ?? 0;
    switch (mode) {
      case "text":
        if (cp === 0x1b) mode = "escape";
        else if (cp === 0x9b) mode = "csi";
        else if (cp === 0x90 || cp === 0x98 || cp === 0x9d || cp === 0x9e || cp === 0x9f) {
          mode = "string";
          len = 0;
        } else if (cp === 0x0a) out += c;
        else if (cp === 0x09) out += "    ";
        else if (!isControl(cp) && !isBidi(cp) && !isSurrogate(cp)) out += c;
        break;
      case "escape":
        if (c === "[") mode = "csi";
        else if (c === "]" || c === "P" || c === "X" || c === "^" || c === "_") {
          mode = "string";
          len = 0;
        } else if (cp >= 0x20 && cp <= 0x2f) mode = "intermediate";
        else mode = "text";
        break;
      case "intermediate":
        if (!(cp >= 0x20 && cp <= 0x2f)) mode = "text";
        break;
      case "csi":
        if (cp >= 0x40 && cp <= 0x7e) mode = "text";
        break;
      case "string":
        if (cp === 0x07 || cp === 0x9c) mode = "text";
        else if (cp === 0x1b) mode = "stringEscape";
        else if (cp === 0x0a) {
          mode = "text"; // unterminated: the line it began on ends it
          out += c;
        } else if (len >= STRING_LIMIT) mode = "text";
        else len += 1;
        break;
      case "stringEscape":
        if (c === "\\" || cp === 0x07) mode = "text";
        else mode = "string";
        break;
    }
  }
  const esc = mode === "text" ? TEXT : { mode, len };
  return { out, esc };
}

/** Where the last `limit` bytes of `s` start, and how many bytes that is (s is well-formed). */
export function tailStart(s: string, limit: number): { start: number; bytes: number } {
  let bytes = 0;
  let i = s.length;
  while (i > 0) {
    const unit = s.charCodeAt(i - 1);
    let size: number;
    let width = 1;
    if (unit >= 0xdc00 && unit <= 0xdfff) {
      size = 4; // the low half of a pair: well-formed text has its high half before it
      width = 2;
    } else if (unit < 0x80) size = 1;
    else if (unit < 0x800) size = 2;
    else size = 3;
    if (bytes + size > limit) break;
    bytes += size;
    i -= width;
  }
  return { start: i, bytes };
}

/** A string with its UTF-8 size. */
export interface Clipped {
  text: string;
  bytes: number;
}

/** A well-formed start: a low surrogate at the front lost its high half to a slice. */
function wholeStart(s: string): string {
  const unit = s.charCodeAt(0);
  return unit >= 0xdc00 && unit <= 0xdfff ? s.slice(1) : s;
}

/**
 * `raw` cleaned and, past `limit` bytes, cut to its last `limit` bytes behind `EARLIER`. A record
 * of megabytes is cut before it is scanned, so its cost is bounded by `limit`. Anything that is
 * not a string is empty.
 */
export function clip(raw: unknown, limit: number): Clipped {
  const s = typeof raw === "string" ? raw : "";
  const keep = limit * 4; // a character is at most 3 bytes, so this holds the last `limit` bytes
  const cut = s.length > keep;
  const { out } = sanitize(cut ? wholeStart(s.slice(-keep)) : s, TEXT);
  const bytes = utf8Length(out);
  if (!cut && bytes <= limit) return { text: out, bytes };
  if (bytes <= limit) return { text: EARLIER + out, bytes: EARLIER_BYTES + bytes };
  const tail = tailStart(out, limit);
  return { text: EARLIER + out.slice(tail.start), bytes: EARLIER_BYTES + tail.bytes };
}

/** `clipped` cut, if need be, to at most `limit` bytes in all: its end behind `EARLIER`, or
 * nothing when `limit` leaves no room for any of it. */
export function fit(clipped: Clipped, limit: number): Clipped {
  if (clipped.bytes <= limit) return clipped;
  if (limit <= EARLIER_BYTES) return { text: "", bytes: 0 };
  const tail = tailStart(clipped.text, limit - EARLIER_BYTES);
  return { text: EARLIER + clipped.text.slice(tail.start), bytes: EARLIER_BYTES + tail.bytes };
}

/** The most characters kept of a short hub string: a name, a reason, a label. */
export const SHORT_CHARS = 512;
/** The most characters kept of a path or an id, and of a session's listed fields. */
export const FIELD_CHARS = 4_096;

/** A short field cleaned and clipped to `limit` characters, for strings that are not output. */
export function field(raw: unknown, limit: number): string {
  const s = typeof raw === "string" ? raw : "";
  const { out } = sanitize(s.length > limit * 2 ? wholeStart(s.slice(0, limit * 2)) : s, TEXT);
  if (out.length <= limit) return out;
  const end = out.charCodeAt(limit - 1);
  return out.slice(0, end >= 0xd800 && end <= 0xdbff ? limit - 1 : limit);
}
