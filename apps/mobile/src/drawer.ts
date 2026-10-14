// Whether the side drawer is open: a zustand store without React in it, so the ☰ buttons, the edge
// swipe and the screens under it (which must not answer an approval while it is up) all read one
// truth. The drawer itself is src/components/Drawer.tsx, mounted once in app/_layout.tsx.
import { createStore, useStore } from "zustand";

import { hideToast } from "./toast";

interface DrawerState {
  open: boolean;
  /** The screen showing has a drawer (see `drawerScreen`). Set by the shell. */
  enabled: boolean;
  /** Where the edge swipe's strip ends: the top of the conversation's prompt line and key bar,
   * in window points, as the screen showing measured it (it moves with the keyboard). Null when
   * no screen has: the strip then ends a fixed offset over the safe area. */
  edge: { owner: object; bottom: number } | null;
}

export const drawerStore = createStore<DrawerState>()(() => ({ open: false, enabled: false, edge: null }));

/** The screen `owner` (any object of its own) says where its controls begin. */
export function setEdgeBottom(owner: object, bottom: number): void {
  const edge = drawerStore.getState().edge;
  if (edge?.owner === owner && edge.bottom === bottom) return;
  drawerStore.setState({ edge: { owner, bottom } });
}

/** The screen `owner` left or lost its controls. A bound another screen set since is kept. */
export function clearEdgeBottom(owner: object): void {
  if (drawerStore.getState().edge?.owner === owner) drawerStore.setState({ edge: null });
}

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

/** The measured end of the edge strip, or null. */
export const useEdgeBottom = (): number | null => useStore(drawerStore, (s) => s.edge?.bottom ?? null);
