import { memo } from "react";
import { Pressable, StyleSheet, Text } from "react-native";

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
  disabled?: boolean;
}

/** A rounded choice that cycles its value on a tap: at least 44 pt high. */
export const Chip = memo(function Chip({ label, dot, dotColor, color, onPress, onLongPress, accessibilityLabel, disabled = false }: ChipProps) {
  const theme = useTheme();
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={accessibilityLabel ?? label}
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
