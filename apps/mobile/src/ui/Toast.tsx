import { memo, useCallback, useMemo } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { hideToast, useToast } from "../toast";
import { radius, spacing, type, useTheme } from "../theme";

/** A screen's bottom action button (New session, Allow all): its height. */
export const BAR_HEIGHT = 52;
/** The space under a screen's bottom button: the home indicator, or at least this. */
export const BAR_GAP = spacing.lg;

/** How far above the screen's bottom edge the toast sits: clear of the home indicator and of a
 * bottom button, with a gap. */
export const toastBottom = (insetBottom: number): number =>
  Math.max(insetBottom, BAR_GAP) + BAR_HEIGHT + spacing.md + spacing.sm;

/** The toast, over the bottom of every screen: the words, and its action when it has one. The
 * words are drawn as text. Mounted once, in app/_layout.tsx. */
export const ToastHost = memo(function ToastHost() {
  const toast = useToast();
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const place = useMemo(() => ({ bottom: toastBottom(insets.bottom) }), [insets.bottom]);
  const palette = useMemo(
    () => ({
      box: { backgroundColor: theme.panel, borderColor: theme.edge },
      text: { color: theme.fg },
      action: { color: theme.accent },
    }),
    [theme],
  );
  const action = toast?.action;
  const run = useCallback(() => {
    hideToast();
    action?.();
  }, [action]);
  if (!toast) return null;
  return (
    <View testID="toast-wrap" pointerEvents="box-none" style={[styles.wrap, place]}>
      <View testID="toast" accessibilityLiveRegion="polite" style={[styles.box, palette.box]}>
        <Text style={[type.body, styles.text, palette.text]}>{toast.text}</Text>
        {toast.action && toast.label ? (
          <Pressable accessibilityRole="button" accessibilityLabel={toast.label} onPress={run} style={styles.action}>
            <Text style={[type.body, palette.action]}>{toast.label}</Text>
          </Pressable>
        ) : null}
      </View>
    </View>
  );
});

const styles = StyleSheet.create({
  wrap: { position: "absolute", left: spacing.lg, right: spacing.lg },
  box: {
    flexDirection: "row",
    alignItems: "center",
    gap: spacing.md,
    minHeight: 44,
    paddingLeft: spacing.lg,
    paddingRight: spacing.xs,
    borderRadius: radius.card,
    borderWidth: 1,
  },
  text: { flex: 1, paddingVertical: spacing.md },
  action: { minHeight: 44, minWidth: 44, paddingHorizontal: spacing.md, alignItems: "center", justifyContent: "center" },
});
