// The one toast the app shows at a time: a short line at the bottom, with at most one action. A
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
}

interface ToastState {
  toast: ToastInfo | null;
}

export const toastStore = createStore<ToastState>()(() => ({ toast: null }));

let seq = 0;
let timer: ReturnType<typeof setTimeout> | null = null;

const clear = () => {
  if (timer !== null) clearTimeout(timer);
  timer = null;
};

/** Shows `text` in place of any toast up, for `TOAST_MS`. */
export function showToast(text: string, action?: () => void, label?: string): void {
  clear();
  seq += 1;
  const toast: ToastInfo = { text, seq };
  if (action && label) {
    toast.action = action;
    toast.label = label;
  }
  toastStore.setState({ toast });
  const mine = seq;
  timer = setTimeout(() => {
    timer = null;
    if (toastStore.getState().toast?.seq === mine) toastStore.setState({ toast: null });
  }, TOAST_MS);
}

/** Takes the toast down. */
export function hideToast(): void {
  clear();
  if (toastStore.getState().toast !== null) toastStore.setState({ toast: null });
}

/** The toast up now, or null. */
export const useToast = (): ToastInfo | null => useStore(toastStore, (s) => s.toast);
