// What the Recent and New screen derives: how long ago a session started, which workspaces and
// models can be offered, and the tmux name in the hub's start ack. Pure; the screen draws it.
import { FIELD_CHARS, field } from "./model/text";
import type { CatalogInfo, Engine, ModelInfo, RecentInfo, SessionInfo } from "./protocol";
import { folderName } from "./sessions";

const ENGINES: readonly string[] = ["claude", "codex", "demo"];

const isItem = (r: unknown): r is RecentInfo =>
  typeof r === "object" &&
  r !== null &&
  ENGINES.includes((r as RecentInfo).engine) &&
  typeof (r as RecentInfo).workspace === "string" &&
  typeof (r as RecentInfo).session === "string" &&
  typeof (r as RecentInfo).title === "string" &&
  typeof (r as RecentInfo).started === "number";

/** The items of a `recent` answer, or null for anything without its shape. The title and
 * workspace are cleaned and clipped like a listed session's; the session id is kept exactly, for
 * the Mac to resume by. */
export function recentItems(frame: unknown): RecentInfo[] | null {
  if (typeof frame !== "object" || frame === null) return null;
  const { type: kind, items } = frame as { type?: unknown; items?: unknown };
  if (kind !== "recent" || !Array.isArray(items)) return null;
  return items
    .filter(isItem)
    .map((r) => ({ ...r, title: field(r.title, FIELD_CHARS), workspace: field(r.workspace, FIELD_CHARS) }));
}

const MINUTE = 60;
const HOUR = 3_600;
const DAY = 86_400;

/** "just now", "5 min ago", "3 h ago", "2 d ago", "2 w ago", "3 mo ago", "1 y ago". `started` is
 * Unix seconds and `nowMs` the clock in milliseconds. A start in the future (clock skew) is just
 * now; zero or not a number is not known, so nothing. */
export function startedAgo(started: number, nowMs: number): string {
  if (!Number.isFinite(started) || started <= 0) return "";
  const seconds = Math.max(0, Math.floor(nowMs / 1000 - started));
  if (seconds < MINUTE) return "just now";
  if (seconds < HOUR) return `${Math.floor(seconds / MINUTE)} min ago`;
  if (seconds < DAY) return `${Math.floor(seconds / HOUR)} h ago`;
  const days = Math.floor(seconds / DAY);
  if (days < 7) return `${days} d ago`;
  if (days < 30) return `${Math.floor(days / 7)} w ago`;
  if (days < 365) return `${Math.floor(days / 30)} mo ago`;
  return `${Math.floor(days / 365)} y ago`;
}

/** The workspaces a new session may start in: the live sessions' and the recent items', each
 * once, by folder name (the full path breaks a tie). The hub refuses any other. */
export function workspaceChoices(sessions: readonly SessionInfo[], recent: readonly RecentInfo[]): string[] {
  const all = new Set<string>();
  for (const s of sessions) if (s.workspace !== "") all.add(s.workspace);
  for (const r of recent) if (r.workspace !== "") all.add(r.workspace);
  return [...all].sort((a, b) => {
    const byFolder = folderName(a).localeCompare(folderName(b));
    return byFolder !== 0 ? byFolder : a < b ? -1 : a > b ? 1 : 0;
  });
}

/** The models of `engine` that any of the live sessions' catalogs offer, in catalog order, the
 * first of a selection winning. */
export function modelsFor(catalogs: readonly CatalogInfo[], engine: Engine): ModelInfo[] {
  const seen = new Set<string>();
  const models: ModelInfo[] = [];
  for (const catalog of catalogs) {
    for (const m of catalog.models) {
      if (m.engine !== engine || seen.has(m.selection)) continue;
      seen.add(m.selection);
      models.push(m);
    }
  }
  return models;
}

const STARTING = /^Starting (octet-[\w-]+) in /;

/** The tmux session's name from a start ack ("Starting octet-a1b2c3 in /path"), else null. */
export function tmuxOfAck(ack: unknown): string | null {
  if (typeof ack !== "object" || ack === null) return null;
  const message = (ack as { message?: unknown }).message;
  if (typeof message !== "string") return null;
  return STARTING.exec(message)?.[1] ?? null;
}

/** A recent session as Recent lists it: the Mac it is on, and that Mac's name (only drawn with
 * several Macs). */
export type RecentItem = RecentInfo & { mac: string; macName: string };

/** A recent item's list key: its Mac, engine, workspace and id. */
export const recentKey = (r: RecentItem): string => `${r.mac}:${r.engine}:${r.workspace}:${r.session}`;

const byText = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

/** Every Mac's recent items as one list: newest start first, a tie by the Mac's name, then by
 * key. Each Mac's items keep the order the Mac gave them for an equal start and name only through
 * the key (the sort is total). */
export function mergeRecent(lists: readonly { mac: string; macName: string; items: readonly RecentInfo[] }[]): RecentItem[] {
  const all: RecentItem[] = [];
  for (const { mac, macName, items } of lists) for (const r of items) all.push({ ...r, mac, macName });
  return all.sort((a, b) => (b.started || 0) - (a.started || 0) || byText(a.macName, b.macName) || byText(recentKey(a), recentKey(b)));
}
