import { useNavigationContainerRef, usePathname, useRouter } from "expo-router";
import { memo, useEffect, useRef } from "react";

import { openSession } from "../navigation";
import type { SessionInfo } from "../protocol";
import { sessionName } from "../sessions";
import { useListedMacs, useSessions, type ListedSession } from "../store";
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
export function freshApprovals<T extends SessionInfo>(
  before: readonly T[],
  next: readonly T[],
  toasted: ReadonlyMap<string, number> = NONE,
): T[] {
  const was = new Map(before.map((s) => [s.id, s]));
  const fresh: T[] = [];
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
const NO_MACS: ReadonlySet<string> = new Set();

/** The session a Conversation path shows, or null on any other screen. The path holds the key,
 * `/` and all (expo-router gives it decoded), or the key percent-encoded. */
const SESSION_PATH = /^\/session\/(.+)$/;
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
 * for the Conversation on screen, which shows its Permission card. Each Mac's first list (since
 * it was paired, or paired again) is its baseline: approvals already waiting then are not news,
 * whenever that Mac comes. Draws nothing.
 */
export const ApprovalToasts = memo(function ApprovalToasts() {
  const router = useRouter();
  const nav = useNavigationContainerRef();
  const pathname = usePathname();
  const sessions = useSessions();
  const listed = useListedMacs();
  // The latest screen, for the effect below, which runs on the sessions only.
  const here = useRef(pathname);
  useEffect(() => {
    here.current = pathname;
  });
  const seen = useRef<{ listed: ReadonlySet<string>; sessions: readonly ListedSession[] }>({ listed: NO_MACS, sessions: [] });
  // The front approval each session last showed, so that one returning after a gap is not news.
  const toasted = useRef(new Map<string, number>());

  useEffect(() => {
    const before = seen.current;
    seen.current = { listed, sessions };
    const memory = toasted.current;
    // A session with nothing waiting is forgotten, and one gone from the list.
    const live = new Set(sessions.filter((x) => x.approvals > 0).map((x) => x.id));
    for (const id of memory.keys()) if (!live.has(id)) memory.delete(id);
    // Each Mac's first listing is its baseline: what waits then is not news. A Mac counts as
    // listed before only when it was listed at the last look too (a reset in between starts over).
    const known = (mac: string) => listed.has(mac) && before.listed.has(mac);
    const news = freshApprovals(before.sessions, sessions, memory).filter((x) => known(x.mac));
    for (const x of sessions) if (x.approvals > 0 && x.approval) memory.set(x.id, x.approval.id);
    const fresh = news[0];
    if (!fresh || here.current === "/" || onScreen(here.current) === fresh.id) return;
    const id = fresh.id;
    // View opens it as the drawer does: back to it when it is in the stack already.
    showToast(`${sessionName(fresh.workspace)}${APPROVAL_TOAST}`, () => openSession(router, nav.getRootState(), here.current, id), VIEW, fresh.mac);
  }, [sessions, listed, router, nav]);

  return null;
});
