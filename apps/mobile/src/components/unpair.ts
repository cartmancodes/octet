// Unpairing one Mac, from Settings › Machines or from Pair: the question asked first, and what a
// screen showing that Mac's session does once it is gone.
import { useIsFocused, useRouter } from "expo-router";
import { useEffect, useState } from "react";
import { Alert } from "react-native";

import { useFleetStatus, useIsPaired } from "../hub";
import { macOfKey } from "../macs";

/** The question before Mac `name` is unpaired. */
export const unpairTitle = (name: string): string => `Unpair ${name}?`;
export const UNPAIR_BODY = "Its sessions leave this phone.";

/** Asks, with the system's sheet, whether to unpair Mac `name`; `unpair` runs on its destructive
 * Unpair, nothing on Cancel. */
export function confirmUnpair(name: string, unpair: () => void): void {
  Alert.alert(unpairTitle(name), UNPAIR_BODY, [
    { text: "Cancel", style: "cancel" },
    { text: "Unpair", style: "destructive", onPress: unpair },
  ]);
}

/**
 * For a screen showing session `key` (a Conversation, Changes, Usage): once the Mac of the key,
 * paired when the screen saw it, is not any more (the user unpaired it, or it unpaired this
 * phone), the screen goes back to the Inbox, as soon as it is in front. A screen under another
 * Mac's goes when that one is left. With no Mac left, the shell shows Pair instead. A key whose Mac
 * the screen never saw paired (a stale link) stays, to show the session gone.
 */
export function useInboxWhenUnpaired(key: string): void {
  const mac = macOfKey(key);
  const paired = useIsPaired(mac);
  const status = useFleetStatus();
  const focused = useIsFocused();
  const router = useRouter();
  // The Mac this screen saw paired: the value from the previous render, kept in state.
  const [seen, setSeen] = useState<string | null>(paired ? mac : null);
  if (paired && mac !== seen) setSeen(mac);
  const gone = mac !== null && seen === mac && !paired && status === "paired";
  useEffect(() => {
    if (gone && focused) router.dismissTo("/");
  }, [gone, focused, router]);
}
