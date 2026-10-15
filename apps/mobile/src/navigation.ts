// Opening a conversation from anywhere (a drawer row, a toast's View, a notification tap) the one
// way: never a second copy of a conversation already in the stack. Plain TypeScript.
import type { useRouter } from "expo-router";

type Router = Pick<ReturnType<typeof useRouter>, "push" | "replace" | "dismiss">;

/** A navigator's state, as much of it as this reads. */
export interface NavState {
  index?: number;
  routeNames?: string[];
  routes: { name: string; params?: object; state?: unknown }[];
}

/** The Conversation's route. */
const SESSION_ROUTE = "session/[id]";

/** Whether `pathname` (expo-router's) is the Conversation of `id`. */
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
