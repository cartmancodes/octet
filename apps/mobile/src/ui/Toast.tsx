import { memo, useCallback, useEffect, useMemo } from "react";
import { AccessibilityInfo, Platform, Pressable, StyleSheet, Text, View, type AccessibilityActionEvent } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { hideToast, holdToasts, useToast } from "../toast";
import { radius, spacing, type, useTheme } from "../theme";

/** A screen's bottom action button (New session, Allow all): its height. */
export const BAR_HEIGHT = 52;
/** The space under a screen's bottom button: the home indicator, or at least this. */
export const BAR_GAP = spacing.lg;

/** How far under the safe area's top the toast sits: just under a screen's header. */
export const TOAST_TOP = 58;

/** A screen reader's way to take the toast down. */
const DISMISS = [{ name: "dismiss", label: "Dismiss" }];
const dismiss = (event: AccessibilityActionEvent) => {
  if (event.nativeEvent.actionName === "dismiss") hideToast();
};

/** The toast, at the top of every screen under its header: the words, and its action when it has
 * one. The words are drawn as text. A screen reader hears it (Android's live region; iOS has none,
 * so it is announced there), and while one runs the toast stays until it is dismissed. Mounted
 * once, in app/_layout.tsx. */
export const ToastHost = memo(function ToastHost() {
  const toast = useToast();
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const place = useMemo(() => ({ top: insets.top + TOAST_TOP }), [insets.top]);
  // While a screen reader runs, a toast does not go by itself: its user may be reading it.
  useEffect(() => {
    let live = true;
    void Promise.resolve(AccessibilityInfo.isScreenReaderEnabled()).then((on) => {
      if (live) holdToasts(on === true);
    });
    const subscription = AccessibilityInfo.addEventListener("screenReaderChanged", (on) => holdToasts(on === true));
    return () => {
      live = false;
      subscription?.remove();
      holdToasts(false);
    };
  }, []);
  const seq = toast?.seq;
  const words = toast?.text;
  useEffect(() => {
    if (seq !== undefined && words !== undefined && Platform.OS === "ios") AccessibilityInfo.announceForAccessibility(words);
  }, [seq, words]);
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
      <View testID="toast" accessibilityLiveRegion="polite" onAccessibilityEscape={hideToast} style={[styles.box, palette.box]}>
        <Text accessibilityActions={DISMISS} onAccessibilityAction={dismiss} style={[type.body, styles.text, palette.text]}>
          {toast.text}
        </Text>
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
