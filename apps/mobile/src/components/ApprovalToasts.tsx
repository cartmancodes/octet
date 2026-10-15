import { usePathname, useRouter } from "expo-router";
import { memo, useEffect, useRef } from "react";

import type { SessionInfo } from "../protocol";
import { sessionName } from "../sessions";
import { useListed, useSessions } from "../store";
import { showToast } from "../toast";

/** The words on the toast, before its View. */
export const APPROVAL_TOAST = " needs approval";
/** The toast's action. */
const VIEW = "View";

/**
 * The first session of `next` that an approval newly came to since `before`: its count of waiting
 * approvals rose, or its front approval is another one. A session the phone did not know before has
 * no approvals behind it, so one with an approval waiting counts. O(n).
 */
export function newApproval(before: readonly SessionInfo[], next: readonly SessionInfo[]): SessionInfo | null {
  const was = new Map(before.map((s) => [s.id, s]));
  for (const s of next) {
    if (s.approvals <= 0) continue;
    const old = was.get(s.id);
    if (!old || s.approvals > old.approvals || (s.approval != null && s.approval.id !== old.approval?.id)) return s;
  }
  return null;
}

/** The session a Conversation path shows, or null on any other screen. */
const SESSION_PATH = /^\/session\/([^/]+)$/;
function onScreen(pathname: string): string | null {
  const id = SESSION_PATH.exec(pathname)?.[1];
  if (id === undefined) return null;
  try {
    return decodeURIComponent(id);
  } catch {
    return id;
  }
}

/**
 * Mounted once, in the shell: when an approval newly comes to a session that is not on screen,
 * a toast says so with a View that opens it. Not on the Inbox, which shows it as a card, and not
 * for the Conversation on screen, which shows its Permission card. The sessions the phone first
 * hears of when it connects are the baseline: approvals already waiting then are not news. Draws nothing.
 */
export const ApprovalToasts = memo(function ApprovalToasts() {
  const router = useRouter();
  const pathname = usePathname();
  const sessions = useSessions();
  const listed = useListed();
  // The latest screen, for the effect below, which runs on the sessions only.
  const here = useRef(pathname);
  useEffect(() => {
    here.current = pathname;
  });
  const seen = useRef<{ listed: boolean; sessions: readonly SessionInfo[] }>({ listed: false, sessions: [] });

  useEffect(() => {
    const before = seen.current;
    seen.current = { listed, sessions };
    if (!listed || !before.listed) return;
    const fresh = newApproval(before.sessions, sessions);
    if (!fresh || here.current === "/" || onScreen(here.current) === fresh.id) return;
    const id = fresh.id;
    showToast(
      `${sessionName(fresh.workspace)}${APPROVAL_TOAST}`,
      () => router.push({ pathname: "/session/[id]", params: { id } }),
      VIEW,
    );
  }, [sessions, listed, router]);

  return null;
});
