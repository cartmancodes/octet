// What the Composer, the Steer field and the controls share: sending one command at a time, and
// the words for a command that did not work.
import { useCallback, useEffect, useRef, useState } from "react";

import { Refusal, type RequestBody } from "../client/HubClient";
import { lightTap } from "../haptics";

/** Sends a command to the Mac; resolves on its ack, rejects with a `Refusal`. */
export type Send = (body: RequestBody) => Promise<unknown>;

/** The hub's PROMPT_LIMIT: a prompt or steer longer than this many UTF-8 bytes is refused. */
export const PROMPT_LIMIT = 65_536;
export const TOO_LONG = "That's too long to send (64 KB max).";
export const LOST = "Not sure this reached the Mac. Check the session before sending again.";
export const SLASH_NOTE = "Slash commands run on the desk";

/** What to tell the user about a failed command, and whether to offer "Try again". */
export interface Failure {
  text: string;
  retry: boolean;
}

/**
 * `unsent` never left the phone, so sending again is safe: "Try again" (for prompts and steers).
 * `lost` may have reached the Mac: never resent for the user. Any other refusal is the hub's own
 * words, shown as they are.
 */
export function failure(error: unknown): Failure {
  if (error instanceof Refusal) {
    if (error.code === "lost") return { text: LOST, retry: false };
    return { text: error.message, retry: error.code === "unsent" };
  }
  return { text: "Something went wrong. Try again.", retry: false };
}

/** The length of `text` once UTF-8 encoded. */
export function utf8Bytes(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 0x80) bytes += 1;
    else if (c < 0x800) bytes += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < text.length) {
      bytes += 4; // a surrogate pair is one character of four bytes
      i += 1;
    } else bytes += 3;
  }
  return bytes;
}

/** Text that starts with `/` is a slash command, which only the terminal UI runs. */
export const isSlash = (text: string): boolean => text.trimStart().startsWith("/");

/** Whether the part of the screen is still there, for a result that comes after it left. */
export function useAlive(): () => boolean {
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  return useCallback(() => alive.current, []);
}

export interface Action {
  busy: boolean;
  failed: Failure | null;
  /** Sends `body` unless a send is out; `onAck` runs when the Mac acks it. */
  run: (body: RequestBody, onAck?: () => void) => void;
  fail: (failed: Failure | null) => void;
}

/** One command at a time: busy while it is out, and its failure after. */
export function useAction(send: Send): Action {
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<Failure | null>(null);
  const out = useRef(false);
  const alive = useAlive();
  const run = useCallback(
    (body: RequestBody, onAck?: () => void) => {
      if (out.current) return;
      out.current = true;
      setBusy(true);
      setFailed(null);
      send(body).then(
        () => {
          out.current = false;
          if (!alive()) return;
          setBusy(false);
          onAck?.();
        },
        (error: unknown) => {
          out.current = false;
          if (!alive()) return;
          setBusy(false);
          setFailed(failure(error));
        },
      );
    },
    [send, alive],
  );
  return { busy, failed, run, fail: setFailed };
}

export interface TextSender {
  text: string;
  setText: (text: string) => void;
  busy: boolean;
  failed: Failure | null;
  slash: boolean;
  canSend: boolean;
  /** Sends the text as the command. It clears only after the ack. */
  submit: () => void;
}

/** A text box that sends its text as a `prompt` or a `steer` for `session`. */
export function useTextSender(send: Send, session: string, kind: "prompt" | "steer", onSent?: () => void): TextSender {
  const [text, setTextState] = useState("");
  const { busy, failed, run, fail } = useAction(send);
  const slash = isSlash(text);
  const canSend = !busy && text.trim() !== "" && !slash;
  const setText = useCallback(
    (next: string) => {
      setTextState(next);
      fail(null);
    },
    [fail],
  );
  const submit = useCallback(() => {
    if (!canSend) return;
    if (utf8Bytes(text) > PROMPT_LIMIT) {
      fail({ text: TOO_LONG, retry: false });
      return;
    }
    lightTap();
    const sent = text;
    run({ type: kind, session, text: sent }, () => {
      // Words typed while it was out are the user's: only an unchanged box is cleared.
      setTextState((current) => (current === sent ? "" : current));
      onSent?.();
    });
  }, [canSend, text, kind, session, run, fail, onSent]);
  return { text, setText, busy, failed, slash, canSend, submit };
}
