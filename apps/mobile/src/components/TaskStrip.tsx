import { memo, useCallback, useMemo, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import type { SessionState } from "../model/session";
import { goalTurns, queueWords, queuedCount } from "../session";
import { dark, light, spacing, type, useTheme, type Theme } from "../theme";

interface TaskStripProps {
  /** The terminal UI's state: goal, NOW and queue. */
  state: SessionState | null;
  /** The NOW line, else the phase in words. */
  now: string;
  /** The list's goal line, for a session whose state has not come yet. */
  goal: string | null;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    strip: {
      backgroundColor: theme.panel,
      borderBottomColor: theme.edge,
      borderBottomWidth: StyleSheet.hairlineWidth,
      paddingHorizontal: spacing.lg,
    },
    toggle: { minHeight: 44, flexDirection: "row", alignItems: "center", gap: spacing.sm },
    title: { ...type.caption, fontWeight: "700", color: theme.fg },
    summary: { ...type.mono, color: theme.fg, flex: 1 },
    count: { ...type.caption, color: theme.muted },
    body: { paddingBottom: spacing.md, gap: 2 },
    section: { ...type.caption, color: theme.muted, marginTop: spacing.xs },
    text: { ...type.body, color: theme.fg },
    mono: { ...type.mono, color: theme.fg },
    muted: { ...type.caption, color: theme.muted },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

const EXPANDED = { expanded: true } as const;
const COLLAPSED = { expanded: false } as const;

/** The terminal UI's Task card: the goal and its turns, what is happening now, and the queue.
 * A tap on its head folds it to one line (NOW and the queue's count). Hub strings are text. */
export const TaskStrip = memo(function TaskStrip({ state, now, goal: listedGoal }: TaskStripProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const [open, setOpen] = useState(true);
  const toggle = useCallback(() => setOpen((o) => !o), []);
  const queue = useMemo(() => queueWords(state), [state]);
  const goal = state?.goal ?? null;
  const count = queuedCount(queue.count);
  const label = ["Task", open ? "hide details" : "show details", count].filter((x) => x !== "").join(", ");
  return (
    <View testID="task-strip" style={s.strip}>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel={label}
        accessibilityState={open ? EXPANDED : COLLAPSED}
        onPress={toggle}
        style={s.toggle}
      >
        <Text style={s.title}>{open ? "▾ TASK" : "▸ TASK"}</Text>
        {open ? (
          <Text style={[s.count, styles.end]}>{count}</Text>
        ) : (
          <Text numberOfLines={1} style={s.summary}>
            {count === "" ? now : `${now} · ${count}`}
          </Text>
        )}
      </Pressable>
      {open ? (
        <View style={s.body}>
          <Text style={s.section}>GOAL</Text>
          {goal ? (
            <>
              <Text numberOfLines={4} style={s.text}>
                {goal.objective}
              </Text>
              <Text style={s.muted}>{goalTurns(goal)}</Text>
            </>
          ) : listedGoal ? (
            <Text numberOfLines={4} style={s.text}>
              {listedGoal}
            </Text>
          ) : (
            <Text style={s.muted}>No goal</Text>
          )}
          <Text style={s.section}>NOW</Text>
          <Text numberOfLines={2} style={s.mono}>
            {now}
          </Text>
          <Text style={s.section}>QUEUED</Text>
          {queue.count === 0 ? <Text style={s.muted}>Nothing queued</Text> : null}
          {queue.lines.map((line, i) => (
            <Text key={i} numberOfLines={1} style={s.text}>
              {line}
            </Text>
          ))}
          {queue.more > 0 ? <Text style={s.muted}>{`+${queue.more} more`}</Text> : null}
        </View>
      ) : null}
    </View>
  );
});

const styles = StyleSheet.create({ end: { marginLeft: "auto" } });
