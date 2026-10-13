// Compile-only: `npm run typecheck` fails if the generated protocol stops
// narrowing journal events by their `type`. Nothing imports this file.
import { isKnownEvent, type JournalEvent } from "./protocol";

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
