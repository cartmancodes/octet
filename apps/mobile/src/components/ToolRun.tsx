import { memo, useCallback, useMemo } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { editedFile, splitPath } from "../changes";
import type { Entry } from "../model/transcript";
import { callRows, moreCalls, moreLines, summary, toolRows, type ToolRow } from "../model/summary";
import { DEFAULT_SETTINGS, type TextSize } from "../persist";
import { sizedStyles, transcriptText } from "../sizedStyles";
import { spacing, useTheme, type Theme } from "../theme";

export type ToolsEntry = Extract<Entry, { kind: "tools" }>;

interface ToolRunProps {
  entry: ToolsEntry;
  /** Expanded: every call with its whole detail. The screen keeps it, so a recycled row cannot. */
  open: boolean;
  /** Called with the entry's key. Must be stable, or every row re-renders with the list. */
  onToggle: (key: string) => void;
  /** The transcript's text size. */
  size?: TextSize;
  /** Given, an entry that edited a file ends in "view ›", which calls it with that file (the
   * first, of several). Must be stable. */
  onView?: (file: string) => void;
}

function makeStyles(theme: Theme, size: TextSize) {
  const mono = transcriptText(size);
  return StyleSheet.create({
    run: { minHeight: 44, justifyContent: "center", gap: spacing.xs, paddingVertical: spacing.xs },
    /** The run beside its "view ›". */
    line: { flexDirection: "row", alignItems: "flex-start" },
    grow: { flex: 1, minHeight: 44, justifyContent: "center", gap: spacing.xs, paddingVertical: spacing.xs },
    view: { minWidth: 44, minHeight: 44, paddingLeft: spacing.sm, paddingVertical: spacing.xs, alignItems: "flex-end" },
    link: { ...mono, color: theme.accent },
    head: { ...mono, color: theme.muted },
    failed: { ...mono, color: theme.amber },
    detail: { ...mono, color: theme.muted, paddingLeft: spacing.lg },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const stylesFor = sizedStyles(makeStyles);

const EXPANDED = { expanded: true } as const;
const COLLAPSED = { expanded: false } as const;

const Rows = memo(function Rows({ rows, s }: { rows: ToolRow[]; s: Styles }) {
  return rows.map((row, i) => (
    <View key={i}>
      <Text style={row.kind === "failed" ? s.failed : s.head}>{row.kind === "summary" ? `▸ ${row.head}` : row.head}</Text>
      {row.lines.length > 0 ? <Text style={s.detail}>{row.lines.join("\n")}</Text> : null}
      {row.more > 0 ? <Text style={s.detail}>{moreLines(row.more)}</Text> : null}
    </View>
  ));
});

/** What a tools entry's row says to a screen reader: the calls in words, and what is not done. */
function spoken(entry: ToolsEntry, open: boolean): string {
  const running = entry.calls.filter((c) => c.phase === "running").length;
  const failed = entry.calls.filter((c) => c.phase === "failed").length;
  return [
    `Tools: ${summary(entry.calls)}`,
    failed > 0 ? `${failed} failed` : "",
    running > 0 ? (entry.over ? `${running} stopped` : `${running} running`) : "",
    (entry.more ?? 0) > 0 ? moreCalls(entry.more ?? 0) : "",
    open ? "hide details" : "show details",
  ]
    .filter((x) => x !== "")
    .join(", ");
}

/**
 * A run of tool calls, as the terminal UI groups them. Collapsed: "▸ Read 3 files · Ran 1
 * command" for the calls done, "… Bash cargo test" per call running, and each failure open in
 * amber with its last lines. A tap lists every call with its whole detail.
 */
export const ToolRun = memo(function ToolRun({ entry, open, onToggle, size = DEFAULT_SETTINGS.textSize, onView }: ToolRunProps) {
  const theme = useTheme();
  const s = stylesFor(theme, size);
  const rows = useMemo(() => {
    if (!open) return toolRows(entry.calls, entry.over);
    const head: ToolRow = { kind: "summary", head: summary(entry.calls), lines: [], more: 0 };
    return [{ ...head, head: `▾ ${head.head}`, kind: "done" as const }, ...callRows(entry.calls, entry.over)];
  }, [entry, open]);
  const toggle = useCallback(() => onToggle(entry.key), [onToggle, entry.key]);
  const edited = useMemo(() => (onView ? editedFile(entry) : null), [entry, onView]);
  const view = useCallback(() => {
    if (edited !== null) onView?.(edited);
  }, [edited, onView]);
  if (entry.calls.length === 0) return null;
  const run = (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={spoken(entry, open)}
      accessibilityState={open ? EXPANDED : COLLAPSED}
      onPress={toggle}
      style={edited === null ? s.run : s.grow}
    >
      <Rows rows={rows} s={s} />
      {(entry.more ?? 0) > 0 ? <Text style={s.head}>{moreCalls(entry.more ?? 0)}</Text> : null}
    </Pressable>
  );
  if (edited === null) return run;
  // Beside the run, not in it: a screen reader reaches each control on its own.
  return (
    <View style={s.line}>
      {run}
      <Pressable accessibilityRole="button" accessibilityLabel={`View changes to ${splitPath(edited).name}`} onPress={view} style={s.view}>
        <Text style={s.link}>view ›</Text>
      </Pressable>
    </View>
  );
});
