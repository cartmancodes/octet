import { useNavigationContainerRef, usePathname, useRouter } from "expo-router";
import { memo, useEffect, useRef } from "react";

import { openSession } from "../navigation";
import type { SessionInfo } from "../protocol";
import { sessionName } from "../sessions";
import { useListed, useSessions } from "../store";
import { showToast } from "../toast";

/** The words on the toast, before its View. */
export const APPROVAL_TOAST = " needs approval";
/** The toast's action. */
const VIEW = "View";

/**
 * The sessions of `next` that an approval newly came to since `before`, in the list's order: the
 * count of waiting approvals rose, or the count held and the front approval is another one. A fall
 * in the count (the Mac answered one, and the next became the front) is not news, and neither is
 * a front approval that `toasted` (the last front id each session showed) already has:
 * a session's row can lose its front for a moment and show it again. O(n).
 */
export function freshApprovals(
  before: readonly SessionInfo[],
  next: readonly SessionInfo[],
  toasted: ReadonlyMap<string, number> = NONE,
): SessionInfo[] {
  const was = new Map(before.map((s) => [s.id, s]));
  const fresh: SessionInfo[] = [];
  for (const s of next) {
    if (s.approvals <= 0) continue;
    const old = was.get(s.id);
    const from = old?.approvals ?? 0;
    const front = s.approval?.id ?? null;
    const announced = front !== null && toasted.get(s.id) === front;
    if (s.approvals > from) {
      // From nothing, the same front again is the one already announced; one more behind a front is new.
      if (from > 0 || !announced) fresh.push(s);
    } else if (s.approvals === from && front !== null && front !== (old?.approval?.id ?? null) && !announced) {
      fresh.push(s);
    }
  }
  return fresh;
}

const NONE: ReadonlyMap<string, number> = new Map();

/** The first of `freshApprovals`, or null. */
export const newApproval = (before: readonly SessionInfo[], next: readonly SessionInfo[], toasted?: ReadonlyMap<string, number>): SessionInfo | null =>
  freshApprovals(before, next, toasted)[0] ?? null;

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
  const nav = useNavigationContainerRef();
  const pathname = usePathname();
  const sessions = useSessions();
  const listed = useListed();
  // The latest screen, for the effect below, which runs on the sessions only.
  const here = useRef(pathname);
  useEffect(() => {
    here.current = pathname;
  });
  const seen = useRef<{ listed: boolean; sessions: readonly SessionInfo[] }>({ listed: false, sessions: [] });
  // The front approval each session last showed, so that one returning after a gap is not news.
  const toasted = useRef(new Map<string, number>());

  useEffect(() => {
    const before = seen.current;
    seen.current = { listed, sessions };
    const memory = toasted.current;
    // A session with nothing waiting is forgotten, and one gone from the list.
    const live = new Set(sessions.filter((x) => x.approvals > 0).map((x) => x.id));
    for (const id of memory.keys()) if (!live.has(id)) memory.delete(id);
    // The first listing is the baseline: what waits then is not news.
    const news = listed && before.listed ? freshApprovals(before.sessions, sessions, memory) : [];
    for (const x of sessions) if (x.approvals > 0 && x.approval) memory.set(x.id, x.approval.id);
    const fresh = news[0];
    if (!fresh || here.current === "/" || onScreen(here.current) === fresh.id) return;
    const id = fresh.id;
    // View opens it as the drawer does: back to it when it is in the stack already.
    showToast(`${sessionName(fresh.workspace)}${APPROVAL_TOAST}`, () => openSession(router, nav.getRootState(), here.current, id), VIEW);
  }, [sessions, listed, router, nav]);

  return null;
});
