import { memo, useMemo } from "react";
import { Pressable, StyleSheet, Text, type StyleProp, type ViewStyle } from "react-native";

import { radius, spacing, type, useTheme } from "../theme";

interface ButtonProps {
  label: string;
  /** Must be stable in a list row. */
  onPress: () => void;
  /** What a screen reader says; the label when not given. */
  accessibilityLabel?: string;
  /** `accent`: accent border and text on the selected fill (Allow once); `primary`: the bold
   * accent outline of a screen's main action; `plain`: an edge outline; `bare`: no outline (a
   * header glyph); `link`: no outline, accent text (a quiet way on, like "Enter code instead"). */
  variant?: "accent" | "primary" | "plain" | "bare" | "link";
  disabled?: boolean;
  busy?: boolean;
  style?: StyleProp<ViewStyle>;
  testID?: string;
}

/** A button of at least 44 × 44. Disabled, it is dimmed and does nothing. */
export const Button = memo(function Button({
  label,
  onPress,
  accessibilityLabel,
  variant = "plain",
  disabled = false,
  busy = false,
  style,
  testID,
}: ButtonProps) {
  const theme = useTheme();
  const look = useMemo(() => {
    switch (variant) {
      case "accent":
        return { box: { borderColor: theme.accent, backgroundColor: theme.selected }, text: { color: theme.accent } };
      case "primary":
        return { box: { borderColor: theme.accent }, text: { color: theme.accent, fontWeight: "700" as const } };
      case "link":
        return { box: { borderColor: "transparent" }, text: { color: theme.accent } };
      case "bare":
        return { box: { borderColor: "transparent" }, text: { color: theme.fg } };
      default:
        return { box: { borderColor: theme.edge }, text: { color: theme.fg } };
    }
  }, [variant, theme]);
  return (
    <Pressable
      testID={testID}
      accessibilityRole="button"
      accessibilityLabel={accessibilityLabel ?? label}
      accessibilityState={{ disabled, busy }}
      disabled={disabled}
      onPress={onPress}
      style={[styles.button, look.box, disabled ? styles.off : null, style]}
    >
      <Text numberOfLines={1} style={[type.body, look.text]}>
        {label}
      </Text>
    </Pressable>
  );
});

const styles = StyleSheet.create({
  button: {
    minHeight: 44,
    minWidth: 44,
    paddingHorizontal: spacing.md,
    alignItems: "center",
    justifyContent: "center",
    borderWidth: 1,
    borderRadius: radius.card,
  },
  off: { opacity: 0.4 },
});
