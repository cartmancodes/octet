// The one toast the app shows at a time: a short line at the top, with at most one action. A
// zustand store without React in it, so anything can show one; the host in app/_layout.tsx draws
// it (src/ui/Toast.tsx).
import { createStore, useStore } from "zustand";

/** How long a toast stays up. */
export const TOAST_MS = 3_400;

export interface ToastInfo {
  text: string;
  /** What the action button does; it closes the toast too. */
  action?: () => void;
  /** The action button's words. */
  label?: string;
  /** Tells one toast from the next with the same words. */
  seq: number;
  /** The Mac whose session the toast is about, when it is about one: it goes with that Mac. */
  mac?: string;
}

interface ToastState {
  toast: ToastInfo | null;
}

export const toastStore = createStore<ToastState>()(() => ({ toast: null }));

let seq = 0;
let timer: ReturnType<typeof setTimeout> | null = null;
/** A screen reader runs: the toast stays until it is dismissed or replaced, since the user may be
 * reading it (React Native says nothing of where a screen reader's focus is). */
let held = false;

const clear = () => {
  if (timer !== null) clearTimeout(timer);
  timer = null;
};

/** Takes the toast that is up down `TOAST_MS` from now, unless toasts are held. */
function schedule(): void {
  clear();
  const toast = toastStore.getState().toast;
  if (!toast || held) return;
  const mine = toast.seq;
  timer = setTimeout(() => {
    timer = null;
    if (toastStore.getState().toast?.seq === mine) toastStore.setState({ toast: null });
  }, TOAST_MS);
}

/** While `on`, a toast does not go by itself; once off, the one up goes `TOAST_MS` later. */
export function holdToasts(on: boolean): void {
  if (held === on) return;
  held = on;
  schedule();
}

/** Shows `text` in place of any toast up, for `TOAST_MS` (or, held, until it is dismissed).
 * `mac` names the Mac it is about, so that unpairing that Mac takes it down. */
export function showToast(text: string, action?: () => void, label?: string, mac?: string): void {
  seq += 1;
  const toast: ToastInfo = { text, seq };
  if (action && label) {
    toast.action = action;
    toast.label = label;
  }
  if (mac !== undefined) toast.mac = mac;
  toastStore.setState({ toast });
  schedule();
}

/** Takes the toast down. */
export function hideToast(): void {
  clear();
  if (toastStore.getState().toast !== null) toastStore.setState({ toast: null });
}

/** Takes down the toast about Mac `mac` (with none, about any Mac), if one is up. */
export function hideToastOf(mac?: string): void {
  const up = toastStore.getState().toast?.mac;
  if (up !== undefined && (mac === undefined || up === mac)) hideToast();
}

/** The toast up now, or null. */
export const useToast = (): ToastInfo | null => useStore(toastStore, (s) => s.toast);
