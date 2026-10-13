// What the Sessions screen derives from the hub's list: the order, the words on a card, and the
// mascot pose for the header. Pure; the screen only draws it.
import type { HubState } from "./client/HubClient";
import type { Activity } from "./mascot/state";
import { poseFor } from "./mascot/state";
import type { PoseName } from "./mascot/poses";
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

/** "Claude · sonnet-5": the engine, and the model when the Mac says which. */
export function engineLine(s: SessionInfo): string {
  const engine = ENGINE_NAMES[s.engine];
  return s.model ? `${engine} · ${s.model}` : engine;
}

const PHASE_WORDS = { connecting: "Connecting", idle: "Idle", running: "Running", stopped: "Stopped" } as const;

/** A phase in words. */
export const phaseWords = (phase: Phase): string => PHASE_WORDS[phase];

/** The NOW line, else the phase in words. */
export function nowText(s: SessionInfo): string {
  return s.now ? s.now : PHASE_WORDS[s.phase];
}

/** Approvals waiting first, then the newest activity; the id breaks a tie so the order never
 * jumps. Returns a new array. */
export function sortSessions(list: readonly SessionInfo[]): SessionInfo[] {
  return [...list].sort(
    (a, b) =>
      Number(b.approvals > 0) - Number(a.approvals > 0) ||
      (b.last_activity ?? 0) - (a.last_activity ?? 0) ||
      (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
  );
}

/** What a running session is doing, from its NOW line ("Running <tool> <target>",
 * "Writing reply", "Thinking"). */
function runningActivity(now: string | null | undefined): Activity {
  const match = /^Running (\S+)(?: (.*))?$/s.exec(now ?? "");
  if (match?.[1]) return { kind: "tool", name: match[1], target: match[2] ?? "" };
  return { kind: "replying" };
}

/** The mascot pose of one session, by the same mapping as the terminal UI's. */
export function sessionPose(s: SessionInfo): PoseName {
  if (s.approvals > 0) return poseFor({ kind: "approval" });
  switch (s.phase) {
    case "running":
      return poseFor(runningActivity(s.now));
    case "stopped":
      return poseFor({ kind: "stopped" });
    default:
      return poseFor({ kind: "idle" });
  }
}

/** The header mascot: the worst state across the sessions. A Mac that cannot be reached is
 * asleep; otherwise an approval waiting, else the newest running session's pose, else idle. */
export function headerPose(list: readonly SessionInfo[], hub: HubState): PoseName {
  if (hub === "offline" || hub === "unpaired") return "sleeping";
  if (list.some((s) => s.approvals > 0)) return "approval";
  const running = sortSessions(list).find((s) => s.phase === "running");
  return running ? sessionPose(running) : "idle";
}
