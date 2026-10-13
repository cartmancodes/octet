// Compile-only: `npm run typecheck` fails if the generated protocol stops
// narrowing journal events by their `type`. Nothing imports this file.
import { isKnownEvent, type JournalEvent, type RegisterPushCommand } from "./protocol";

export function text(e: JournalEvent): string {
  if (isKnownEvent(e) && e.type === "text") {
    const s: string = e.data;
    return s;
  }
  return "";
}

export function approvalId(e: JournalEvent): number | null {
  if (isKnownEvent(e) && e.type === "approval_closed") {
    const id: number = e.data;
    return id;
  }
  return null;
}

// Turning notifications off is said with an explicit null; the hub refuses an omitted token.
export const pushOff: RegisterPushCommand = { v: 1, type: "register_push", req: 1, token: null };
// @ts-expect-error `token` is required
export const pushOmitted: RegisterPushCommand = { v: 1, type: "register_push", req: 1 };
