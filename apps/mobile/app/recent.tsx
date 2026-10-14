import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { Stack, useIsFocused, useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { HubBanner } from "../src/components/HubBanner";
import { RecentRow } from "../src/components/RecentRow";
import { failure, useAlive, type Failure } from "../src/components/send";
import type { RecentInfo } from "../src/protocol";
import { recentItems } from "../src/recent";
import { useStartSession } from "../src/start";
import { useHub } from "../src/store";
import { radius, spacing, type, useTheme } from "../src/theme";

const keyOf = (r: RecentInfo): string => `${r.engine}:${r.workspace}:${r.session}`;

interface Loaded {
  /** `loading` until the first answer. */
  state: "loading" | "ready";
  items: RecentInfo[];
  /** Whether a load that follows an earlier one is out (the pull spinner). */
  pulling: boolean;
  failed: Failure | null;
  /** When the items came, in ms. */
  at: number;
}

/** Resume a recent session. The hub asks for a terminal UI in tmux on the Mac; when it shows in
 * Sessions the app opens it. (A new session starts on /new.) */
export default function Recent() {
  const theme = useTheme();
  const router = useRouter();
  const alive = useAlive();
  const hub = useHub();
  const focused = useIsFocused();
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading", items: [], pulling: false, failed: null, at: 0 });
  const loadId = useRef(0);
  const open = useCallback((id: string) => router.replace({ pathname: "/session/[id]", params: { id } }), [router]);
  const { send, start, failed, locked, notice } = useStartSession(open);

  // Asks the Mac. Reading is always safe to repeat, so a failure of any kind offers Try again;
  // only the newest request's answer is kept.
  const ask = useCallback(() => {
    const mine = ++loadId.current;
    send({ type: "recent" }).then(
      (frame) => {
        if (!alive() || loadId.current !== mine) return;
        const items = recentItems(frame);
        setLoaded((l) =>
          items
            ? { state: "ready", items, pulling: false, failed: null, at: Date.now() }
            : { ...l, pulling: false, failed: { text: "Something went wrong. Try again.", retry: true } },
        );
      },
      (error: unknown) => {
        if (!alive() || loadId.current !== mine) return;
        setLoaded((l) => ({ ...l, pulling: false, failed: failure(error) }));
      },
    );
  }, [send, alive]);
  // Pull to refresh and Try again: the spinner (or the cleared failure) shows at once.
  const load = useCallback(() => {
    setLoaded((l) => ({ ...l, pulling: l.state === "ready", failed: null }));
    ask();
  }, [ask]);
  // On open, and again whenever the connection comes back: the list may have changed on the Mac.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- one extra render on a reconnect, to clear a stale failure now
    if (hub === "ready") load();
  }, [hub, load]);

  // Nothing can be resumed while the Mac can't be reached, as on the Session screen.
  const onResume = useCallback(
    (r: RecentInfo) => start({ type: "start", engine: r.engine, workspace: r.workspace, resume: r.session }),
    [start],
  );

  const renderItem = useCallback<ListRenderItem<RecentInfo>>(
    ({ item }) => <RecentRow item={item} now={loaded.at} disabled={locked} onResume={onResume} />,
    [loaded.at, locked, onResume],
  );
  const empty = useMemo(
    () =>
      loaded.state === "ready" ? (
        <Text style={[type.body, styles.center, { color: theme.muted }]}>No recent sessions</Text>
      ) : loaded.failed === null && hub === "ready" ? (
        <Text style={[type.body, styles.center, { color: theme.muted }]}>Loading recent sessions…</Text>
      ) : null,
    [loaded.state, loaded.failed, hub, theme],
  );

  return (
    <View style={[styles.screen, { backgroundColor: theme.bg }]}>
      <Stack.Screen options={{ title: "Resume a session" }} />
      <HubBanner state={hub} announce={focused} />
      {notice ? (
        <View accessibilityLiveRegion="polite" style={styles.status}>
          <Text style={[type.body, { color: theme.fg }]}>{notice}</Text>
        </View>
      ) : null}
      {failed ? (
        <View accessibilityLiveRegion="polite" style={styles.status}>
          <Text style={[type.body, { color: theme.fg }]}>{failed.text}</Text>
        </View>
      ) : null}
      {loaded.failed ? (
        <View accessibilityLiveRegion="polite" style={styles.status}>
          <Text style={[type.body, { color: theme.fg }]}>{loaded.failed.text}</Text>
          <Pressable
            accessibilityRole="button"
            accessibilityLabel="Try again"
            onPress={load}
            style={[styles.button, { borderColor: theme.accent }]}
          >
            <Text style={[type.body, { color: theme.accent }]}>Try again</Text>
          </Pressable>
        </View>
      ) : null}
      <FlashList
        testID="recent-list"
        data={loaded.items}
        extraData={locked}
        renderItem={renderItem}
        keyExtractor={keyOf}
        ListEmptyComponent={empty}
        refreshing={loaded.pulling}
        onRefresh={load}
        contentContainerStyle={styles.list}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  status: { paddingHorizontal: spacing.lg, paddingVertical: spacing.md, gap: spacing.sm },
  list: { paddingBottom: spacing.xxl },
  center: { textAlign: "center", padding: spacing.xl },
  button: {
    minHeight: 44,
    minWidth: 44,
    alignSelf: "flex-start",
    paddingHorizontal: spacing.xl,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
    borderWidth: 1,
  },
});
