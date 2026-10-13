import { memo } from "react";
import { StyleSheet, Text, View } from "react-native";

import type { HubState } from "../client/HubClient";
import { radius, spacing, type, useTheme } from "../theme";

/** What the banner over the list says for a connection that is not ready. */
export function bannerText(state: HubState): string | null {
  switch (state) {
    case "ready":
      return null;
    case "connecting":
    case "authenticating":
      return "Connecting to your Mac…";
    default:
      return "Can't reach your Mac";
  }
}

/** The strip over the list while the Mac is not reachable: what the list shows is the last the
 * Mac said. */
export const HubBanner = memo(function HubBanner({ state }: { state: HubState }) {
  const theme = useTheme();
  const text = bannerText(state);
  if (text === null) return null;
  return (
    <View
      accessible
      accessibilityRole="alert"
      accessibilityLabel={text}
      style={[styles.banner, { backgroundColor: theme.panel, borderColor: theme.edge }]}
    >
      <Text style={[type.body, { color: theme.muted }]}>{text}</Text>
    </View>
  );
});

const styles = StyleSheet.create({
  banner: {
    paddingVertical: spacing.sm,
    paddingHorizontal: spacing.lg,
    borderRadius: radius.md,
    borderWidth: StyleSheet.hairlineWidth,
    marginHorizontal: spacing.lg,
    marginBottom: spacing.sm,
  },
});
