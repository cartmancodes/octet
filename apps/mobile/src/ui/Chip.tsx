import { memo } from "react";
import { Pressable, StyleSheet, Text, type AccessibilityActionEvent, type AccessibilityActionInfo } from "react-native";

import { spacing, type, useTheme } from "../theme";

interface ChipProps {
  label: string;
  /** A mark before the label: ● or ○. */
  dot?: string;
  /** The dot's colour; muted when not given. */
  dotColor?: string;
  /** The label's colour; fg when not given. */
  color?: string;
  /** Must be stable. */
  onPress: () => void;
  onLongPress?: () => void;
  accessibilityLabel?: string;
  /** What a screen reader adds, such as what a long press does. */
  accessibilityHint?: string;
  /** A screen reader's way to what a gesture does. Must be stable, with its handler. */
  accessibilityActions?: readonly AccessibilityActionInfo[];
  onAccessibilityAction?: (event: AccessibilityActionEvent) => void;
  disabled?: boolean;
}

/** A rounded choice that cycles its value on a tap: at least 44 pt high. */
export const Chip = memo(function Chip({
  label,
  dot,
  dotColor,
  color,
  onPress,
  onLongPress,
  accessibilityLabel,
  accessibilityHint,
  accessibilityActions,
  onAccessibilityAction,
  disabled = false,
}: ChipProps) {
  const theme = useTheme();
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={accessibilityLabel ?? label}
      accessibilityHint={accessibilityHint}
      accessibilityActions={accessibilityActions}
      onAccessibilityAction={onAccessibilityAction}
      accessibilityState={{ disabled }}
      disabled={disabled}
      onPress={onPress}
      onLongPress={onLongPress}
      style={[styles.chip, { borderColor: theme.edge }, disabled ? styles.off : null]}
    >
      {dot ? <Text style={[type.body, { color: dotColor ?? theme.muted }]}>{dot}</Text> : null}
      <Text numberOfLines={1} style={[type.body, { color: color ?? theme.fg }]}>
        {label}
      </Text>
    </Pressable>
  );
});

const styles = StyleSheet.create({
  chip: {
    minHeight: 44,
    minWidth: 44,
    flexDirection: "row",
    alignItems: "center",
    gap: 6,
    paddingHorizontal: spacing.md,
    borderWidth: 1,
    borderRadius: 22,
  },
  off: { opacity: 0.4 },
});
