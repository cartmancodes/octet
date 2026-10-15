// The desk's own commands the phone may run (`command` requests): what an ack does for each, over
// one action at a time. Used by the prompt line's slash menu and the Usage screen.
import { useCallback, useState } from "react";

import type { CommandName } from "../protocol";
import { field } from "../model/text";
import { octetStore } from "../store";
import { showToast } from "../toast";
import { useAction, type Action, type Send } from "./send";

/** The most characters of an ack's text kept. */
export const ACK_CHARS = 8_000;
/** Said in the sheet when the desk's ack had no words. */
export const NO_DETAILS = "The desk sent no details";
/** Toasted for an export ack with no words. */
export const EXPORTED = "Exported the journal";

/** An ack's `message`, cleaned of escapes and controls and clipped; "" when it has none. */
export function ackText(ack: unknown): string {
  const message = typeof ack === "object" && ack !== null ? (ack as { message?: unknown }).message : undefined;
  return field(message, ACK_CHARS).trimEnd();
}

/** The divider /new leaves in the transcript. The session keeps its id and the hub replays its
 * journals from the first, so the earlier conversation stays on screen: this marks where the new
 * one begins, and the new journal's records land below it. */
export const FRESH_NOTE = "Fresh conversation — the earlier one is above.";

/** What /session and /help open: a title and the ack's text. */
export interface SheetInfo {
  title: string;
  text: string;
}

export interface DeskCommand {
  busy: boolean;
  failed: Action["failed"];
  fail: Action["fail"];
  /** The sheet an ack opened, or null. */
  sheet: SheetInfo | null;
  closeSheet: () => void;
  /** Sends the command unless one is out, and does what its ack calls for: /session and /help
   * open a sheet, /export toasts. `onAck` runs after, for the caller's own part (clearing the
   * words, leaving the screen). True if it sent. */
  run: (name: CommandName, onAck?: () => void) => boolean;
}

/** The desk's commands for `session`, one at a time. */
export function useDeskCommand(session: string, send: Send): DeskCommand {
  // The fresh-start divider is the Mac's doing as soon as it acks, whether or not this screen is
  // still there to see it.
  const marking = useCallback<Send>(
    (body) =>
      send(body).then((ack) => {
        if (body.type === "command" && body.name === "new") octetStore.getState().addNote(session, FRESH_NOTE);
        return ack;
      }),
    [send, session],
  );
  const action = useAction(marking);
  const { run: sendBody } = action;
  const [sheet, setSheet] = useState<SheetInfo | null>(null);
  const run = useCallback(
    (name: CommandName, onAck?: () => void): boolean =>
      sendBody({ type: "command", session, name }, (ack) => {
        const text = ackText(ack);
        if (name === "export") showToast(text === "" ? EXPORTED : text);
        else if (name === "session" || name === "help") setSheet({ title: name === "help" ? "Help" : "Session", text: text === "" ? NO_DETAILS : text });
        onAck?.();
      }),
    [sendBody, session],
  );
  const closeSheet = useCallback(() => setSheet(null), []);
  return { busy: action.busy, failed: action.failed, fail: action.fail, sheet, closeSheet, run };
}
