import { memo } from "react";
import { StyleSheet, Text, type StyleProp, type TextStyle } from "react-native";

import { type, useTheme } from "../theme";

interface LabelProps {
  children: string;
  style?: StyleProp<TextStyle>;
}

/** A section label: 12 / 16 monospace, upper-case and muted. */
export const Label = memo(function Label({ children, style }: LabelProps) {
  const theme = useTheme();
  return <Text style={[type.label, styles.label, { color: theme.muted }, style]}>{children}</Text>;
});

const styles = StyleSheet.create({ label: { textTransform: "uppercase" } });
