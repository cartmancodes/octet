// What the Live Activity shows, the same way the hub's driver decides it while the phone is away
// (crates/octet-hub/src/activity.rs), so the widget sees one shape whoever drives it; what the
// widget's App Group holds of each waiting approval; and the tokens a Mac is told. Pure: no React,
// no native module, no storage (src/activity.ts drives it).
import { entryKey, type ActivityKind, type WidgetEntry } from "../modules/octet-live";
import type { RequestBody } from "./client/HubClient";
import { foldCommand } from "./model/command";
import { field } from "./model/text";
import { sessionName } from "./sessions";
import type { ListedSession } from "./store";

/** How long every session must have been idle before the activity ends: the hub's `IDLE_END`. */
export const IDLE_END_MS = 5 * 60_000;
/** The App Group's entries, at most: what the native side takes (modules/octet-live). */
export const ENTRIES_LIMIT = 64;
/** The App Group's JSON, at most, in UTF-8 bytes: what the native side takes. */
export const ENTRIES_BYTES = 64 * 1024;
/** A session's name in an entry, at most, in characters. */
export const NAME_CHARS = 80;
/** An approval's command in an entry, at most, in characters. */
export const COMMAND_CHARS = 200;

/** What the derivation reads of one listed session. */
export interface ActivityRow {
  /** The phone's key, `mac/session`. */
  key: string;
  /** The approvals waiting in it. */
  approvals: number;
  phase: string;
  /** The id of its first waiting approval, when its row names one. */
  front: number | null;
}

/** What the activity shows, with the session as the phone's key ("" for none) and `since` in
 * milliseconds; the controller turns it into the content state. */
export interface Lead {
  kind: ActivityKind;
  key: string;
  since: number;
  approvals: number;
}

/** The rows of `sessions`, as the derivation reads them. */
export const rowsOf = (sessions: readonly ListedSession[]): ActivityRow[] =>
  sessions.map((s) => ({ key: s.id, approvals: s.approvals, phase: s.phase, front: s.approval?.id ?? null }));

/** What the model remembers of one session. */
interface Tracked {
  kind: ActivityKind;
  /** When its own kind began. */
  since: number;
  /** When it last went from running or waiting to done; null before it has. */
  finished: number | null;
  /** When the approval it has waited on longest was first seen; null while none waits. */
  waiting: number | null;
  front: number | null;
  approvals: number;
}

/** What a row shows on its own: an approval if one waits, running while its phase says so, else
 * done. */
const ownKind = (row: ActivityRow): ActivityKind => (row.approvals > 0 ? "approval" : row.phase === "running" ? "running" : "done");

const byKey = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

/**
 * The hub's driver's rules, on the phone's list: `approval` if any approval waits, else `running`
 * if any session runs, else `done`. The session that leads is the one with the oldest waiting
 * approval, else the most recently started running one, else the last that finished; `since` is
 * when that session's own kind began, and `approvals` counts every approval waiting. A session's
 * first look only seeds it (it has finished nothing yet), and one that leaves the list is
 * forgotten. With none leading, `since` is when every session last went idle.
 *
 * The phone's row names only a session's first waiting approval, not each one with its own time as
 * the hub's state does: a session's wait starts when its first approval comes, and starts again
 * when the one it waited on was answered with no other waiting behind it.
 */
export class ActivityModel {
  private readonly sessions = new Map<string, Tracked>();
  private idle: number | null = null;

  /** Takes the sessions as they stand at `now` (milliseconds). */
  observe(rows: readonly ActivityRow[], now: number): void {
    const seen = new Set<string>();
    for (const row of rows) {
      seen.add(row.key);
      const kind = ownKind(row);
      const old = this.sessions.get(row.key);
      if (!old) {
        this.sessions.set(row.key, {
          kind,
          since: now,
          finished: null,
          waiting: row.approvals > 0 ? now : null,
          front: row.front,
          approvals: row.approvals,
        });
        continue;
      }
      if (row.approvals === 0) old.waiting = null;
      else if (old.waiting === null) old.waiting = now;
      else if (old.front !== null && row.front !== null && row.front !== old.front && old.approvals <= 1) old.waiting = now;
      old.front = row.approvals > 0 ? row.front : null;
      old.approvals = row.approvals;
      if (kind !== old.kind) {
        if (kind === "done") old.finished = now;
        old.kind = kind;
        old.since = now;
      }
    }
    for (const key of [...this.sessions.keys()]) if (!seen.has(key)) this.sessions.delete(key);
    if (this.content(now).kind === "done") this.idle ??= now;
    else this.idle = null;
  }

  /** When every session last went idle; null while one waits or runs, or before the first look. */
  idleSince(): number | null {
    return this.idle;
  }

  /** What the activity shows at `now`. */
  content(now: number): Lead {
    let approvals = 0;
    let waiting: [string, Tracked] | null = null;
    let running: [string, Tracked] | null = null;
    let done: [string, Tracked] | null = null;
    for (const entry of this.sessions) {
      const [key, s] = entry;
      approvals += s.approvals;
      if (s.kind === "approval") {
        const at = s.waiting ?? s.since;
        const best = waiting ? (waiting[1].waiting ?? waiting[1].since) : Infinity;
        if (at < best || (at === best && waiting && byKey(key, waiting[0]) < 0)) waiting = entry;
      } else if (s.kind === "running") {
        if (!running || s.since > running[1].since || (s.since === running[1].since && byKey(key, running[0]) > 0)) running = entry;
      } else if (s.finished !== null) {
        const best = done?.[1].finished ?? -Infinity;
        if (!done || s.finished > best || (s.finished === best && byKey(key, done[0]) > 0)) done = entry;
      }
    }
    const chosen = waiting ?? running ?? done;
    if (!chosen) return { kind: "done", key: "", since: this.idle ?? now, approvals };
    const [key, s] = chosen;
    return { kind: s.kind, key, since: s.since, approvals };
  }
}

/** UTF-8 bytes of `text`. */
function utf8Bytes(text: string): number {
  let bytes = 0;
  for (const c of text) {
    const cp = c.codePointAt(0) ?? 0;
    bytes += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
  }
  return bytes;
}

const newestFirst = (a: ListedSession, b: ListedSession): number =>
  (b.last_activity ?? 0) - (a.last_activity ?? 0) || byKey(a.id, b.id);

/**
 * What the widget's App Group holds: for each session with an approval waiting, keyed the way the
 * widget sees it (`<host>/<raw session>`, from its content state), the session's name (at most
 * `NAME_CHARS`), what the approval runs (its command, folded so no line of it hides below blank
 * ones, else its action; at most `COMMAND_CHARS`) and the approval's id. Each value is cleaned of
 * control characters. Newest first, at most `ENTRIES_LIMIT`, within `ENTRIES_BYTES` of JSON. A
 * session whose Mac has no host (`hostOf` null) is left out.
 */
export function widgetEntries(sessions: readonly ListedSession[], hostOf: (mac: string) => string | null): Record<string, WidgetEntry> {
  const out: Record<string, WidgetEntry> = {};
  let bytes = 2; // {}
  let count = 0;
  for (const s of sessions.filter((x) => x.approvals > 0 && x.approval).sort(newestFirst)) {
    if (count >= ENTRIES_LIMIT) break;
    const host = hostOf(s.mac);
    if (host === null || !s.approval) continue;
    const { id, summary } = s.approval;
    const key = entryKey(host, s.raw);
    const entry: WidgetEntry = {
      name: field(sessionName(s.workspace), NAME_CHARS),
      command: field(foldCommand(summary.command ?? summary.action).text, COMMAND_CHARS),
      approval: String(id),
    };
    const size = utf8Bytes(JSON.stringify(key)) + 1 + utf8Bytes(JSON.stringify(entry)) + (count > 0 ? 1 : 0);
    if (bytes + size > ENTRIES_BYTES) break;
    out[key] = entry;
    bytes += size;
    count += 1;
  }
  return out;
}

/** The ActivityKit tokens the phone has now: the running activity's, and the push-to-start one. */
export interface ActivityTokens {
  token: string | null;
  start: string | null;
}

/** The `register_activity` that states `tokens` (a token it leaves out is one the phone has not
 * got); null when there is neither, which the hub would refuse. */
export function activityRegistration({ token, start }: ActivityTokens): RequestBody | null {
  if (token === null && start === null) return null;
  return { type: "register_activity", ...(token !== null ? { token } : {}), ...(start !== null ? { start_token: start } : {}) };
}
