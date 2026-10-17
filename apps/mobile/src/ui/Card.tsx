import { memo, type ReactNode } from "react";
import { StyleSheet, Text, View, type StyleProp, type ViewStyle } from "react-native";

import { radius, spacing, type, useTheme } from "../theme";

interface CardProps {
  /** Notched into the top border. */
  title?: string;
  /** `amber` for approvals only. */
  tone?: "edge" | "amber";
  /** The card's own background: `panel` for one that wants attention. */
  fill?: "bg" | "panel";
  style?: StyleProp<ViewStyle>;
  testID?: string;
  children: ReactNode;
}

/** A 1 px bordered card with a 12 px radius, its title (when it has one) sitting on the top
 * border over the card's own background, so the border looks cut there. */
export const Card = memo(function Card({ title, tone = "edge", fill = "bg", style, testID, children }: CardProps) {
  const theme = useTheme();
  const line = tone === "amber" ? theme.amber : theme.edge;
  const background = fill === "panel" ? theme.panel : theme.bg;
  return (
    <View testID={testID} style={[styles.card, { borderColor: line, backgroundColor: background }, title ? styles.titled : null, style]}>
      {title ? (
        <Text
          numberOfLines={1}
          style={[type.label, styles.title, { backgroundColor: background, color: tone === "amber" ? theme.amber : theme.muted }]}
        >
          {title}
        </Text>
      ) : null}
      {children}
    </View>
  );
});

const styles = StyleSheet.create({
  card: { borderWidth: 1, borderRadius: radius.card, padding: spacing.md },
  titled: { paddingTop: spacing.lg },
  // Half the label's line height above the border, so the border runs through its middle.
  title: { position: "absolute", top: -8, left: spacing.md - spacing.xs, paddingHorizontal: spacing.xs, fontWeight: "700" },
});
