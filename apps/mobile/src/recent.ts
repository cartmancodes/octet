// What the Recent and New screen derives: how long ago a session started, which workspaces and
// models can be offered, and the tmux name in the hub's start ack. Pure; the screen draws it.
import type { CatalogInfo, Engine, ModelInfo, RecentInfo, SessionInfo } from "./protocol";
import { folderName } from "./sessions";

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
