// What the screens derive from one of the hub's sessions: its name, its engine and phase in words,
// and its mascot pose. Pure.
import type { Activity } from "./mascot/state";
import { poseFor } from "./mascot/state";
import type { PoseName } from "./mascot/poses";
import type { TurnEnd } from "./model/session";
import type { Engine, Phase, SessionInfo } from "./protocol";

/** The last component of a workspace path ("/Users/me/Octet/" -> "Octet"). */
export function folderName(path: string): string {
  const parts = path.split(/[\\/]+/).filter((p) => p !== "");
  return parts[parts.length - 1] ?? (path === "" ? "" : path.slice(0, 1));
}

/** A session's name: its workspace folder, else "Session" (a blank or root workspace). */
export function sessionName(workspace: string | undefined): string {
  const folder = folderName(workspace ?? "").trim();
  return folder === "" || folder === "/" || folder === "\\" ? "Session" : folder;
}

/** An accessibility label part cut to about `max` characters: a screen reader reads every word. */
export function clipLabel(text: string, max = 120): string {
  if (text.length <= max) return text;
  const end = text.charCodeAt(max - 2);
  return `${text.slice(0, end >= 0xd800 && end <= 0xdbff ? max - 2 : max - 1)}…`;
}

const ENGINE_NAMES = { claude: "Claude", codex: "Codex", demo: "Demo" } as const;

/** An engine as the app names it. */
export const engineName = (engine: Engine): string => ENGINE_NAMES[engine];

const PHASE_WORDS = { connecting: "Connecting", idle: "Idle", running: "Running", stopped: "Stopped" } as const;

/** A phase in words. */
export const phaseWords = (phase: Phase): string => PHASE_WORDS[phase];

/** What a running session is doing, from its NOW line ("Running <tool> <target>",
 * "Writing reply", "Thinking"). */
function runningActivity(now: string | null | undefined): Activity {
  const match = /^Running (\S+)(?: (.*))?$/s.exec(now ?? "");
  if (match?.[1]) return { kind: "tool", name: match[1], target: match[2] ?? "" };
  return { kind: "replying" };
}

/** The mascot pose of one session, by the same mapping as the terminal UI's: an approval
 * waiting, else the running turn's activity, a stopped connection asleep, and otherwise how the
 * last turn ended (success, error) when known, else idle. */
export function poseOf(s: { approvals: number; phase: Phase; now?: string | null; turn?: TurnEnd | null }): PoseName {
  if (s.approvals > 0) return poseFor({ kind: "approval" });
  switch (s.phase) {
    case "running":
      return poseFor(runningActivity(s.now));
    case "stopped":
      // A session that stopped after an error keeps the error pose.
      return poseFor(s.turn === "failed" ? { kind: "failed" } : { kind: "stopped" });
    default:
      return poseFor(s.turn ? { kind: s.turn } : { kind: "idle" });
  }
}

/** The pose of a listed session: the list does not say how its last turn ended. */
export const sessionPose = (s: SessionInfo): PoseName => poseOf(s);
