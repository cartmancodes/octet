// Asking a conversation's ▾ menu to open from outside its header: the prompt line's `/model`.
// Listeners by session, with no React in it; the header listens while it is mounted.

const listeners = new Map<string, Set<() => void>>();

/** Calls `listener` whenever something asks `session`'s menu to open. Returns the way to stop. */
export function onMenuRequest(session: string, listener: () => void): () => void {
  let set = listeners.get(session);
  if (!set) {
    set = new Set();
    listeners.set(session, set);
  }
  set.add(listener);
  return () => {
    const now = listeners.get(session);
    if (!now) return;
    now.delete(listener);
    if (now.size === 0) listeners.delete(session);
  };
}

/** Asks `session`'s menu to open. False when no header of it is listening. */
export function requestMenu(session: string): boolean {
  const set = listeners.get(session);
  if (!set || set.size === 0) return false;
  for (const listener of [...set]) listener();
  return true;
}
