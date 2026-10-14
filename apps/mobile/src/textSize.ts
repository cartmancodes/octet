// The transcript's text size, which the Conversation header's `Aa` cycles: a zustand store without
// React in it, so `Aa` can change it without subscribing (the header never draws again for it)
// while the transcript, which does subscribe, draws at the new size. Saved in settings.
import { createStore, useStore } from "zustand";

import { DEFAULT_SETTINGS, TEXT_SIZES, loadSettings, saveSettings, type TextSize } from "./persist";

export const textSizeStore = createStore<{ size: TextSize }>()(() => ({ size: DEFAULT_SETTINGS.textSize }));

/** The size after `size`: 12 → 13 → 14 → 12. */
export function nextTextSize(size: TextSize): TextSize {
  return TEXT_SIZES[(TEXT_SIZES.indexOf(size) + 1) % TEXT_SIZES.length] ?? DEFAULT_SETTINGS.textSize;
}

let loading: Promise<void> | null = null;
/** `Aa` was pressed: a saved size read after that is older than the user's choice. */
let chosen = false;

/** Reads the saved size, once per app run. */
export function loadTextSize(): Promise<void> {
  loading ??= loadSettings().then((settings) => {
    if (!chosen) textSizeStore.setState({ size: settings.textSize });
  });
  return loading;
}

/** Moves the transcript to the next size and saves it. Returns the new size. */
export function cycleTextSize(): TextSize {
  chosen = true;
  const size = nextTextSize(textSizeStore.getState().size);
  textSizeStore.setState({ size });
  void saveSettings({ textSize: size });
  return size;
}

/** The transcript's text size. */
export const useTextSize = (): TextSize => useStore(textSizeStore, (s) => s.size);

/** Back to the default, with nothing read yet: for tests, which share the module. */
export function resetTextSize(): void {
  loading = null;
  chosen = false;
  textSizeStore.setState({ size: DEFAULT_SETTINGS.textSize });
}
