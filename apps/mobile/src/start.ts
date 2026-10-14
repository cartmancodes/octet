// Starting a session on the Mac and waiting for it: what New session and Resume share. One start
// at a time, the tmux name from the ack, the session found when it is listed, the 30 s late notice.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { RequestBody } from "./client/HubClient";
import { useAction, useAlive, useSend, type Action, type Send } from "./components/send";
import type { RecentInfo } from "./protocol";
import { recentItems, tmuxOfAck, workspaceChoices } from "./recent";
import { useHub, useSessions } from "./store";

export type StartBody = Extract<RequestBody, { type: "start" }>;

export const NO_WORKSPACE = "Start a session on the Mac first, or add a workspace to hub.json.";

/** A launched session that has not shown in Sessions by now is probably not coming. */
export const APPEAR_MS = 30_000;

/** How long Start stays locked after an ack that names no session. */
export const UNNAMED_MS = 10_000;

/** A start whose session is awaited: `tmux` is the name from the ack, null when the hub's words
 * did not carry one; `late` once 30 seconds (10 without a name) have passed. */
export interface Launch {
  tmux: string | null;
  late: boolean;
}

/** The status line for a launch, or null without one. */
export function launchNotice(launch: Launch | null): string | null {
  if (launch === null) return null;
  if (launch.tmux === null) return "Started — it will appear in Sessions";
  return launch.late
    ? `${launch.tmux} hasn't appeared yet. Check tmux on the Mac.`
    : `Starting ${launch.tmux} — it will appear in Sessions`;
}

export interface StartSession {
  send: Send;
  /** Sends `body` (a `start`) unless one is out, and forgets any earlier launch if it sent.
   * True if it sent. */
  start: (body: StartBody) => boolean;
  /** A start is out. */
  busy: boolean;
  /** The last start's failure. */
  failed: Action["failed"];
  fail: Action["fail"];
  /** The launched session is awaited (30 s, or 10 s when the ack named none, have not passed). */
  waiting: boolean;
  /** Nothing may start now: one is out, the session is awaited, or the Mac can't be reached. */
  locked: boolean;
  /** The line to show about the launch, or null. */
  notice: string | null;
}

/**
 * Starts sessions and watches for the launched one. When a listed session carries the tmux name
 * from the ack, `onListed` runs once with its id. Leaving the screen ends the watch. `onListed`
 * may change between renders.
 */
export function useStartSession(onListed: (sessionId: string) => void): StartSession {
  const send = useSend();
  const hub = useHub();
  const sessions = useSessions();
  const { busy, failed, run, fail } = useAction(send);
  const [launch, setLaunch] = useState<Launch | null>(null);
  const opened = useRef(false);
  const listed = useRef(onListed);
  useEffect(() => {
    listed.current = onListed;
  });

  const waiting = launch !== null && !launch.late;
  const locked = busy || hub !== "ready" || waiting;

  const started = useCallback((ack: unknown) => setLaunch({ tmux: tmuxOfAck(ack), late: false }), []);
  const start = useCallback(
    (body: StartBody): boolean => {
      // A start already out is left alone: its watch is the one to keep.
      if (!run(body, started)) return false;
      opened.current = false;
      setLaunch(null); // the first start's session is no longer the one to open
      return true;
    },
    [run, started],
  );

  const tmux = launch?.tmux ?? null;
  useEffect(() => {
    if (launch === null || launch.late) return;
    const timer = setTimeout(
      () => setLaunch((l) => (l === launch ? { ...l, late: true } : l)),
      launch.tmux === null ? UNNAMED_MS : APPEAR_MS,
    );
    return () => clearTimeout(timer);
  }, [launch]);
  useEffect(() => {
    if (tmux === null || opened.current) return;
    const found = sessions.find((s) => s.tmux === tmux);
    if (!found) return;
    opened.current = true;
    listed.current(found.id);
  }, [tmux, sessions]);

  return { send, start, busy, failed, fail, waiting, locked, notice: launchNotice(launch) };
}

export interface Workspaces {
  /** The workspaces a new session may start in. */
  choices: string[];
  /** The Mac has answered the request for its recent folders (or refused it), so none may yet
   * be on its way. */
  loaded: boolean;
}

/** The workspaces a new session may start in: the live sessions' and the recent items' (asked of
 * the Mac once the connection is ready; if it fails, the live sessions' alone). */
export function useWorkspaceChoices(): Workspaces {
  const send = useSend();
  const alive = useAlive();
  const hub = useHub();
  const sessions = useSessions();
  const [recent, setRecent] = useState<RecentInfo[]>([]);
  const [loaded, setLoaded] = useState(false);
  useEffect(() => {
    if (hub !== "ready") return;
    send({ type: "recent" }).then(
      (frame) => {
        if (!alive()) return;
        const items = recentItems(frame);
        if (items) setRecent(items);
        setLoaded(true);
      },
      () => {
        if (alive()) setLoaded(true);
      },
    );
  }, [hub, send, alive]);
  const choices = useMemo(() => workspaceChoices(sessions, recent), [sessions, recent]);
  return { choices, loaded };
}
