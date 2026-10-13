import { memo, useCallback, useState } from "react";
import { Pressable, StyleSheet, Text, TextInput, View } from "react-native";

import type { Engine, Phase } from "../protocol";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { SLASH_NOTE, useAction, useTextSender, type Send } from "./send";

interface SessionControlsProps {
  session: string;
  phase: Phase;
  engine: Engine;
  /** Must be stable. */
  send: Send;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    box: { paddingHorizontal: spacing.lg, paddingTop: spacing.sm, gap: spacing.xs },
    row: { flexDirection: "row", flexWrap: "wrap", alignItems: "center", gap: spacing.sm },
    button: {
      minHeight: 44,
      minWidth: 44,
      paddingHorizontal: spacing.lg,
      alignItems: "center",
      justifyContent: "center",
      borderRadius: radius.md,
      borderColor: theme.edge,
      borderWidth: 1,
    },
    primary: { backgroundColor: theme.accent, borderColor: theme.accent },
    label: { ...type.body, color: theme.fg },
    primaryLabel: { ...type.body, color: theme.bg },
    off: { opacity: 0.4 },
    question: { ...type.body, color: theme.fg },
    failure: { flexDirection: "row", alignItems: "center", gap: spacing.md },
    failureText: { ...type.caption, color: theme.fg, flex: 1 },
    input: {
      ...type.body,
      minHeight: 44,
      maxHeight: 132,
      paddingHorizontal: spacing.md,
      paddingVertical: spacing.sm,
      color: theme.fg,
      backgroundColor: theme.panel,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.md,
    },
    note: { ...type.caption, color: theme.muted },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/**
 * What the user can do to the session itself: Cancel the running turn, Steer it (Codex only),
 * or End the session when it is idle (after a question). Each shows only when the Mac would
 * accept it; a refusal is shown in the hub's words.
 */
export const SessionControls = memo(function SessionControls({ session, phase, engine, send }: SessionControlsProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const control = useAction(send);
  const { run } = control;
  const [steering, setSteering] = useState(false);
  const [ending, setEnding] = useState(false);
  // The Mac took End: wait for the session to leave rather than offer it again.
  const [ended, setEnded] = useState(false);
  const closeSteer = useCallback(() => setSteering(false), []);
  const steer = useTextSender(send, session, "steer", closeSteer);

  // A new phase is a new situation: a half-made End or an open Steer field means nothing now.
  const [seen, setSeen] = useState(phase);
  if (seen !== phase) {
    setSeen(phase);
    setEnding(false);
    setEnded(false);
    setSteering(false);
    steer.setText("");
  }

  const running = phase === "running";
  const canSteer = running && engine === "codex";
  const canEnd = phase === "idle";

  const cancel = useCallback(() => run({ type: "cancel", session }), [run, session]);
  const ask = useCallback(() => setEnding(true), []);
  const keep = useCallback(() => setEnding(false), []);
  const end = useCallback(() => {
    setEnding(false);
    run({ type: "end", session }, () => setEnded(true));
  }, [run, session]);
  const toggleSteer = useCallback(() => setSteering((open) => !open), []);

  if (!running && !canEnd) return null;
  const steerOpen = canSteer && steering;
  return (
    <View style={s.box}>
      {canEnd && ending ? (
        <View style={s.row}>
          <Text style={s.question}>End this session?</Text>
          <Pressable
            accessibilityRole="button"
            accessibilityLabel="Yes, end it"
            onPress={end}
            style={[s.button, s.primary]}
          >
            <Text style={s.primaryLabel}>Yes, end it</Text>
          </Pressable>
          <Pressable accessibilityRole="button" accessibilityLabel="Keep it" onPress={keep} style={s.button}>
            <Text style={s.label}>Keep it</Text>
          </Pressable>
        </View>
      ) : (
        <View style={s.row}>
          {running ? (
            <Pressable
              accessibilityRole="button"
              accessibilityLabel="Cancel"
              accessibilityState={{ disabled: control.busy, busy: control.busy }}
              disabled={control.busy}
              onPress={cancel}
              style={[s.button, control.busy && s.off]}
            >
              <Text style={s.label}>Cancel</Text>
            </Pressable>
          ) : null}
          {canSteer ? (
            <Pressable
              accessibilityRole="button"
              accessibilityLabel="Steer"
              accessibilityState={{ expanded: steerOpen }}
              onPress={toggleSteer}
              style={s.button}
            >
              <Text style={s.label}>Steer</Text>
            </Pressable>
          ) : null}
          {canEnd ? (
            <Pressable
              accessibilityRole="button"
              accessibilityLabel={ended ? "Ending…" : "End session"}
              accessibilityState={{ disabled: control.busy || ended, busy: control.busy || ended }}
              disabled={control.busy || ended}
              onPress={ask}
              style={[s.button, (control.busy || ended) && s.off]}
            >
              <Text style={s.label}>{ended ? "Ending…" : "End session"}</Text>
            </Pressable>
          ) : null}
        </View>
      )}
      {control.failed ? (
        <View accessibilityLiveRegion="polite">
          <Text style={s.failureText}>{control.failed.text}</Text>
        </View>
      ) : null}
      {steerOpen ? (
        <View style={s.box}>
          <Text style={s.note}>Steer the running turn</Text>
          <TextInput
            accessibilityLabel="Steer message"
            placeholder="Tell Codex what to do differently"
            placeholderTextColor={theme.muted}
            multiline
            value={steer.text}
            onChangeText={steer.setText}
            style={s.input}
          />
          {steer.slash ? <Text style={s.note}>{SLASH_NOTE}</Text> : null}
          {steer.failed ? (
            <View style={s.failure} accessibilityLiveRegion="polite">
              <Text style={s.failureText}>{steer.failed.text}</Text>
              {steer.failed.retry ? (
                <Pressable
                  accessibilityRole="button"
                  accessibilityLabel="Try again"
                  onPress={steer.submit}
                  style={s.button}
                >
                  <Text style={s.label}>Try again</Text>
                </Pressable>
              ) : null}
            </View>
          ) : null}
          <View style={s.row}>
            <Pressable
              accessibilityRole="button"
              accessibilityLabel="Send steer"
              accessibilityState={{ disabled: !steer.canSend, busy: steer.busy }}
              disabled={!steer.canSend}
              onPress={steer.submit}
              style={[s.button, s.primary, !steer.canSend && s.off]}
            >
              <Text style={s.primaryLabel}>Send steer</Text>
            </Pressable>
          </View>
        </View>
      ) : null}
    </View>
  );
});
