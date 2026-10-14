// Answering an approval from the phone, wherever it shows (the sheet on a session's screen, an
// Inbox card, Allow all): the guards every Allow goes through. Allow is once only.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

import { confirmBiometric, type BiometricResult } from "./auth";
import { failure, useAlive, type Send } from "./components/send";
import { successTap } from "./haptics";
import { loadSettings } from "./persist";
import type { ApprovalInfo } from "./protocol";

/** Allow and Deny stay dead this long after a request comes into view: a tap meant for the one
 * before (or a thumb already on its way) must not answer the next. */
export const ARM_MS = 400;
export const NO_FACE_ID = "Face ID isn't set up on this phone. Turn it off in Settings to allow from here.";
/** The system prompt's sentence is short; the action can be long. */
const REASON_CHARS = 80;

/** One request as the user saw it: its session, its id and what it asks. A request whose
 * summary changed (its journal record filled it in) is a new look, armed again. */
export const approvalKey = (session: string, approval: ApprovalInfo): string =>
  `${session}\u0000${approval.id}\u0000${JSON.stringify(approval.summary)}`;

/**
 * True `ARM_MS` after `key` came into view while `reachable`, and until either changes. The delay
 * counts from when it can be seen and answered: its arrival, the link coming back, or a sheet over
 * it closing. Unreachable, it is dead.
 */
export function useArm(key: string, reachable: boolean): boolean {
  const [armedFor, setArmedFor] = useState<string | null>(null);
  if (!reachable && armedFor !== null) setArmedFor(null);
  useEffect(() => {
    if (!reachable) return;
    const timer = setTimeout(() => setArmedFor(key), ARM_MS);
    return () => clearTimeout(timer);
  }, [reachable, key]);
  return reachable && armedFor === key;
}

/**
 * The answers out and given, shared by the surfaces of one screen (an Inbox card and Allow all),
 * so one request is never answered twice from it. Keyed by `approvalKey`.
 */
export interface AnswerLedger {
  /** True, and the request is marked out, unless an answer to it is out or was given. */
  claim(key: string): boolean;
  /** The answer is back: `answered` when the Mac took it, so it is not sent again. */
  finish(key: string, answered: boolean): void;
  /** Forgets the answered requests not in `keys`: an id that left and came back is a new
   * request (a vendor numbers its approvals from 1 again after a provider switch). */
  keep(keys: Iterable<string>): void;
}

export function createAnswerLedger(): AnswerLedger {
  const out = new Set<string>();
  const given = new Set<string>();
  return {
    claim(key) {
      if (out.has(key) || given.has(key)) return false;
      out.add(key);
      return true;
    },
    finish(key, answered) {
      out.delete(key);
      if (answered) given.add(key);
    },
    keep(keys) {
      const listed = new Set(keys);
      for (const key of given) if (!listed.has(key)) given.delete(key);
    },
  };
}

export interface ApprovalAnswerOptions {
  /** Must be stable. */
  send: Send;
  /** The Mac can't be reached: nothing can be answered. */
  offline?: boolean;
  /** Something is over the request (another sheet): nothing can be answered, and the arm delay
   * starts again once it is gone, so a tap that closed it answers nothing. */
  covered?: boolean;
  /** Whether the approval is still open: it may have been closed elsewhere while Face ID was up.
   * Must be stable. */
  isOpen: (id: number) => boolean;
  /** The Mac took the answer. */
  onAnswered?: (id: number) => void;
  /** Shared with the screen's other answer surfaces. */
  ledger?: AnswerLedger;
}

export interface ApprovalAnswer {
  /** The arm delay has passed and nothing is over it. */
  armed: boolean;
  /** Face ID is up, or the answer is out. */
  busy: boolean;
  /** The answer is out, waiting for its ack. */
  sending: boolean;
  /** Why the last answer did not go, in words; null when it did, or none was tried. */
  failure: string | null;
  /** Armed, not busy, reachable, and not already answered from here. */
  canAnswer: boolean;
  allow(): Promise<void>;
  deny(): Promise<void>;
}

/**
 * Answers `approval` of `session`, once. Allow asks for Face ID when the setting is on (read at
 * the tap, so a change in Settings counts at once). After Face ID the answer goes only if the
 * request is still on screen, still the same one (the generation), still open, and the Mac can
 * still be reached. Two taps send one answer.
 */
export function useApprovalAnswer(
  session: string,
  approval: ApprovalInfo | null,
  { send, offline = false, covered = false, isOpen, onAnswered, ledger }: ApprovalAnswerOptions,
): ApprovalAnswer {
  const alive = useAlive();
  const key = approval ? approvalKey(session, approval) : "";
  const reachable = approval !== null && !offline && !covered;
  const armed = useArm(key, reachable);
  // "checking": the biometric prompt is up. "sending": the answer is out, waiting for its ack.
  const [stage, setStage] = useState<"checking" | "sending" | null>(null);
  // Each error and answer belongs to the request it was for: the next one starts with neither.
  const [error, setError] = useState<{ key: string; text: string } | null>(null);
  const [answeredKey, setAnsweredKey] = useState<string | null>(null);
  if (approval === null && answeredKey !== null) setAnsweredKey(null);
  const out = useRef(false);
  // Read after Face ID: what is on screen now, and whether the link is up. Layout effects, not
  // passive ones: they run in the commit itself, so a change is seen at once by an answer that is
  // awaiting Face ID, with no frame in between.
  const generation = useRef(key);
  const down = useRef(offline);
  useLayoutEffect(() => {
    generation.current = key;
    down.current = offline;
  });

  const id = approval?.id;
  const action = approval?.summary.action ?? "";
  const answer = useCallback(
    async (allow: boolean) => {
      if (id === undefined || out.current || !armed || down.current || answeredKey === key) return;
      const asked = key;
      if (ledger && !ledger.claim(asked)) return;
      out.current = true;
      let given = false;
      setError(null);
      setStage("checking");
      try {
        if (allow) {
          const { faceId } = await loadSettings();
          if (faceId) {
            const checked = await confirmBiometric(`Allow: ${action.slice(0, REASON_CHARS)}`);
            if (checked === "unavailable") {
              if (alive()) setError({ key: asked, text: NO_FACE_ID });
              return;
            }
            if (checked === "failed") return; // cancelled or no match: nothing is sent
          }
        }
        // Face ID takes a while: send only if this request is still on screen, still the one the
        // user looked at, still open, and the Mac can still be reached.
        if (!alive() || generation.current !== asked || !isOpen(id) || down.current) return;
        setStage("sending");
        await send({ type: "answer", session, approval: id, allow });
        given = true;
        if (allow) successTap();
        if (alive()) setAnsweredKey(asked);
        onAnswered?.(id);
      } catch (e) {
        if (alive()) setError({ key: asked, text: failure(e).text });
      } finally {
        out.current = false;
        ledger?.finish(asked, given);
        if (alive()) setStage(null);
      }
    },
    [id, armed, key, answeredKey, action, session, send, isOpen, onAnswered, ledger, alive],
  );
  const allow = useCallback(() => answer(true), [answer]);
  const deny = useCallback(() => answer(false), [answer]);

  const busy = stage !== null;
  return {
    armed,
    busy,
    sending: stage === "sending",
    failure: error && error.key === key ? error.text : null,
    canAnswer: armed && !busy && reachable && answeredKey !== key,
    allow,
    deny,
  };
}

/** How Allow all ended: the batch was answered, or Face ID did not pass. */
export type AllowAllResult = "ok" | Exclude<BiometricResult, "ok">;

/**
 * Allow all: one Face ID pass (when the setting is on) for the whole batch, then `answerOnce` for
 * each item, in order. `answerOnce` checks that its request is still open and still armed, and
 * sends nothing otherwise; the answers go out in order without waiting on each other's acks, and
 * one that fails does not stop the rest.
 */
export async function allowAll<T>(items: readonly T[], answerOnce: (item: T) => Promise<unknown>): Promise<AllowAllResult> {
  if (items.length === 0) return "ok";
  const { faceId } = await loadSettings();
  if (faceId) {
    const checked = await confirmBiometric(`Allow ${items.length} ${items.length === 1 ? "request" : "requests"} once`);
    if (checked !== "ok") return checked;
  }
  const answers: Promise<unknown>[] = [];
  for (const item of items) answers.push(answerOnce(item).catch(() => undefined));
  await Promise.all(answers);
  return "ok";
}
