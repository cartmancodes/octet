// The two sheets the desk's commands use: the text an ack brings back (/session, /help), and the
// ask before /new. Hub text is drawn as plain `<Text>` only: no links, no markup.
import { memo } from "react";
import { Modal, Pressable, ScrollView, StyleSheet, Text, View } from "react-native";

import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { Button } from "../ui";

/** /new's question, which the sheet puts to the user before anything is sent. */
export const FRESH_QUESTION = "Start a fresh conversation? The current one stays in the journal.";

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
    head: { flexDirection: "row", alignItems: "center", justifyContent: "space-between" },
    heading: { ...type.body, fontWeight: "700", color: theme.fg },
    body: { maxHeight: 360 },
    text: { ...type.body, color: theme.fg },
    question: { ...type.body, color: theme.fg },
    buttons: { flexDirection: "row", flexWrap: "wrap", gap: spacing.sm },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

interface CommandSheetProps {
  /** "Session", "Help". */
  title: string;
  /** The ack's words, already cleaned. */
  text: string;
  /** Must be stable. */
  onClose: () => void;
}

/** An ack's text in a sheet: monospace, selectable, with Close. */
export const CommandSheet = memo(function CommandSheet({ title, text, onClose }: CommandSheetProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  return (
    <Modal testID="command-sheet" visible transparent animationType="slide" onRequestClose={onClose}>
      <View style={s.root}>
        <View style={s.scrim} />
        <Pressable testID="command-sheet-scrim" accessible={false} onPress={onClose} style={StyleSheet.absoluteFill} />
        <View style={s.sheet}>
          <View style={s.head}>
            <Text accessibilityRole="header" style={s.heading}>
              {title}
            </Text>
            <Button label="Close" onPress={onClose} />
          </View>
          <ScrollView style={s.body}>
            <Text selectable style={s.text}>
              {text}
            </Text>
          </ScrollView>
        </View>
      </View>
    </Modal>
  );
});

interface FreshConfirmProps {
  /** The Mac can't be reached: Start fresh is off. */
  offline: boolean;
  /** Must be stable. */
  onStart: () => void;
  /** Must be stable. */
  onCancel: () => void;
}

/** The ask before /new: nothing is sent until Start fresh. */
export const FreshConfirm = memo(function FreshConfirm({ offline, onStart, onCancel }: FreshConfirmProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  return (
    <Modal testID="fresh-confirm" visible transparent animationType="slide" onRequestClose={onCancel}>
      <View style={s.root}>
        <View style={s.scrim} />
        <Pressable testID="fresh-confirm-scrim" accessible={false} onPress={onCancel} style={StyleSheet.absoluteFill} />
        <View style={s.sheet}>
          <Text style={s.question}>{FRESH_QUESTION}</Text>
          <View style={s.buttons}>
            <Button label="Cancel" onPress={onCancel} />
            <Button label="Start fresh" variant="accent" onPress={onStart} disabled={offline} />
          </View>
        </View>
      </View>
    </Modal>
  );
});
