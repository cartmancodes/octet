import { memo, useCallback } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { startedAgo, type RecentItem } from "../recent";
import { clipLabel, engineName, sessionName } from "../sessions";
import { radius, spacing, type, useTheme } from "../theme";

interface RecentRowProps {
  item: RecentItem;
  /** The Mac it is on, drawn (and said) only with several Macs; empty otherwise. */
  macName?: string;
  /** The clock the list was loaded at, in ms: it moves only with a load, so rows stay put. */
  now: number;
  /** A start is out or a launched session is awaited. */
  disabled: boolean;
  /** Must be stable, or every row draws again with the list. */
  onResume: (item: RecentItem) => void;
}

/** One resumable session: engine, title (else "Untitled"), workspace folder, how long ago, and
 * Resume. Every string is the Mac's and drawn as text. */
export const RecentRow = memo(function RecentRow({ item, macName = "", now, disabled, onResume }: RecentRowProps) {
  const theme = useTheme();
  const folder = sessionName(item.workspace);
  const title = item.title.trim() === "" ? "Untitled" : item.title;
  const ago = startedAgo(item.started, now);
  const resume = useCallback(() => onResume(item), [onResume, item]);
  return (
    <View style={[styles.row, { backgroundColor: theme.panel, borderColor: theme.edge }]}>
      <View style={styles.text}>
        <Text style={[type.caption, { color: theme.muted }]}>{engineName(item.engine)}</Text>
        {macName === "" ? null : <Text style={[type.caption, { color: theme.muted }]}>{macName}</Text>}
        <Text numberOfLines={2} style={[type.body, { color: theme.fg }]}>
          {title}
        </Text>
        <Text numberOfLines={1} style={[type.caption, { color: theme.muted }]}>
          {folder}
        </Text>
        {ago === "" ? null : <Text style={[type.caption, { color: theme.muted }]}>{ago}</Text>}
      </View>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel={macName === "" ? `Resume ${clipLabel(title)}, ${clipLabel(folder)}` : `Resume ${clipLabel(title)}, ${clipLabel(folder)}, on ${macName}`}
        accessibilityState={disabled ? OFF : ON}
        disabled={disabled}
        onPress={resume}
        style={[styles.button, { borderColor: theme.accent }, disabled && styles.off]}
      >
        <Text style={[type.body, { color: theme.accent }]}>Resume</Text>
      </Pressable>
    </View>
  );
});

const OFF = { disabled: true } as const;
const ON = { disabled: false } as const;

const styles = StyleSheet.create({
  row: {
    flexDirection: "row",
    alignItems: "center",
    gap: spacing.md,
    padding: spacing.lg,
    marginHorizontal: spacing.lg,
    marginBottom: spacing.sm,
    borderRadius: radius.md,
    borderWidth: StyleSheet.hairlineWidth,
  },
  text: { flex: 1, gap: 2 },
  button: {
    minHeight: 44,
    minWidth: 44,
    paddingHorizontal: spacing.lg,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
    borderWidth: 1,
  },
  off: { opacity: 0.4 },
});
