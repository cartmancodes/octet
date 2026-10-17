import { memo } from "react";
import { StyleSheet, Text, View } from "react-native";

import { isDevBuild } from "../build";
import { radius, spacing, type, useTheme } from "../theme";

export { isDevBuild };

/** What a development build says on Pair, Sessions and Settings (see src/build.ts for why). */
export const DEV_BUILD_WARNING = "Development build — don't pair with your everyday Mac";

/** The amber strip a development build shows on Pair, Sessions and Settings; nothing in a release build. */
export const DevBuildBanner = memo(function DevBuildBanner({ inset = false }: { inset?: boolean }) {
  const theme = useTheme();
  if (!isDevBuild()) return null;
  return (
    <View
      accessible
      accessibilityLabel={DEV_BUILD_WARNING}
      style={[styles.banner, inset && styles.inset, { backgroundColor: theme.panel, borderColor: theme.amber }]}
    >
      <Text style={[type.body, { color: theme.amber, fontWeight: "600" }]}>{DEV_BUILD_WARNING}</Text>
    </View>
  );
});

const styles = StyleSheet.create({
  banner: {
    paddingVertical: spacing.sm,
    paddingHorizontal: spacing.lg,
    borderRadius: radius.md,
    borderWidth: 1,
  },
  /** On a screen with no padding of its own. */
  inset: { marginHorizontal: spacing.lg, marginBottom: spacing.sm },
});
