// What the prompt line, the key bar and the other controls share: sending one command at a time,
// the limits on what is sent, and the words for a command that did not work.
import { useCallback, useEffect, useRef, useState } from "react";

import { Refusal, type Ack, type RequestBody } from "../client/HubClient";
import { useLinks } from "../hub";
import { splitKey, type Links } from "../macs";

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

export const UNREACHABLE = "Can't reach your Mac";
export const CHOOSE_MAC = "Choose a Mac";

/**
 * Sends `body` to the Mac it is for, through `links`, with the hub's raw id in place of the key
 * in its `session`. The Mac is the one of the body's `session` (a key), else of `key`; a body
 * that names neither goes to the only Mac. Never sent, refused as `unsent`: a session that is not
 * a key, a body for another Mac than `key`'s, no Mac connected, or no Mac named while there are
 * several.
 */
export function sendVia(links: Links, key: string | undefined, body: RequestBody): Promise<Ack> {
  const unsent = (words: string) => Promise.reject(new Refusal("unsent", words));
  const named = "session" in body ? body.session : undefined;
  const target = named ?? key;
  let mac: string;
  if (target === undefined) {
    const macs = links.macs();
    if (macs.length > 1) return unsent(CHOOSE_MAC);
    const only = macs[0];
    if (only === undefined) return unsent(UNREACHABLE);
    mac = only;
  } else {
    const split = splitKey(target);
    if (split === null) return unsent(UNREACHABLE);
    if (key !== undefined && named !== undefined && splitKey(key)?.mac !== split.mac) return unsent(UNREACHABLE);
    mac = split.mac;
  }
  const client = links.client(mac);
  if (client === null) return unsent(UNREACHABLE);
  if (named === undefined) return client.request(body);
  const raw = splitKey(named)?.raw ?? "";
  return client.request({ ...body, session: raw } as RequestBody);
}

/** A `Send` to the Mac of `key` (or of each body's session; with neither, the only Mac): see
 * `sendVia`. With no Mac connected (not paired yet) every command is unsent. Stable while the
 * links are. */
export function useSend(key?: string): Send {
  const links = useLinks();
  return useCallback<Send>((body) => sendVia(links, key, body), [links, key]);
}

/**
 * `sendVia` for Mac `mac`, chosen by the user (New session's chip, a Recent row): a body that
 * names no session (`start`, `recent`) goes to that Mac, and one that names a session goes to
 * the session's Mac, which must be `mac`. Refused as `unsent`: a session of another Mac, or a Mac
 * with no connection. With no `mac`, `sendVia` with no key.
 */
export function sendTo(links: Links, mac: string | undefined, body: RequestBody): Promise<Ack> {
  if (mac === undefined) return sendVia(links, undefined, body);
  if ("session" in body) {
    if (splitKey(body.session)?.mac !== mac) return Promise.reject(new Refusal("unsent", UNREACHABLE));
    return sendVia(links, body.session, body);
  }
  const client = links.client(mac);
  if (client === null) return Promise.reject(new Refusal("unsent", UNREACHABLE));
  return client.request(body);
}

/** A `Send` to Mac `mac` (see `sendTo`); with none, `useSend()`. Stable while the links and the
 * Mac are. */
export function useSendTo(mac: string | undefined): Send {
  const links = useLinks();
  return useCallback<Send>((body) => sendTo(links, mac, body), [links, mac]);
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
