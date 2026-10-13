import { memo } from "react";
import { StyleSheet, Text, View, type TextStyle } from "react-native";

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
    paragraphs: { gap: type.body.lineHeight },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

const Paragraph = memo(function Paragraph({ text, style }: { text: string; style: TextStyle }) {
  return <Text style={style}>{text}</Text>;
});

/** A reply still streaming, as plain text in paragraphs: a chunk changes only the last one, so
 * only its text is laid out again, however long the reply has grown. */
const Streaming = memo(function Streaming({ text, style, gap }: { text: string; style: TextStyle; gap: object }) {
  const parts = text.split("\n\n");
  if (parts.length === 1) return <Paragraph text={text} style={style} />;
  return (
    <View style={gap}>
      {parts.map((part, i) => (
        <Paragraph key={i} text={part} style={style} />
      ))}
    </View>
  );
});

/**
 * One transcript entry, labelled as the terminal UI labels it (YOU, OCTET, NOTE, ERROR). A reply
 * still streaming is plain text, cheap to redraw on every chunk; once done it is Markdown.
 * Memoised on the entry, which the store replaces only when it changes: while a reply streams,
 * only its row draws again.
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
      {entry.kind === "reply" && entry.done ? (
        <Markdown text={entry.text} />
      ) : entry.kind === "reply" ? (
        <Streaming text={entry.text} style={s.text} gap={s.paragraphs} />
      ) : (
        <Text style={entry.kind === "error" ? s.error : entry.kind === "note" ? s.note : s.text}>{entry.text}</Text>
      )}
    </View>
  );
});
