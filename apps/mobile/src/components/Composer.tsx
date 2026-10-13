import { memo } from "react";
import { Pressable, StyleSheet, Text, TextInput, View } from "react-native";

import type { Phase } from "../protocol";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { SLASH_NOTE, useTextSender, type Send } from "./send";

interface ComposerProps {
  session: string;
  /** Running queues the prompt behind the turn; idle sends it. */
  phase: Phase;
  /** Prompts waiting on the Mac. */
  queued: number;
  /** Must be stable. */
  send: Send;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    box: { paddingHorizontal: spacing.lg, paddingTop: spacing.sm, gap: spacing.xs },
    hint: { ...type.caption, color: theme.muted },
    note: { ...type.caption, color: theme.muted },
    failure: { flexDirection: "row", alignItems: "center", gap: spacing.md },
    failureText: { ...type.caption, color: theme.fg, flex: 1 },
    row: { flexDirection: "row", alignItems: "flex-end", gap: spacing.sm },
    input: {
      ...type.body,
      flex: 1,
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
    send: {
      width: 44,
      height: 44,
      alignItems: "center",
      justifyContent: "center",
      borderRadius: radius.lg,
      backgroundColor: theme.accent,
    },
    off: { opacity: 0.4 },
    glyph: { ...type.title, color: theme.bg },
    retry: {
      minHeight: 44,
      minWidth: 44,
      alignItems: "center",
      justifyContent: "center",
      paddingHorizontal: spacing.sm,
    },
    retryText: { ...type.body, color: theme.accent },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/** "Send" while idle; "Queue" (and how many wait) while a turn runs. */
export const hintFor = (phase: Phase, queued: number): string =>
  phase === "running" ? (queued > 0 ? `Queue · ${queued} queued` : "Queue") : "Send";

/**
 * The prompt box. The text is cleared only when the Mac acks it, so a refusal or a lost
 * connection never costs the user their words. `/…` is not sent: slash commands are the
 * terminal UI's.
 */
export const Composer = memo(function Composer({ session, phase, queued, send }: ComposerProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const { text, setText, busy, failed, slash, canSend, submit } = useTextSender(send, session, "prompt");
  const verb = phase === "running" ? "Queue" : "Send";
  return (
    <View style={s.box}>
      <Text style={s.hint}>{hintFor(phase, queued)}</Text>
      {slash ? <Text style={s.note}>{SLASH_NOTE}</Text> : null}
      {failed ? (
        <View style={s.failure} accessibilityLiveRegion="polite">
          <Text style={s.failureText}>{failed.text}</Text>
          {failed.retry ? (
            <Pressable accessibilityRole="button" accessibilityLabel="Try again" onPress={submit} style={s.retry}>
              <Text style={s.retryText}>Try again</Text>
            </Pressable>
          ) : null}
        </View>
      ) : null}
      <View style={s.row}>
        <TextInput
          accessibilityLabel="Message"
          placeholder="Message Octet"
          placeholderTextColor={theme.muted}
          multiline
          value={text}
          onChangeText={setText}
          style={s.input}
        />
        <Pressable
          accessibilityRole="button"
          accessibilityLabel={verb}
          accessibilityState={{ disabled: !canSend, busy }}
          disabled={!canSend}
          onPress={submit}
          style={[s.send, !canSend && s.off]}
        >
          <Text style={s.glyph}>↑</Text>
        </Pressable>
      </View>
    </View>
  );
});
