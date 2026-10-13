import { memo } from "react";
import { StyleSheet, Text, View } from "react-native";

import { Markdown } from "../markdown/Markdown";
import type { Entry } from "../model/transcript";
import { rowRole } from "../session";
import { dark, light, spacing, type, useTheme, type Theme } from "../theme";
import { ToolRun } from "./ToolRun";

interface TranscriptRowProps {
  entry: Entry;
  /** For a tools entry: expanded. */
  open: boolean;
  /** Called with a tools entry's key. Must be stable. */
  onToggle: (key: string) => void;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    row: { paddingHorizontal: spacing.lg, paddingVertical: spacing.sm, gap: spacing.xs },
    label: { ...type.caption, fontWeight: "700", letterSpacing: 0.5 },
    accent: { color: theme.accent },
    fg: { color: theme.fg },
    muted: { color: theme.muted },
    amber: { color: theme.amber },
    text: { ...type.body, color: theme.fg },
    note: { ...type.body, color: theme.muted },
    error: { ...type.body, color: theme.amber },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/**
 * One transcript entry, labelled as the terminal UI labels it (YOU, OCTET, NOTE, ERROR). A reply
 * is Markdown, streaming or done; a streaming one parses only what came since its last whole
 * block, so a chunk costs the same however long the reply has grown. Memoised on the entry,
 * which the store replaces only when it changes: while a reply streams, only its row draws again.
 */
export const TranscriptRow = memo(function TranscriptRow({ entry, open, onToggle }: TranscriptRowProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const role = rowRole(entry);
  if (entry.kind === "tools") {
    return (
      <View style={s.row}>
        <ToolRun entry={entry} open={open} onToggle={onToggle} />
      </View>
    );
  }
  return (
    <View style={s.row}>
      <Text style={[s.label, s[role.tone]]}>{role.label}</Text>
      {entry.kind === "reply" ? (
        <Markdown text={entry.text} streaming={!entry.done} />
      ) : (
        <Text style={entry.kind === "error" ? s.error : entry.kind === "note" ? s.note : s.text}>{entry.text}</Text>
      )}
    </View>
  );
});
