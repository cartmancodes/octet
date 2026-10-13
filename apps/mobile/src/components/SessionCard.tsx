import { memo, useCallback, useMemo } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import type { SessionInfo } from "../protocol";
import { clipLabel, engineLine, nowText, sessionName } from "../sessions";
import { radius, spacing, type, useTheme } from "../theme";

interface SessionCardProps {
  info: SessionInfo;
  /** The Mac is not reachable: the card is greyed and does nothing. */
  disabled: boolean;
  /** Called with the session's id. Must be stable, or every card re-renders with the list. */
  onOpen: (id: string) => void;
}

const approvalsWaiting = (n: number): string => `${n} ${n === 1 ? "approval" : "approvals"} waiting`;

/** One live session: engine and model, workspace folder, the NOW line (else the phase), the goal,
 * and an amber badge counting approvals waiting. Every string is the Mac's and drawn as text. */
export const SessionCard = memo(function SessionCard({ info, disabled, onOpen }: SessionCardProps) {
  const theme = useTheme();
  const folder = sessionName(info.workspace);
  const now = nowText(info);
  const engine = engineLine(info);
  const open = useCallback(() => onOpen(info.id), [onOpen, info.id]);
  const palette = useMemo(
    () => ({
      card: { backgroundColor: theme.panel, borderColor: theme.edge },
      badge: { backgroundColor: theme.amber },
      badgeText: { color: theme.bg },
    }),
    [theme],
  );
  const label = [
    folder,
    engine,
    clipLabel(now),
    info.goal ? clipLabel(info.goal) : null,
    info.approvals > 0 ? approvalsWaiting(info.approvals) : null,
  ]
    .filter((x) => x !== null && x !== "")
    .join(", ");
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      accessibilityState={disabled ? DISABLED : ENABLED}
      disabled={disabled}
      onPress={open}
      style={disabled ? [styles.card, palette.card, styles.grey] : [styles.card, palette.card]}
    >
      <View style={styles.top}>
        <View style={styles.titles}>
          <Text testID="folder" numberOfLines={1} style={[type.title, { color: theme.fg }]}>
            {folder}
          </Text>
          <Text numberOfLines={1} style={[type.caption, { color: theme.muted }]}>
            {engine}
          </Text>
        </View>
        {info.approvals > 0 ? (
          <View accessible accessibilityLabel={approvalsWaiting(info.approvals)} style={[styles.badge, palette.badge]}>
            <Text style={[type.caption, styles.badgeText, palette.badgeText]}>{info.approvals}</Text>
          </View>
        ) : null}
      </View>
      <Text numberOfLines={2} style={[type.mono, { color: theme.fg }]}>
        {now}
      </Text>
      {info.goal ? (
        <Text numberOfLines={2} style={[type.body, { color: theme.muted }]}>
          {info.goal}
        </Text>
      ) : null}
    </Pressable>
  );
});

const DISABLED = { disabled: true } as const;
const ENABLED = { disabled: false } as const;

const styles = StyleSheet.create({
  card: {
    minHeight: 44,
    padding: spacing.lg,
    gap: spacing.xs,
    marginHorizontal: spacing.lg,
    marginBottom: spacing.sm,
    borderRadius: radius.md,
    borderWidth: StyleSheet.hairlineWidth,
  },
  grey: { opacity: 0.5 },
  top: { flexDirection: "row", alignItems: "center", gap: spacing.sm },
  titles: { flex: 1 },
  badge: {
    minWidth: 28,
    height: 28,
    borderRadius: 14,
    paddingHorizontal: spacing.sm,
    alignItems: "center",
    justifyContent: "center",
  },
  badgeText: { fontWeight: "700" },
});
