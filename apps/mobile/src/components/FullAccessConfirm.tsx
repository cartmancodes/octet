import { memo, useCallback, useState } from "react";
import { Pressable, StyleSheet, Text, TextInput, View } from "react-native";

import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { FULL_ACCESS_WARNING, FULL_ACCESS_WORDS } from "./fullAccess";

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    confirm: {
      marginHorizontal: spacing.lg,
      marginBottom: spacing.sm,
      padding: spacing.md,
      gap: spacing.sm,
      borderRadius: radius.md,
      borderColor: theme.amber,
      borderWidth: 1,
    },
    warning: { ...type.caption, color: theme.amber },
    text: { ...type.caption, color: theme.fg },
    buttons: { flexDirection: "row", gap: spacing.md },
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
    off: { opacity: 0.4 },
    label: { ...type.body, color: theme.fg },
    primaryLabel: { ...type.body, color: theme.bg },
    input: {
      ...type.body,
      minHeight: 44,
      paddingHorizontal: spacing.md,
      paddingVertical: spacing.sm,
      color: theme.fg,
      backgroundColor: theme.bg,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.md,
    },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

interface FullAccessConfirmProps {
  busy: boolean;
  /** Called only once the words are exact. */
  onTurnOn: () => void;
  onCancel: () => void;
}

/**
 * The words that turn full access on, after Face ID. Its text lives here, so closing the panel
 * (or the sheet) forgets what was typed.
 */
export const FullAccessConfirm = memo(function FullAccessConfirm({ busy, onTurnOn, onCancel }: FullAccessConfirmProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const [words, setWords] = useState("");
  const exact = words === FULL_ACCESS_WORDS;
  const turnOn = useCallback(() => {
    if (exact) onTurnOn();
  }, [exact, onTurnOn]);
  const off = busy || !exact;
  return (
    <View style={s.confirm}>
      <Text style={s.warning}>{FULL_ACCESS_WARNING}</Text>
      <Text style={s.text}>{`Type ${FULL_ACCESS_WORDS} to turn it on.`}</Text>
      <TextInput
        accessibilityLabel="Type full access to confirm"
        autoFocus
        autoCapitalize="none"
        autoCorrect={false}
        spellCheck={false}
        placeholder={FULL_ACCESS_WORDS}
        placeholderTextColor={theme.muted}
        value={words}
        onChangeText={setWords}
        style={s.input}
      />
      <View style={s.buttons}>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Cancel full access"
          onPress={onCancel}
          style={s.button}
        >
          <Text style={s.label}>Cancel</Text>
        </Pressable>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Turn on full access"
          accessibilityState={{ disabled: off, busy }}
          disabled={off}
          onPress={turnOn}
          style={[s.button, s.primary, off && s.off]}
        >
          <Text style={s.primaryLabel}>Turn on full access</Text>
        </Pressable>
      </View>
    </View>
  );
});
