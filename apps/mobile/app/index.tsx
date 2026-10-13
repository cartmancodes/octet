import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { Stack, useIsFocused, useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { HubBanner } from "../src/components/HubBanner";
import { SessionCard } from "../src/components/SessionCard";
import { useHubClient } from "../src/hub";
import { Mascot } from "../src/mascot/Mascot";
import type { SessionInfo } from "../src/protocol";
import { headerPose, sortSessions } from "../src/sessions";
import { useHub, useListed, useSessions } from "../src/store";
import { radius, spacing, type, useTheme } from "../src/theme";

/** The mascot is drawn at one whole multiple of its 59-unit width. */
const MASCOT = 59;
/** The pull-to-refresh spinner stops by itself if the Mac does not answer. */
const REFRESH_MAX_MS = 8_000;

const keyOf = (s: SessionInfo): string => s.id;

/** The gear in the header: opens Settings. */
function SettingsButton() {
  const theme = useTheme();
  const router = useRouter();
  const open = useCallback(() => router.push("/settings"), [router]);
  return (
    <Pressable accessibilityRole="button" accessibilityLabel="Settings" onPress={open} style={styles.gear}>
      <Text style={[type.title, { color: theme.fg }]}>{"\u2699"}</Text>
    </Pressable>
  );
}

/** The live sessions, approvals waiting first. The mascot at the top is the worst state across them. */
export default function Sessions() {
  const theme = useTheme();
  const router = useRouter();
  const client = useHubClient();
  const sessions = useSessions();
  const hub = useHub();
  const listed = useListed();
  const focused = useIsFocused();
  const [pulled, setPulled] = useState(false);

  const sorted = useMemo(() => sortSessions(sessions), [sessions]);
  const pose = headerPose(sessions, hub);
  const waiting = useMemo(() => sessions.reduce((n, s) => n + (s.approvals > 0 ? 1 : 0), 0), [sessions]);
  const reachable = hub === "ready";

  const open = useCallback(
    (id: string) => router.push({ pathname: "/session/[id]", params: { id } }),
    [router],
  );
  const renderItem = useCallback<ListRenderItem<SessionInfo>>(
    ({ item }) => <SessionCard info={item} disabled={!reachable} onOpen={open} />,
    [reachable, open],
  );
  const openRecent = useCallback(() => router.push("/recent"), [router]);

  // A pull is a fresh connection: stopping and starting says `hello` again and asks for the list.
  const refresh = useCallback(() => {
    if (!client) return;
    client.stop();
    client.start();
    setPulled(true);
  }, [client]);
  useEffect(() => {
    if (!pulled) return;
    const timer = setTimeout(() => setPulled(false), REFRESH_MAX_MS);
    return () => clearTimeout(timer);
  }, [pulled]);
  // The Mac answered: the pull is over, so a later drop does not bring the spinner back. (A
  // pull stops the client first, so the render it causes sees the Mac unreachable.)
  if (pulled && reachable) setPulled(false);
  const refreshing = pulled && !reachable;

  const empty = useMemo(
    () =>
      reachable && !listed ? (
        <View style={styles.empty}>
          <Text style={[type.body, styles.emptyText, { color: theme.muted }]}>Loading sessions…</Text>
        </View>
      ) : reachable ? (
        <View style={styles.empty}>
          <Text style={[type.body, styles.emptyText, { color: theme.muted }]}>
            No Octet sessions are running on your Mac
          </Text>
          <Pressable
            accessibilityRole="button"
            accessibilityLabel="Recent and new"
            onPress={openRecent}
            style={[styles.button, { borderColor: theme.accent }]}
          >
            <Text style={[type.body, { color: theme.accent }]}>Recent and new</Text>
          </Pressable>
        </View>
      ) : null,
    [reachable, listed, openRecent, theme],
  );

  return (
    <View style={[styles.screen, { backgroundColor: theme.bg }]}>
      <Stack.Screen options={HEADER} />
      <View style={styles.header}>
        <Mascot pose={pose} size={MASCOT} />
        <Text style={[type.body, { color: theme.muted }]}>{summary(sessions.length, waiting)}</Text>
      </View>
      <HubBanner state={hub} announce={focused} />
      <FlashList
        testID="sessions-list"
        data={sorted}
        extraData={reachable}
        renderItem={renderItem}
        keyExtractor={keyOf}
        ListEmptyComponent={empty}
        refreshing={refreshing}
        onRefresh={refresh}
        contentContainerStyle={styles.list}
      />
    </View>
  );
}

function summary(total: number, waiting: number): string {
  if (total === 0) return "";
  const sessions = `${total} ${total === 1 ? "session" : "sessions"}`;
  return waiting > 0 ? `${sessions} · ${waiting} waiting for approval` : sessions;
}

// The header calls this as a function, not as a component: the hooks live in SettingsButton.
const renderGear = () => <SettingsButton />;
const HEADER = { headerRight: renderGear };

const styles = StyleSheet.create({
  screen: { flex: 1 },
  gear: { minHeight: 44, minWidth: 44, alignItems: "center", justifyContent: "center" },
  header: {
    flexDirection: "row",
    alignItems: "center",
    gap: spacing.md,
    paddingHorizontal: spacing.lg,
    paddingVertical: spacing.md,
  },
  list: { paddingBottom: spacing.xxl },
  empty: { alignItems: "center", gap: spacing.lg, padding: spacing.xl },
  emptyText: { textAlign: "center" },
  button: {
    minHeight: 44,
    minWidth: 44,
    paddingHorizontal: spacing.xl,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
    borderWidth: 1,
  },
});
