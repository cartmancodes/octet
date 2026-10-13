// The protocol's closed sets, as values, to check what the hub sends against. protocol.ts is
// generated and has them only as types; the `Missing` checks fail the build if one grows.
import type { Engine, Mode, Phase } from "../protocol";

export const ENGINES = ["claude", "codex", "demo"] as const satisfies readonly Engine[];
export const MODES = ["ask", "accept-edits", "auto", "full-access"] as const satisfies readonly Mode[];
export const PHASES = ["connecting", "idle", "running", "stopped"] as const satisfies readonly Phase[];

// Each of these is `never` while the list above is complete.
export type Missing = [Exclude<Engine, (typeof ENGINES)[number]>, Exclude<Mode, (typeof MODES)[number]>, Exclude<Phase, (typeof PHASES)[number]>];
export const NOTHING_MISSING: Missing extends [never, never, never] ? true : false = true;

/** `x` when it is one of `list`, else null. */
export function member<T extends string>(list: readonly T[], x: unknown): T | null {
  return typeof x === "string" && (list as readonly string[]).includes(x) ? (x as T) : null;
}

/** A count from the hub: a non-negative whole number, at most `max`. */
export function count(x: unknown, max = 1_000_000): number {
  return typeof x === "number" && Number.isFinite(x) ? Math.min(max, Math.max(0, Math.floor(x))) : 0;
}
