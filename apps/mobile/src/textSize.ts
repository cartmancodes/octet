// The transcript's text size, which the Conversation header's `Aa` cycles: a zustand store without
// React in it, so `Aa` can change it without subscribing (the header never draws again for it)
// while the transcript, which does subscribe, draws at the new size. Saved in settings.
import { createStore, useStore } from "zustand";

import { DEFAULT_SETTINGS, TEXT_SIZES, loadSettings, saveSettings, type TextSize } from "./persist";

interface TextSizeState {
  size: TextSize;
  /** `Aa` was pressed: a saved size read after that is older than the user's choice. */
  chosen: boolean;
  /** The saved size being read, once per app run. */
  loading: Promise<void> | null;
}

export const textSizeStore = createStore<TextSizeState>()(() => ({ size: DEFAULT_SETTINGS.textSize, chosen: false, loading: null }));

/** The size after `size`: 12 → 13 → 14 → 12. */
export function nextTextSize(size: TextSize): TextSize {
  return TEXT_SIZES[(TEXT_SIZES.indexOf(size) + 1) % TEXT_SIZES.length] ?? DEFAULT_SETTINGS.textSize;
}

/** Reads the saved size, once per app run. */
export function loadTextSize(): Promise<void> {
  const { loading } = textSizeStore.getState();
  if (loading) return loading;
  const read = loadSettings().then((settings) => {
    if (!textSizeStore.getState().chosen) textSizeStore.setState({ size: settings.textSize });
  });
  textSizeStore.setState({ loading: read });
  return read;
}

/** Moves the transcript to the next size and saves it. Returns the new size. */
export function cycleTextSize(): TextSize {
  const size = nextTextSize(textSizeStore.getState().size);
  textSizeStore.setState({ size, chosen: true });
  void saveSettings({ textSize: size });
  return size;
}

/** The transcript's text size. */
export const useTextSize = (): TextSize => useStore(textSizeStore, (s) => s.size);
