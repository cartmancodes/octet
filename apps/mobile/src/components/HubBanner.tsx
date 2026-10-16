import { memo, useCallback, useEffect } from "react";
import { AccessibilityInfo, Platform, Pressable, StyleSheet, Text, View } from "react-native";

import type { HubState } from "../client/HubClient";
import { useFleetStatus, useHubController, useNotice } from "../hub";
import { radius, spacing, type, useTheme } from "../theme";

/** What the banner over the list says for a connection that is not ready; `several` when it
 * speaks for every paired Mac at once (none of them can be reached). */
export function bannerText(state: HubState, several: boolean = false): string | null {
  switch (state) {
    case "ready":
      return null;
    case "connecting":
    case "authenticating":
      return several ? "Connecting to your Macs…" : "Connecting to your Mac…";
    default:
      return several ? "Can't reach your Macs" : "Can't reach your Mac";
  }
}

/** Speaks `text` when it changes: by the live region on Android, by an announcement on iOS (which
 * has no live regions; announcing on Android too would speak twice). */
function useAnnounce(text: string | null, announce: boolean): void {
  useEffect(() => {
    if (text !== null && announce && Platform.OS === "ios") AccessibilityInfo.announceForAccessibility(text);
  }, [text, announce]);
}

interface HubBannerProps {
  state: HubState;
  /** False on a screen not in front, so a banner under it does not speak twice. */
  announce?: boolean;
  /** `state` stands for every paired Mac: the words speak of the Macs. */
  several?: boolean;
  /** Shows the Fleet's notice too (a Mac that unpaired this phone, by name), with Dismiss. */
  notices?: boolean;
}

/** The strip over the list while the Mac is not reachable: what the list shows is the last the
 * Mac said. A change of its words is spoken. With `notices`, the Fleet's notice shows under it
 * while some Mac is paired, until dismissed. */
export const HubBanner = memo(function HubBanner({ state, announce = true, several = false, notices = false }: HubBannerProps) {
  const theme = useTheme();
  const text = bannerText(state, several);
  useAnnounce(text, announce);
  return (
    <>
      {text !== null ? (
        <View
          accessible
          accessibilityRole="alert"
          accessibilityLiveRegion="polite"
          accessibilityLabel={text}
          style={[styles.banner, { backgroundColor: theme.panel, borderColor: theme.edge }]}
        >
          <Text style={[type.body, { color: theme.muted }]}>{text}</Text>
        </View>
      ) : null}
      {notices ? <FleetNotice announce={announce} /> : null}
    </>
  );
});

/** The Fleet's notice while some Mac is still paired: its words (the hub's Mac named in them) and
 * Dismiss. Pair shows it once no Mac is left. */
const FleetNotice = memo(function FleetNotice({ announce }: { announce: boolean }) {
  const theme = useTheme();
  const fleet = useHubController();
  const status = useFleetStatus();
  const notice = useNotice();
  const text = status === "paired" ? notice : null;
  useAnnounce(text, announce);
  const dismiss = useCallback(() => fleet.dismissNotice(), [fleet]);
  if (text === null) return null;
  return (
    <View style={[styles.banner, styles.notice, { backgroundColor: theme.panel, borderColor: theme.edge }]}>
      <Text accessibilityRole="alert" accessibilityLiveRegion="polite" style={[type.body, styles.grow, { color: theme.fg }]}>
        {text}
      </Text>
      <Pressable accessibilityRole="button" accessibilityLabel="Dismiss" onPress={dismiss} style={styles.dismiss}>
        <Text style={[type.body, { color: theme.accent }]}>Dismiss</Text>
      </Pressable>
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
  notice: { flexDirection: "row", alignItems: "center", gap: spacing.sm, paddingVertical: 0, paddingRight: spacing.sm },
  grow: { flex: 1, paddingVertical: spacing.sm },
  dismiss: { minHeight: 44, minWidth: 44, alignItems: "center", justifyContent: "center", paddingHorizontal: spacing.sm },
});
