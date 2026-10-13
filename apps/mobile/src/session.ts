// What the Session screen derives from a session's view: the role of each transcript row, and
// the Task strip's words (the terminal UI's Task card, crates/octet-tui/src/view.rs `sidebar`).
// Pure; the components only draw it.
import type { SessionState } from "./model/session";
import type { Entry } from "./model/transcript";
import type { GoalInfo, Phase, SessionInfo } from "./protocol";
import { phaseWords } from "./sessions";

/** A theme colour role, by name. */
export type Tone = "accent" | "fg" | "muted" | "amber";

export interface RowRole {
  /** The terminal UI's label over the entry; empty for tools, which draw their own rows. */
  label: string;
  tone: Tone;
}

const ROLES: Record<Entry["kind"], RowRole> = {
  user: { label: "YOU", tone: "accent" },
  reply: { label: "OCTET", tone: "fg" },
  note: { label: "NOTE", tone: "muted" },
  error: { label: "ERROR", tone: "amber" },
  tools: { label: "", tone: "muted" },
};

/** How a transcript row is labelled and coloured. Called once per row drawn. */
export function rowRole(entry: Entry): RowRole {
  return ROLES[entry.kind];
}

/** "active · 3/20 turns". */
export function goalTurns(goal: GoalInfo): string {
  return `${goal.status} · ${goal.turns}/${goal.max} turns`;
}

/** Prompts the strip shows by their first line; the rest are counted. */
export const QUEUE_SHOWN = 3;

export interface QueueWords {
  /** The first line of each of the first prompts queued. */
  lines: string[];
  /** Prompts queued beyond `lines`. */
  more: number;
  /** Prompts queued in all: the terminal UI's count, which may be more than it sent. */
  count: number;
}

export function queueWords(state: SessionState | null): QueueWords {
  const queue = state?.queue ?? [];
  const count = Math.max(state?.queued ?? 0, queue.length);
  const lines = queue.slice(0, QUEUE_SHOWN).map((prompt) => prompt.split("\n", 1)[0] ?? "");
  return { lines, more: count - lines.length, count };
}

/** The NOW line: the terminal UI's, else the list's, else the phase in words. */
export function nowLine(state: SessionState | null, info: SessionInfo | undefined): string {
  if (state?.now) return state.now;
  if (info?.now) return info.now;
  const phase: Phase = state?.phase ?? info?.phase ?? "idle";
  return phaseWords(phase);
}

/** "5 queued", or nothing. */
export const queuedCount = (n: number): string => (n > 0 ? `${n} queued` : "");
