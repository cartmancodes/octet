import type { PoseName } from "./poses";

/** What a session is doing, as the mascot cares about it. */
export type Activity =
  | { kind: "idle" }
  | { kind: "approval" }
  | { kind: "tool"; name: string; target: string }
  | { kind: "replying" }
  | { kind: "finished" }
  | { kind: "failed" }
  | { kind: "stopped" };

// These mirror `mascot::State::tool` in crates/octet-tui/src/mascot.rs: the
// tool's name and command only, never its output.
const DELEGATE_WORDS = ["dispatch", "delegate", "spawn_agent", "task", "collabagent"];
const SEARCH_WORDS = ["search", "read", "glob", "grep"];
const SEARCH_COMMANDS = ["rg ", "grep ", "find ", "cat ", "ls"];

/** The pose for an activity. */
export function poseFor(activity: Activity): PoseName {
  switch (activity.kind) {
    case "approval":
      return "approval";
    case "replying":
      return "thinking";
    case "finished":
      return "success";
    case "failed":
      return "error";
    case "stopped":
      return "sleeping";
    case "idle":
      return "idle";
    case "tool": {
      const name = activity.name.toLowerCase();
      const command = activity.target.trim();
      if (DELEGATE_WORDS.some((w) => name.includes(w))) return "delegating";
      if (
        SEARCH_WORDS.some((w) => name.includes(w)) ||
        SEARCH_COMMANDS.some((p) => command.startsWith(p))
      ) {
        return "searching";
      }
      return "coding";
    }
  }
}
