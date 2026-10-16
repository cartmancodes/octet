// Opening a conversation from anywhere (a drawer row, a toast's View, a notification tap, the
// Island's Allow) the one way: never a second copy of a conversation already in the stack. Plain
// TypeScript.
import type { useRouter } from "expo-router";

import { isHostName, isSessionId } from "./macs";

type Router = Pick<ReturnType<typeof useRouter>, "push" | "replace" | "dismiss">;

/** A navigator's state, as much of it as this reads. */
export interface NavState {
  index?: number;
  routeNames?: string[];
  routes: { name: string; params?: object; state?: unknown }[];
}

/** The Conversation's route. */
const SESSION_ROUTE = "session/[id]";

/** Whether `pathname` (expo-router's) is the Conversation of `id`, a session key: its `/` comes
 * decoded in the path, or percent-encoded. */
export const isSessionPath = (pathname: string, id: string): boolean =>
  pathname === `/session/${id}` || pathname === `/session/${encodeURIComponent(id)}`;

const isState = (x: unknown): x is NavState =>
  typeof x === "object" && x !== null && Array.isArray((x as { routes?: unknown }).routes);

/** The stack that holds the Conversations, down the focused routes from `state`; null if none. */
function sessionStack(state: unknown): NavState | null {
  for (let depth = 0; isState(state) && depth < 8; depth++) {
    if (state.routeNames?.includes(SESSION_ROUTE) || state.routes.some((r) => r.name === SESSION_ROUTE)) return state;
    state = state.routes[state.index ?? state.routes.length - 1]?.state;
  }
  return null;
}

/**
 * Opens the Conversation of `id`. Already on it: nothing. In the stack below the screen showing:
 * back to it, so there is one copy of each conversation (and one header to answer `/model`). From
 * another conversation: in its place, so back still returns to where that one came from.
 * Otherwise on top. `root` is the navigation's root state now; `pathname` the screen showing.
 */
export function openSession(router: Router, root: unknown, pathname: string, id: string): void {
  if (isSessionPath(pathname, id)) return;
  const stack = sessionStack(root);
  if (stack) {
    const at = stack.index ?? stack.routes.length - 1;
    for (let i = at - 1; i >= 0; i--) {
      const route = stack.routes[i];
      if (route?.name === SESSION_ROUTE && (route.params as { id?: unknown } | undefined)?.id === id) {
        router.dismiss(at - i);
        return;
      }
    }
  }
  const target = { pathname: "/session/[id]", params: { id } } as const;
  if (pathname.startsWith("/session/")) router.replace(target);
  else router.push(target);
}

/** What the Island's Allow names: `octet://approve/<host>/<raw session>/<approval>`. `host` is the
 * hub's published host, null for none (an empty part: the only pairing's); `raw` the hub's own
 * session id; `approval` its id. */
export interface ApproveLink {
  host: string | null;
  raw: string;
  approval: number;
}

/** The scheme and action of an approve link, as the widget builds it. */
const APPROVE = /^octet:\/\/approve\//i;
/** An approval id as the hub numbers them: a whole number, written out in digits. */
const APPROVAL_ID = /^[0-9]{1,16}$/;

/** The approval id `text` names, or null for one that is not a whole number the phone can hold. */
export function parseApprovalId(text: string): number | null {
  if (!APPROVAL_ID.test(text)) return null;
  const id = Number(text);
  return Number.isSafeInteger(id) ? id : null;
}

/** Whether `url` is meant as an approve link (well formed or not): such a link only ever opens the
 * Inbox, and maybe the approval's session over it. */
export const isApproveUrl = (url: string): boolean => APPROVE.test(url);

/** The parts of an approve link, each percent-decoded and checked: the host by the push rule
 * (`isHostName`, or empty), the session by `isSessionId`, the approval by `parseApprovalId`. Null
 * for anything else: another scheme or action, a part missing or extra, a query or fragment, or a
 * part that fails its rule. */
export function parseApproveLink(url: string): ApproveLink | null {
  if (!isApproveUrl(url)) return null;
  const parts = url.replace(APPROVE, "").split("/");
  if (parts.length !== 3) return null;
  let decoded: string[];
  try {
    decoded = parts.map((part) => decodeURIComponent(part));
  } catch {
    return null; // bad percent-encoding
  }
  const [host = "", raw = "", approval = ""] = decoded;
  if (host !== "" && !isHostName(host)) return null;
  if (!isSessionId(raw)) return null;
  const id = parseApprovalId(approval);
  return id === null ? null : { host: host === "" ? null : host, raw, approval: id };
}

/** An approve link that came before anything listened (a cold start from the Island), kept for the
 * first listener. Only the newest is kept. */
let heldLink: string | null = null;
const linkListeners = new Set<(url: string) => void>();

/** The system opened an approve link (`app/+native-intent.ts`): handed to the shell, or kept until
 * it listens. */
export function handOverApproveLink(url: string): void {
  if (linkListeners.size === 0) {
    heldLink = url;
    return;
  }
  for (const fn of [...linkListeners]) fn(url);
}

/** Calls `fn` with each approve link the system opens, the one held from before first; returns the
 * stop. */
export function watchApproveLinks(fn: (url: string) => void): () => void {
  linkListeners.add(fn);
  const held = heldLink;
  heldLink = null;
  if (held !== null) fn(held);
  return () => void linkListeners.delete(fn);
}
