// The transcript, built the way the TUI builds it (crates/octet-tui/src/app.rs: `App::event`,
// `tool`, `add`, `trim`, `calls_over`). Plain TypeScript, pure, with structural sharing: an
// event yields a new `Transcript` whose unchanged entries are the very same objects.
import type { HookData, KnownJournalEvent, ToolData, ToolPhase } from "../protocol";
import { utf8Length } from "../client/frames";
import { EARLIER, EARLIER_BYTES, TEXT, clip, fit, sanitize, tailStart, type Clipped, type Esc } from "./text";
import type { ToolCall } from "./summary";

export type { ToolCall };
export { EARLIER };

export type Entry =
  | { kind: "user"; key: string; text: string }
  | { kind: "reply"; key: string; text: string; done: boolean }
  | { kind: "tools"; key: string; calls: ToolCall[]; over: boolean }
  | { kind: "note" | "error"; key: string; text: string };

export interface Transcript {
  entries: Entry[];
  /** UTF-8 bytes the entries hold, exactly: the sum of `entrySize`. */
  bytes: number;
  /** The next entry's key. */
  next: number;
  /** Where an escape string cut by the end of a text record is. */
  esc: Esc;
  /** The bytes of the newest entry when it is an open reply or an open tools entry, so a stream
   * of records never counts it again. */
  tail: number;
}

export const EMPTY_TRANSCRIPT: Transcript = { entries: [], bytes: 0, next: 1, esc: TEXT, tail: 0 };

/** A tools entry closes, and a reply is trimmed back to, this many bytes. */
export const BLOCK_BYTES = 64 * 1024;
/** How far a streaming reply may outgrow `BLOCK_BYTES` before its start is dropped. */
export const BLOCK_SLACK = 16 * 1024;
/** Entries kept per session; older ones are on the Mac. */
export const MAX_ENTRIES = 1_000;
/** Bytes kept per session; the TUI keeps 512 KiB of 160 entries, the phone more of both. */
export const MAX_BYTES = 4 * 1024 * 1024;
/** The most one entry holds, however its calls come: a paired end is cut to what is left. */
export const ENTRY_BYTES = 200_000;
/** Entries dropped that wait before they are cut off. */
const COMPACT_AT = 256;
/** The longest tool name, and target, kept. */
const NAME_BYTES = 256;
const TARGET_BYTES = 4 * 1024;
/** A text record is cut to its last 4 × `BLOCK_BYTES` characters before it is scanned. */
const NOTE_BYTES = 8 * 1024;

const callBytes = (c: ToolCall): number => utf8Length(c.name) + utf8Length(c.target) + utf8Length(c.detail);

/** Bytes an entry holds, for the transcript's limit. */
export function entrySize(e: Entry): number {
  if (e.kind !== "tools") return utf8Length(e.text);
  let n = 0;
  for (const call of e.calls) n += callBytes(call);
  return n;
}

const isObj = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null;
const PHASES: readonly string[] = ["running", "done", "failed"];

/** A tool record's data with its strings clean and clipped, or null for data that is not one. */
interface Made {
  id: string;
  phase: ToolPhase;
  name: Clipped;
  target: Clipped;
  detail: Clipped;
}
function toCall(data: unknown): Made | null {
  if (!isObj(data)) return null;
  const { id, name, target, phase, detail } = data as Partial<Record<keyof ToolData, unknown>>;
  if (typeof id !== "string" || typeof name !== "string" || typeof phase !== "string" || !PHASES.includes(phase)) return null;
  return {
    id: clip(id, NAME_BYTES).text,
    phase: phase as ToolPhase,
    name: clip(name, NAME_BYTES),
    target: clip(target, TARGET_BYTES),
    detail: clip(detail, BLOCK_BYTES),
  };
}

/** The words for a hook the TUI shows, or null for one it does not (it ran). */
function hookNote(data: unknown): string | null {
  if (!isObj(data)) return null;
  const hook = data as Partial<Record<keyof HookData, unknown>>;
  const name = clip(hook.name, NAME_BYTES).text;
  const detail = clip(hook.detail, NOTE_BYTES).text;
  switch (hook.outcome) {
    case "failed":
      return detail === "" ? `Hook ${name} failed` : `Hook ${name} failed: ${detail}`;
    case "blocked":
      return `Hook ${name} blocked the action`;
    case "decided": {
      const decision = clip(hook.decision, NAME_BYTES).text;
      return decision === "allow"
        ? `Hook ${name} allowed the tool (Octet was not asked)`
        : `Hook ${name} answered the approval: ${decision}`;
    }
    default:
      return null;
  }
}

/**
 * A transcript being changed by a run of events. The first change copies the entries array, and
 * later ones write into the copy; `finish` returns the original when nothing changed. Entries
 * are never written to, only replaced.
 */
export class TranscriptDraft {
  private entries: Entry[];
  private bytes: number;
  private next: number;
  private esc: Esc;
  private tail: number;
  private owned = false;
  private changed = false;
  /** Entries before this index were dropped, and are cut off in bulk. */
  private head = 0;

  constructor(private readonly base: Transcript) {
    this.entries = base.entries;
    this.bytes = base.bytes;
    this.next = base.next;
    this.esc = base.esc;
    this.tail = base.tail;
  }

  finish(): Transcript {
    if (!this.changed) return this.base;
    this.compact();
    return { entries: this.entries, bytes: this.bytes, next: this.next, esc: this.esc, tail: this.tail };
  }

  apply(ev: KnownJournalEvent): void {
    switch (ev.type) {
      case "user":
        if (typeof ev.data === "string") this.add("user", ev.data);
        break;
      case "notice":
        if (typeof ev.data === "string") this.add("note", ev.data);
        break;
      case "error":
        if (typeof ev.data === "string") this.add("error", ev.data);
        break;
      case "text":
        if (typeof ev.data === "string") this.text(ev.data);
        break;
      case "tool":
        this.tool(ev.data);
        break;
      case "hook": {
        const note = hookNote(ev.data);
        if (note !== null) this.add("note", note);
        break;
      }
      case "started":
        this.closeReply();
        this.setEsc(TEXT);
        break;
      case "finished":
      case "stopped":
        this.turnOver();
        break;
      default:
        break; // not in the transcript: the session keeps approvals, mode, model and usage
    }
  }

  // The array is copied once, on the first write.
  private own(): Entry[] {
    if (!this.owned) {
      this.entries = this.entries.slice();
      this.owned = true;
    }
    this.changed = true;
    return this.entries;
  }

  private compact(): void {
    if (this.head === 0) return;
    this.own().splice(0, this.head);
    this.head = 0;
  }

  private setEsc(esc: Esc): void {
    if (esc === this.esc) return;
    this.esc = esc;
    this.changed = true;
  }

  private key(): string {
    return String(this.next++);
  }

  private push(entry: Entry, size: number): void {
    this.own().push(entry);
    this.bytes += size;
    this.tail = size;
    this.trim();
  }

  /** Adds an entry of cleaned, clipped `raw`. */
  private add(kind: "user" | "note" | "error", raw: string): void {
    const { text, bytes } = clip(raw, BLOCK_BYTES);
    this.push({ kind, key: this.key(), text }, bytes);
  }

  /** The newest entry, when it is an open reply. */
  private openReply(): (Entry & { kind: "reply" }) | null {
    const last = this.entries[this.entries.length - 1];
    return last?.kind === "reply" && !last.done ? last : null;
  }

  private closeReply(): void {
    const reply = this.openReply();
    if (reply) this.own()[this.entries.length - 1] = { ...reply, done: true };
  }

  private text(raw: string): void {
    // A record longer than any reply keeps is cut first; the escape state it began in is lost.
    const cut = raw.length > BLOCK_BYTES * 4;
    const piece = cut ? raw.slice(-BLOCK_BYTES * 4) : raw;
    const { out, esc } = sanitize(piece, cut ? TEXT : this.esc);
    this.setEsc(esc);
    if (out === "") return;
    const body = out.charCodeAt(0) >= 0xdc00 && out.charCodeAt(0) <= 0xdfff && cut ? out.slice(1) : out;
    let add = body;
    let addBytes = utf8Length(body);
    const reply = this.openReply();
    if (cut || addBytes > BLOCK_BYTES) {
      // The chunk alone outgrows a reply: its end is the reply's end.
      const tail = tailStart(add, BLOCK_BYTES);
      add = EARLIER + add.slice(tail.start);
      addBytes = EARLIER_BYTES + tail.bytes;
      this.replace(reply, add, addBytes, this.tail);
      return;
    }
    if (reply) this.replace(reply, reply.text + add, this.tail + addBytes, this.tail);
    else this.replace(null, add, addBytes, 0);
  }

  /** Makes `text` (of `size` bytes) the open reply's, or a new reply's; `old` is what it held. */
  private replace(reply: (Entry & { kind: "reply" }) | null, text: string, size: number, old: number): void {
    if (reply === null) {
      this.push({ kind: "reply", key: this.key(), text, done: false }, size);
      return;
    }
    let next = text;
    let nextSize = size;
    if (nextSize > BLOCK_BYTES + BLOCK_SLACK) {
      // Dropped in steps, since each drop makes the reply draw again from its start.
      const had = next.startsWith(EARLIER);
      const body = had ? next.slice(EARLIER.length) : next;
      const tail = tailStart(body, BLOCK_BYTES);
      next = EARLIER + body.slice(tail.start);
      nextSize = EARLIER_BYTES + tail.bytes;
    }
    this.own()[this.entries.length - 1] = { ...reply, text: next };
    this.bytes += nextSize - old;
    this.tail = nextSize;
    this.trim();
  }

  /** An end goes to the current turn's call with its ID, wherever other entries put it; any
   * other call joins the newest entry when that is an open tools entry under `BLOCK_BYTES`,
   * else starts one. */
  private tool(data: unknown): void {
    const made = toCall(data);
    if (!made) return;
    const { id, phase } = made;
    let index = -1;
    if (phase !== "running" && id !== "") {
      for (let i = this.entries.length - 1; i >= this.head; i--) {
        const e = this.entries[i];
        if (e?.kind !== "tools") continue;
        if (e.over) break; // the entries before it are over too
        if (e.calls.some((c) => c.id === id)) {
          index = i;
          break;
        }
      }
    }
    if (index < 0) {
      const back = this.entries[this.entries.length - 1];
      if (back?.kind !== "tools" || back.over || this.tail > BLOCK_BYTES) {
        this.push({ kind: "tools", key: this.key(), calls: [], over: false }, 0);
      }
      index = this.entries.length - 1;
    }
    const entry = this.entries[index];
    if (entry?.kind !== "tools") return;
    // A call with an ID replaces the one with that ID, keeping the start's target when the end has none.
    const at = id === "" ? -1 : entry.calls.findIndex((c) => c.id === id);
    const old = at >= 0 ? entry.calls[at] : undefined;
    const target: Clipped =
      old && made.target.text === "" ? { text: old.target, bytes: utf8Length(old.target) } : made.target;
    // An entry holds ENTRY_BYTES at most, however its calls arrive: the detail gets what is left.
    const size = index === this.entries.length - 1 ? this.tail : entrySize(entry);
    const oldBytes = old ? callBytes(old) : 0;
    const room = ENTRY_BYTES - (size - oldBytes) - made.name.bytes - target.bytes;
    const detail = fit(made.detail, Math.min(BLOCK_BYTES, room));
    const call: ToolCall = { id, name: made.name.text, target: target.text, phase, detail: detail.text };
    const bytes = made.name.bytes + target.bytes + detail.bytes;
    const calls = entry.calls.slice();
    if (old) calls[at] = call;
    else calls.push(call);
    this.own()[index] = { ...entry, calls };
    this.bytes += bytes - oldBytes;
    if (index === this.entries.length - 1) this.tail += bytes - oldBytes;
    this.trim();
  }

  /** The turn ended, finished or not: calls still running were stopped, and the reply is whole. */
  private turnOver(): void {
    for (let i = this.entries.length - 1; i >= this.head; i--) {
      const e = this.entries[i];
      if (e?.kind !== "tools") continue;
      if (e.over) break; // the entries before it are over too
      this.own()[i] = { ...e, over: true };
    }
    this.closeReply();
  }

  /** Old entries go, whole, past the limits; the newest never does. They are cut off in bulk,
   * when the batch ends or a few hundred are waiting, not one by one. */
  private trim(): void {
    while (this.entries.length - this.head > 1 && (this.bytes > MAX_BYTES || this.entries.length - this.head > MAX_ENTRIES)) {
      const old = this.entries[this.head];
      if (old) this.bytes -= entrySize(old);
      this.head += 1;
      this.changed = true;
    }
    if (this.head >= COMPACT_AT) this.compact();
  }
}

/** The transcript after `events`, in order. */
export function applyAll(t: Transcript, events: readonly KnownJournalEvent[]): Transcript {
  const draft = new TranscriptDraft(t);
  for (const ev of events) draft.apply(ev);
  return draft.finish();
}

/** The transcript after one event; `t` itself when the event changes nothing. */
export function apply(t: Transcript, ev: KnownJournalEvent): Transcript {
  return applyAll(t, [ev]);
}
