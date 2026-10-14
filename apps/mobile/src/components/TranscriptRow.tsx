import { memo } from "react";
import { StyleSheet, Text, View } from "react-native";

import { Markdown } from "../markdown/Markdown";
import type { Entry } from "../model/transcript";
import { DEFAULT_SETTINGS, TEXT_SIZES, type TextSize } from "../persist";
import { rowRole } from "../session";
import { MONO, dark, light, spacing, useTheme, type Theme } from "../theme";
import { ToolRun } from "./ToolRun";

interface TranscriptRowProps {
  entry: Entry;
  /** For a tools entry: expanded. */
  open: boolean;
  /** Called with a tools entry's key. Must be stable. */
  onToggle: (key: string) => void;
  /** The transcript's text size (`Aa`); 13 when not given. */
  size?: TextSize;
}

function makeStyles(theme: Theme, size: TextSize) {
  const text = { fontFamily: MONO, fontSize: size, lineHeight: size + 5 };
  const label = { ...text, fontWeight: "700" as const };
  return StyleSheet.create({
    row: { paddingHorizontal: spacing.lg, paddingVertical: spacing.sm },
    accent: { ...label, color: theme.accent },
    fg: { ...label, color: theme.fg },
    muted: { ...label, color: theme.muted },
    amber: { ...label, color: theme.amber },
    text: { ...text, color: theme.fg },
    note: { ...text, color: theme.muted },
    error: { ...text, color: theme.amber },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const STYLES: Record<"dark" | "light", Record<TextSize, Styles>> = {
  dark: Object.fromEntries(TEXT_SIZES.map((n) => [n, makeStyles(dark, n)])) as Record<TextSize, Styles>,
  light: Object.fromEntries(TEXT_SIZES.map((n) => [n, makeStyles(light, n)])) as Record<TextSize, Styles>,
};

/**
 * One transcript entry under its bold label, as the terminal UI labels it (YOU, OCTET, TOOL, NOTE,
 * ERROR). A reply is Markdown, streaming or done; a streaming one parses only what came since its
 * last whole block, so a chunk costs the same however long the reply has grown. Memoised on the
 * entry, which the store replaces only when it changes: while a reply streams, only its row draws
 * again. A new text size draws every row again.
 */
export const TranscriptRow = memo(function TranscriptRow({ entry, open, onToggle, size = DEFAULT_SETTINGS.textSize }: TranscriptRowProps) {
  const theme = useTheme();
  const s = STYLES[theme === light ? "light" : "dark"][size];
  const role = rowRole(entry);
  return (
    <View style={s.row}>
      <Text style={s[role.tone]}>{role.label}</Text>
      {entry.kind === "tools" ? (
        <ToolRun entry={entry} open={open} onToggle={onToggle} size={size} />
      ) : entry.kind === "reply" ? (
        <Markdown text={entry.text} streaming={!entry.done} size={size} />
      ) : (
        <Text style={entry.kind === "error" ? s.error : entry.kind === "note" ? s.note : s.text}>{entry.text}</Text>
      )}
    </View>
  );
});
