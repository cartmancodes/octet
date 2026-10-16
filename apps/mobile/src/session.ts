// What the Session screen derives from a session's view: the role of each transcript row, and
// the Task strip's words (the terminal UI's Task card, crates/octet-tui/src/view.rs `sidebar`).
// Pure; the components only draw it.
import { rawOf } from "./macs";
import type { PoseName } from "./mascot/poses";
import type { SessionState } from "./model/session";
import type { Entry } from "./model/transcript";
import type { CatalogInfo, Engine, GoalInfo, Mode, Phase, SessionInfo } from "./protocol";
import { phaseWords } from "./sessions";

/** A theme colour role, by name. */
export type Tone = "accent" | "fg" | "muted" | "amber";

export interface RowRole {
  /** The label over the entry, as the terminal UI's. */
  label: string;
  tone: Tone;
}

const ROLES: Record<Entry["kind"], RowRole> = {
  user: { label: "YOU", tone: "accent" },
  reply: { label: "OCTET", tone: "fg" },
  note: { label: "NOTE", tone: "muted" },
  error: { label: "ERROR", tone: "amber" },
  tools: { label: "TOOL", tone: "muted" },
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

/** What the Conversation's header says of a session: from its catalog when the Mac sent one,
 * else from the sessions list. */
export interface SessionHead {
  workspace: string | undefined;
  engine: Engine | undefined;
  /** The listed model, when the list names one. */
  model: string | undefined;
  mode: Mode | undefined;
}

/** The session's engine: the catalog's current model's, else the listed one. */
export function headEngine(catalog: CatalogInfo | null, head: SessionHead): Engine {
  return catalog?.current?.engine ?? head.engine ?? "claude";
}

/** The current model's name: the catalog's, else the listed model; null when neither says. */
export function headModel(catalog: CatalogInfo | null, head: SessionHead): string | null {
  const current = catalog?.current;
  if (current) {
    const model = catalog.models.find((m) => m.engine === current.engine && m.selection === current.selection);
    return model?.name ?? current.selection;
  }
  return head.model ?? null;
}

/** The current mode: the catalog's, else the listed one. */
export function headMode(catalog: CatalogInfo | null, head: SessionHead): Mode {
  return catalog?.current_mode ?? head.mode ?? "ask";
}

/** "Sonnet · ask"; just the mode when no model is known. */
export function headLine(catalog: CatalogInfo | null, head: SessionHead): string {
  const model = headModel(catalog, head);
  const mode = headMode(catalog, head);
  return model ? `${model} · ${mode}` : mode;
}

/** "claude / Sonnet"; just the engine when no model is known. */
export function engineModel(catalog: CatalogInfo | null, head: SessionHead): string {
  const model = headModel(catalog, head);
  const engine = headEngine(catalog, head);
  return model ? `${engine} / ${model}` : engine;
}

/** The first characters of a session's hub id, as the prototype's short hash: a key's Mac is not
 * part of it. */
export const shortId = (id: string): string => (rawOf(id) ?? id).slice(0, 6);

/** A workspace with the home folder as `~` ("/Users/me/code/octet" -> "~/code/octet"). */
export function homePath(path: string): string {
  return path.replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, "~");
}

/** The phase word over the transcript, by the mascot's pose: "CODING", "APPROVAL". A stopped
 * session's sleeping pose reads STOPPED. */
export function poseWord(pose: PoseName): string {
  return pose === "sleeping" ? "STOPPED" : pose.toUpperCase();
}
