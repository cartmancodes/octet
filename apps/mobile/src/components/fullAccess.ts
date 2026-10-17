// What turning on full access shares between the mode sheet and a new session: the words, and the
// guard that makes Face ID and the typed words approve exactly one thing, once.
import { useCallback, useEffect, useRef, useState } from "react";
import { AppState } from "react-native";

import { confirmBiometric } from "../auth";
import { useAlive, type Failure } from "./send";

/** What the user types, exactly, to turn full access on. */
export const FULL_ACCESS_WORDS = "full access";
export const NO_FACE_ID = "Full access needs Face ID set up on this phone.";
export const FULL_ACCESS_WARNING = "The agent can run any command without asking.";

export interface FullAccessArm {
  /** Face ID passed for the form as it is now: the typed-words panel may show. */
  armed: boolean;
  /** A Face ID prompt is up. */
  checking: () => boolean;
  /** Asks for Face ID, always, whatever the Face ID setting says, for what `key` names. Arms only
   * if Face ID passes and the form still has the same `key` when it answers. */
  arm: () => Promise<void>;
  /** Forgets the approval, and any prompt still up: its answer will arm nothing. */
  disarm: () => void;
}

/**
 * Face ID, then (in the panel the caller draws while `armed`) the typed words, for one request.
 * `key` names exactly what Face ID approves (the session and mode for the sheet; engine,
 * workspace, model and resume for a new session). The approval is for that key only: any change
 * of the key disarms it and voids a prompt still up, as do `disarm` (call it on a send, on a
 * close), a refusal (`failed`) and unmounting. Leaving the app disarms it too. So the words can never send anything but what
 * Face ID was asked about, and never twice.
 */
export function useFullAccessArm(opts: {
  reason: string;
  key: string;
  /** The last command's failure; a new one disarms. */
  failed: Failure | null;
  /** Shows (or clears) a failure; used for a phone with no Face ID. */
  fail: (failed: Failure | null) => void;
}): FullAccessArm {
  const { reason, key, failed, fail } = opts;
  const alive = useAlive();
  const [armedKey, setArmedKey] = useState<string | null>(null);
  // Moves on at every disarm and key change: a prompt asked under an older number belongs to nobody.
  const generation = useRef(0);
  const prompting = useRef(false);
  const current = useRef(key);

  const invalidate = useCallback(() => {
    generation.current += 1;
    prompting.current = false;
  }, []);

  useEffect(() => {
    current.current = key;
    invalidate();
  }, [key, invalidate]);

  // The key moved under an approval, or a refusal came: disarm.
  const [seenFailure, setSeenFailure] = useState(failed);
  if (seenFailure !== failed) {
    setSeenFailure(failed);
    if (failed && armedKey !== null) setArmedKey(null);
  }
  if (armedKey !== null && armedKey !== key) setArmedKey(null);

  const disarm = useCallback(() => {
    invalidate();
    setArmedKey(null);
  }, [invalidate]);

  const arm = useCallback(async () => {
    if (prompting.current || AppState.currentState !== "active") return;
    const mine = generation.current;
    const asked = current.current;
    prompting.current = true;
    try {
      const checked = await confirmBiometric(reason);
      if (!alive() || generation.current !== mine || current.current !== asked) return;
      if (checked === "ok") setArmedKey(asked);
      else if (checked === "unavailable") fail({ text: NO_FACE_ID, retry: false });
    } finally {
      if (generation.current === mine) prompting.current = false;
    }
  }, [reason, alive, fail]);

  // Leaving the app (the background, or the app switcher: anything but active) forgets a pass:
  // whoever comes back finds the words are not enough. The Face ID prompt itself makes the app
  // inactive, so a pass still being asked for is left alone.
  useEffect(() => {
    const subscription = AppState.addEventListener("change", (state) => {
      // The background ends it all, a prompt still up included; inactive (the app switcher, or
      // the prompt itself) spares only a prompt still up.
      if (state === "background" || (state !== "active" && !prompting.current)) disarm();
    });
    return () => subscription.remove();
  }, [disarm]);

  const checking = useCallback(() => prompting.current, []);
  return { armed: armedKey !== null && armedKey === key, checking, arm, disarm };
}
