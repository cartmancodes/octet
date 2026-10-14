// Whether the side drawer is open: a zustand store without React in it, so the ☰ buttons, the edge
// swipe and the screens under it (which must not answer an approval while it is up) all read one
// truth. The drawer itself is src/components/Drawer.tsx, mounted once in app/_layout.tsx.
import { createStore, useStore } from "zustand";

import { hideToast } from "./toast";

interface DrawerState {
  open: boolean;
  /** The screen showing has a drawer (see `drawerScreen`). Set by the shell. */
  enabled: boolean;
}

export const drawerStore = createStore<DrawerState>()(() => ({ open: false, enabled: false }));

/**
 * The screens that have the drawer: the Inbox, a Conversation and Settings. On any other (Pair,
 * New session) it is closed and stays closed. `pathname` is expo-router's.
 */
export const drawerScreen = (pathname: string): boolean => pathname === "/" || pathname === "/settings" || /^\/session\/[^/]+$/.test(pathname);

/** Whether the screen showing has a drawer; one that has not closes it. */
export function setDrawerEnabled(enabled: boolean): void {
  drawerStore.setState(enabled ? { enabled } : { enabled, open: false });
}

/** Opens the drawer, and takes a toast down (it would sit under the scrim). Does nothing where
 * there is no drawer. */
export function openDrawer(): void {
  const { open, enabled } = drawerStore.getState();
  if (open || !enabled) return;
  hideToast();
  drawerStore.setState({ open: true });
}

export function closeDrawer(): void {
  if (drawerStore.getState().open) drawerStore.setState({ open: false });
}

/** Whether the drawer is open. */
export const useDrawerOpen = (): boolean => useStore(drawerStore, (s) => s.open);

/** Whether the screen showing has a drawer. */
export const useDrawerEnabled = (): boolean => useStore(drawerStore, (s) => s.enabled);
