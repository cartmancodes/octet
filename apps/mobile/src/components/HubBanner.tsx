import { memo, useEffect } from "react";
import { AccessibilityInfo, StyleSheet, Text, View } from "react-native";

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
 * Mac said. A change of its words is spoken: a live region on Android, an announcement on iOS.
 * `announce` is false on a screen not in front, so a banner under it does not speak twice. */
export const HubBanner = memo(function HubBanner({ state, announce = true }: { state: HubState; announce?: boolean }) {
  const theme = useTheme();
  const text = bannerText(state);
  useEffect(() => {
    if (text !== null && announce) AccessibilityInfo.announceForAccessibility(text);
  }, [text, announce]);
  if (text === null) return null;
  return (
    <View
      accessible
      accessibilityRole="alert"
      accessibilityLiveRegion="polite"
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
