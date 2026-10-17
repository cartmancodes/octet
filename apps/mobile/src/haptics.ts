// The two taps the app gives, behind one place: a light one on send, a success one on an allowed
// approval. A phone without a haptic engine, or a refused call, is silence, never an error.
import * as Haptics from "expo-haptics";

function quietly(run: () => Promise<void>): void {
  try {
    void Promise.resolve(run()).catch(() => undefined);
  } catch {
    // no haptics here
  }
}

/** A light tap: a prompt was sent. */
export const lightTap = (): void => quietly(() => Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light));

/** A success tap: an approval was allowed. */
export const successTap = (): void =>
  quietly(() => Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success));
