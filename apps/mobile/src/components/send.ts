// What the prompt line, the key bar and the other controls share: sending one command at a time,
// the limits on what is sent, and the words for a command that did not work.
import { useCallback, useEffect, useRef, useState } from "react";

import { Refusal, type RequestBody } from "../client/HubClient";
import { useHubClient } from "../hub";

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

/** A `Send` over the app's one connection; with none (not paired yet) every command is unsent.
 * Stable while the connection is. */
export function useSend(): Send {
  const client = useHubClient();
  return useCallback<Send>(
    (body) => (client ? client.request(body) : Promise.reject(new Refusal("unsent", "Can't reach your Mac"))),
    [client],
  );
}

const isLow = (c: number): boolean => c >= 0xdc00 && c <= 0xdfff;

/** The length of `text` once UTF-8 encoded. */
export function utf8Bytes(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 0x80) bytes += 1;
    else if (c < 0x800) bytes += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < text.length && isLow(text.charCodeAt(i + 1))) {
      bytes += 4; // a surrogate pair is one character of four bytes
      i += 1;
    } else bytes += 3; // the rest of the BMP, and a lone surrogate (U+FFFD when encoded)
  }
  return bytes;
}

/** The approval demo, which the desk sends as a prompt (`APPROVAL_DEMO`,
 * crates/octet-engine/src/live/mod.rs, re-exported by octet-core). */
export const APPROVAL_DEMO = "/approval-demo";

/** A slash command, which only the terminal UI runs: the desk's own rule (`is_command_line`,
 * crates/octet-tui/src/input.rs), which the bridge applies to the phone's text too. The trimmed
 * text starts with `/`, is not the approval demo, and its first word names a command: one with
 * a `/` in it is a path ("/usr/lib is broken"), so the text is a prompt. */
export function isSlash(text: string): boolean {
  const line = text.trim();
  if (!line.startsWith("/") || line === APPROVAL_DEMO) return false;
  const first = line.split(/\s/, 1)[0] ?? "";
  return !first.slice(1).includes("/");
}

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
  /** Sends `body` unless a send is out; `onAck` runs, with the Mac's answer, when it acks. True
   * if it sent. */
  run: (body: RequestBody, onAck?: (ack: unknown) => void) => boolean;
  fail: (failed: Failure | null) => void;
}

/** One command at a time: busy while it is out, and its failure after. */
export function useAction(send: Send): Action {
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<Failure | null>(null);
  const out = useRef(false);
  const alive = useAlive();
  const run = useCallback(
    (body: RequestBody, onAck?: (ack: unknown) => void) => {
      if (out.current) return false;
      out.current = true;
      setBusy(true);
      setFailed(null);
      send(body).then(
        (ack) => {
          out.current = false;
          if (!alive()) return;
          setBusy(false);
          onAck?.(ack);
        },
        (error: unknown) => {
          out.current = false;
          if (!alive()) return;
          setBusy(false);
          setFailed(failure(error));
        },
      );
      return true;
    },
    [send, alive],
  );
  return { busy, failed, run, fail: setFailed };
}
