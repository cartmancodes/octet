// Starting a session on the Mac and waiting for it: what New session and Resume share. One start
// at a time, the tmux name from the ack, the session found when it is listed, the 30 s late notice.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { RequestBody } from "./client/HubClient";
import { sendTo, sendVia, useAction, useAlive, useSendTo, type Action, type Send } from "./components/send";
import { useLinks } from "./hub";
import type { RecentInfo } from "./protocol";
import { recentItems, tmuxOfAck, workspaceChoices } from "./recent";
import { useHub, useSessions } from "./store";

export type StartBody = Extract<RequestBody, { type: "start" }>;

export const NO_WORKSPACE = "Start a session on the Mac first, or add a workspace to hub.json.";

/** A launched session that has not shown in the Inbox by now is probably not coming. */
export const APPEAR_MS = 30_000;

/** How long Start stays locked after an ack that names no session. */
export const UNNAMED_MS = 10_000;

/** A start whose session is awaited: `tmux` is the name from the ack, null when the hub's words
 * did not carry one; `late` once 30 seconds (10 without a name) have passed. */
export interface Launch {
  tmux: string | null;
  late: boolean;
  /** The Mac it was started on; undefined when the only one. */
  mac?: string;
}

/** The status line for a launch, or null without one. */
export function launchNotice(launch: Launch | null): string | null {
  if (launch === null) return null;
  if (launch.tmux === null) return "Started — it will appear in the Inbox";
  return launch.late
    ? `${launch.tmux} hasn't appeared yet. Check tmux on the Mac.`
    : `Starting ${launch.tmux} — it will appear in the Inbox`;
}

export interface StartSession {
  /** Sends a body naming a session to that session's Mac, by its key (the launched session's
   * first prompt, whatever Mac the screen shows by then); any other body to the hook's Mac. */
  send: Send;
  /** Sends `body` (a `start`) to Mac `mac` (else the hook's) unless one is out, and forgets any
   * earlier launch if it sent. True if it sent. */
  start: (body: StartBody, mac?: string) => boolean;
  /** A start is out. */
  busy: boolean;
  /** The last start's failure. */
  failed: Action["failed"];
  fail: Action["fail"];
  /** The launched session is awaited (30 s, or 10 s when the ack named none, have not passed). */
  waiting: boolean;
  /** Nothing may start now: one is out, the session is awaited, or the hook's Mac can't be reached
   * (with no Mac given, the screen checks the Mac of each start). */
  locked: boolean;
  /** The line to show about the launch, or null. */
  notice: string | null;
}

/**
 * Starts sessions on Mac `mac` and watches for the launched one. When a session of that Mac is
 * listed carrying the tmux name from the ack, `onListed` runs once with its id. Leaving the
 * screen ends the watch. `onListed` may change between renders. With no `mac`, each `start`
 * names its own (else it goes to the only Mac).
 */
export function useStartSession(onListed: (sessionId: string) => void, mac?: string): StartSession {
  const links = useLinks();
  const send = useCallback<Send>(
    (body) => ("session" in body ? sendVia(links, body.session, body) : sendTo(links, mac, body)),
    [links, mac],
  );
  const hub = useHub(mac ?? "");
  const sessions = useSessions();
  // The Mac of the start being sent: read by the send below as `run` calls it.
  const target = useRef(mac);
  const startSend = useCallback<Send>((body) => sendTo(links, target.current, body), [links]);
  const { busy, failed, run, fail } = useAction(startSend);
  const [launch, setLaunch] = useState<Launch | null>(null);
  const opened = useRef(false);
  const listed = useRef(onListed);
  useEffect(() => {
    listed.current = onListed;
  });

  const waiting = launch !== null && !launch.late;
  const locked = busy || (mac !== undefined && hub !== "ready") || waiting;

  const launchedOn = useRef<string | undefined>(mac);
  const started = useCallback((ack: unknown) => setLaunch({ tmux: tmuxOfAck(ack), late: false, mac: launchedOn.current }), []);
  const start = useCallback(
    (body: StartBody, on?: string): boolean => {
      const to = on ?? mac;
      // `run` sends at once, to `target`. A start already out is left alone: its watch is the one
      // to keep.
      target.current = to;
      if (!run(body, started)) return false;
      launchedOn.current = to;
      opened.current = false;
      setLaunch(null); // the first start's session is no longer the one to open
      return true;
    },
    [run, started, mac],
  );

  const tmux = launch?.tmux ?? null;
  const launchMac = launch?.mac;
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
    const found = sessions.find((s) => s.tmux === tmux && (launchMac === undefined || s.mac === launchMac));
    if (!found) return;
    opened.current = true;
    listed.current(found.id);
  }, [tmux, launchMac, sessions]);

  return { send, start, busy, failed, fail, waiting, locked, notice: launchNotice(launch) };
}

export interface Workspaces {
  /** The workspaces a new session may start in. */
  choices: string[];
  /** The Mac has answered the request for its recent folders (or refused it), so none may yet
   * be on its way. */
  loaded: boolean;
}

/** The workspaces a new session may start in on Mac `mac`: its live sessions' and its recent
 * items' (asked of it once its connection is ready; if that fails, the live sessions' alone). */
export function useWorkspaceChoices(mac: string | undefined): Workspaces {
  const send = useSendTo(mac);
  const alive = useAlive();
  const hub = useHub(mac ?? "");
  const sessions = useSessions();
  // What each Mac answered, kept by Mac: a late answer of a Mac chosen before is that Mac's alone.
  const [answers, setAnswers] = useState<ReadonlyMap<string | undefined, { items: RecentInfo[] }>>(NO_ANSWERS);
  useEffect(() => {
    if (hub !== "ready") return;
    const keep = (items: RecentInfo[] | null) =>
      setAnswers((all) => new Map(all).set(mac, { items: items ?? all.get(mac)?.items ?? NO_RECENT }));
    send({ type: "recent" }).then(
      (frame) => {
        if (alive()) keep(recentItems(frame));
      },
      () => {
        if (alive()) keep(null);
      },
    );
  }, [hub, send, alive, mac]);
  const answer = answers.get(mac);
  const items = answer?.items ?? NO_RECENT;
  const own = useMemo(() => (mac === undefined ? sessions : sessions.filter((s) => s.mac === mac)), [sessions, mac]);
  const choices = useMemo(() => workspaceChoices(own, items), [own, items]);
  return { choices, loaded: answer !== undefined };
}

const NO_ANSWERS: ReadonlyMap<string | undefined, { items: RecentInfo[] }> = new Map();
const NO_RECENT: RecentInfo[] = [];
