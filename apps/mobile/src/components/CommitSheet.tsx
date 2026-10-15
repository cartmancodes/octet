// The Changes screen's two sheets: the commit message, sent behind Face ID, and "Open PR on the
// desk", which only says what to run at the Mac. The phone commits everything in the session's
// workspace; it never pushes, opens a pull request or commits part of the changes.
import { memo, useCallback, useLayoutEffect, useRef, useState } from "react";
import { KeyboardAvoidingView, Modal, Platform, Pressable, StyleSheet, Text, TextInput, View } from "react-native";

import { confirmBiometric } from "../auth";
import { COMMIT_CHARS, COMMIT_REASON, PR_COMMAND, PR_INTRO, firstPrompt } from "../changes";
import { useEntries } from "../store";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { Button } from "../ui";
import { Refusal } from "../client/HubClient";
import { failure, useAlive, useSend } from "./send";

/** Said in the sheet when the phone can't ask for Face ID: nothing is committed without it. */
export const NO_FACE_ID_COMMIT = "Face ID isn't set up on this phone, so it can't commit from here.";

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    root: { flex: 1, justifyContent: "flex-end" },
    scrim: { ...StyleSheet.absoluteFill, backgroundColor: dark.bg, opacity: 0.5 },
    sheet: {
      backgroundColor: theme.panel,
      borderTopLeftRadius: radius.lg,
      borderTopRightRadius: radius.lg,
      borderColor: theme.edge,
      borderWidth: 1,
      paddingHorizontal: 20,
      paddingTop: spacing.md,
      paddingBottom: spacing.xxl,
      gap: spacing.md,
    },
    heading: { ...type.body, fontWeight: "700", color: theme.fg },
    text: { ...type.body, color: theme.fg },
    input: {
      ...type.body,
      minHeight: 120,
      maxHeight: 240,
      paddingHorizontal: spacing.md,
      paddingVertical: spacing.sm,
      color: theme.fg,
      backgroundColor: theme.bg,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.md,
      textAlignVertical: "top",
    },
    command: {
      paddingHorizontal: spacing.md,
      paddingVertical: spacing.md,
      backgroundColor: theme.bg,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.md,
    },
    buttons: { flexDirection: "row", flexWrap: "wrap", justifyContent: "flex-end", gap: spacing.sm },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);
const KEYBOARD = Platform.OS === "ios" ? "padding" : undefined;

interface CommitSheetProps {
  session: string;
  /** The Mac can't be reached, or an approval waits: Commit is off, and a commit whose Face ID
   * check ends meanwhile is not sent. */
  blocked: boolean;
  /** The Mac committed; runs with its ack even when the sheet has gone. Must be stable. */
  onCommitted: (ack: unknown) => void;
  /** The commit went and its answer was lost: it may have committed. The sheet stays, with the
   * words that say so; the screen asks for the changes again, which show whether it did. Runs
   * even when the sheet has gone. Must be stable. */
  onLost: () => void;
  /** Must be stable. */
  onClose: () => void;
}

/**
 * The commit message, prefilled with the session's first prompt as one line, and Commit: Face ID
 * every time, whatever the Face ID to allow setting (a commit changes the repository), then one
 * `commit`. One at a time: a second tap while Face ID is up or the commit is out does nothing, and
 * the sheet stays until the commit's answer is in, so it is seen and no second sheet can send
 * another. A refusal is shown in the hub's words, and the message stays.
 */
export const CommitSheet = memo(function CommitSheet({ session, blocked, onCommitted, onLost, onClose }: CommitSheetProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const entries = useEntries(session);
  const [message, setMessage] = useState(() => firstPrompt(entries));
  const send = useSend();
  const alive = useAlive();
  // "checking": the Face ID prompt is up. "sending": the commit is out, waiting for its ack.
  const [stage, setStage] = useState<"checking" | "sending" | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const out = useRef(false);
  const sending = useRef(false);
  // Read after Face ID, in the commit itself so no frame passes unseen: whether it may still go.
  const blockedNow = useRef(blocked);
  useLayoutEffect(() => {
    blockedNow.current = blocked;
  });

  const commit = useCallback(async () => {
    const text = message;
    if (out.current || blockedNow.current || text.trim() === "") return;
    out.current = true;
    setFailed(null);
    setStage("checking");
    try {
      const checked = await confirmBiometric(COMMIT_REASON);
      if (checked === "unavailable") {
        if (alive()) setFailed(NO_FACE_ID_COMMIT);
        return;
      }
      // Cancelled or no match, the sheet gone, or the Mac out of reach or an approval come
      // while the prompt was up: nothing is sent.
      if (checked !== "ok" || !alive() || blockedNow.current) return;
      setStage("sending");
      sending.current = true;
      const ack = await send({ type: "commit", session, message: text });
      onCommitted(ack);
    } catch (error) {
      if (error instanceof Refusal && error.code === "lost") onLost();
      if (alive()) setFailed(failure(error).text);
    } finally {
      out.current = false;
      sending.current = false;
      if (alive()) setStage(null);
    }
  }, [message, send, session, alive, onCommitted, onLost]);
  const press = useCallback(() => void commit(), [commit]);
  const close = useCallback(() => {
    if (!sending.current) onClose();
  }, [onClose]);

  const busy = stage !== null;
  const off = blocked || busy || message.trim() === "";
  return (
    <Modal testID="commit-sheet" visible transparent animationType="slide" onRequestClose={close}>
      <KeyboardAvoidingView behavior={KEYBOARD} style={s.root}>
        <View style={s.scrim} />
        <Pressable testID="commit-sheet-scrim" accessible={false} onPress={close} style={StyleSheet.absoluteFill} />
        <View testID="commit-sheet-body" accessibilityViewIsModal style={s.sheet}>
          <Text accessibilityRole="header" style={s.heading}>
            Commit
          </Text>
          <TextInput
            accessibilityLabel="Commit message"
            multiline
            maxLength={COMMIT_CHARS}
            placeholder="Commit message"
            placeholderTextColor={theme.muted}
            value={message}
            onChangeText={setMessage}
            style={s.input}
          />
          {failed !== null ? (
            <Text accessibilityLiveRegion="polite" style={s.text}>
              {failed}
            </Text>
          ) : null}
          <View style={s.buttons}>
            <Button label="Cancel" disabled={stage === "sending"} onPress={close} />
            <Button label="Commit" accessibilityLabel="Commit the changes" variant="primary" disabled={off} busy={busy} onPress={press} />
          </View>
        </View>
      </KeyboardAvoidingView>
    </Modal>
  );
});

/** "Open PR on the desk": the command to run at the Mac, to select and copy, and Close. It sends
 * nothing. */
export const PrSheet = memo(function PrSheet({ onClose }: { onClose: () => void }) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  return (
    <Modal testID="pr-sheet" visible transparent animationType="slide" onRequestClose={onClose}>
      <View style={s.root}>
        <View style={s.scrim} />
        <Pressable testID="pr-sheet-scrim" accessible={false} onPress={onClose} style={StyleSheet.absoluteFill} />
        <View testID="pr-sheet-body" accessibilityViewIsModal style={s.sheet}>
          <Text style={s.text}>{PR_INTRO}</Text>
          <View style={s.command}>
            {/* The terminal look's one face, monospace (type.body). */}
            <Text selectable style={s.text}>
              {PR_COMMAND}
            </Text>
          </View>
          <View style={s.buttons}>
            <Button label="Close" onPress={onClose} />
          </View>
        </View>
      </View>
    </Modal>
  );
});
