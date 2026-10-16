import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { Stack, useIsFocused, useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import type { HubState } from "../src/client/HubClient";
import { HubBanner } from "../src/components/HubBanner";
import { RecentRow } from "../src/components/RecentRow";
import { failure, sendTo, useAlive, type Failure } from "../src/components/send";
import { useLinks, useMacNames } from "../src/hub";
import type { RecentInfo } from "../src/protocol";
import { mergeRecent, recentItems, recentKey, type RecentItem } from "../src/recent";
import { useStartSession } from "../src/start";
import { useConnecting, useHub, useReadyMacs } from "../src/store";
import { radius, spacing, type, useTheme } from "../src/theme";

/** What one Mac answered last. */
interface Answer {
  /** Null until its first list. */
  items: RecentInfo[] | null;
  failed: Failure | null;
  /** When its list last came, in ms: its rows count "ago" from it, so another Mac's answer
   * moves none of them. */
  at: number;
}

interface Loaded {
  /** Each Mac asked, by id. */
  byMac: ReadonlyMap<string, Answer>;
  /** Whether a load that follows an earlier one is out (the pull spinner). */
  pulling: boolean;
}

const NOT_YET: Answer = { items: null, failed: null, at: 0 };
const BAD_ANSWER: Failure = { text: "Something went wrong. Try again.", retry: true };

/** Resume a recent session. The hub asks for a terminal UI in tmux on the Mac; when it shows in
 * Sessions the app opens it. (A new session starts on /new.) With several Macs, every online Mac
 * is asked at once and their lists are merged by time, each row naming its Mac; a Mac that fails
 * says so on its own line, and the others still show. */
export default function Recent() {
  const theme = useTheme();
  const router = useRouter();
  const alive = useAlive();
  const links = useLinks();
  const names = useMacNames();
  const several = names.size > 1;
  const only = several ? undefined : names.keys().next().value;
  const ready = useReadyMacs();
  const connecting = useConnecting();
  const onlyHub = useHub(only ?? "");
  const focused = useIsFocused();
  const [loaded, setLoaded] = useState<Loaded>({ byMac: new Map(), pulling: false });
  // The newest request to each Mac: only its answer is kept. And the Macs still to answer.
  const loadIds = useRef(new Map<string, number>());
  const out = useRef(new Set<string>());
  const open = useCallback((id: string) => router.replace({ pathname: "/session/[id]", params: { id } }), [router]);
  const { start, failed, locked, notice } = useStartSession(open, only);

  const answer = useCallback((mac: string, change: (a: Answer) => Answer) => {
    out.current.delete(mac);
    const done = out.current.size === 0;
    setLoaded((l) => ({
      byMac: new Map(l.byMac).set(mac, change(l.byMac.get(mac) ?? NOT_YET)),
      pulling: done ? false : l.pulling,
    }));
  }, []);

  // Asks each Mac. Reading is always safe to repeat, so a failure of any kind offers Try again.
  const ask = useCallback(
    (mac: string) => {
      const mine = (loadIds.current.get(mac) ?? 0) + 1;
      loadIds.current.set(mac, mine);
      out.current.add(mac);
      sendTo(links, mac, { type: "recent" }).then(
        (frame) => {
          if (!alive() || loadIds.current.get(mac) !== mine) return;
          const items = recentItems(frame);
          if (items) answer(mac, () => ({ items, failed: null, at: Date.now() }));
          else answer(mac, (a) => ({ ...a, failed: BAD_ANSWER }));
        },
        (error: unknown) => {
          if (!alive() || loadIds.current.get(mac) !== mine) return;
          answer(mac, (a) => ({ ...a, failed: failure(error) }));
        },
      );
    },
    [links, alive, answer],
  );
  // Pull to refresh, Try again, and a Mac coming back: the spinner (or the cleared failure) shows
  // at once.
  const load = useCallback(
    (which: readonly string[]) => {
      if (which.length === 0) return;
      setLoaded((l) => {
        const byMac = new Map(l.byMac);
        for (const mac of which) byMac.set(mac, { ...(byMac.get(mac) ?? NOT_YET), failed: null });
        return { ...l, byMac, pulling: [...l.byMac.values()].some((a) => a.items !== null) };
      });
      for (const mac of which) ask(mac);
    },
    [ask],
  );
  const loadAll = useCallback(() => load([...ready]), [load, ready]);
  // On open, and again whenever a Mac's connection comes back: its list may have changed.
  const before = useRef<ReadonlySet<string>>(new Set());
  useEffect(() => {
    const back = [...ready].filter((mac) => !before.current.has(mac));
    before.current = ready;
    load(back);
  }, [ready, load]);

  const onResume = useCallback(
    (r: RecentItem) => start({ type: "start", engine: r.engine, workspace: r.workspace, resume: r.session }, r.mac),
    [start],
  );

  // The paired Macs' lists, merged; a Mac no longer paired is left out. A row that did not change
  // keeps its item, so it does not draw again when another Mac answers.
  const [merged, setMerged] = useState<{ byMac: Loaded["byMac"] | null; names: typeof names | null; items: RecentItem[] }>({
    byMac: null,
    names: null,
    items: [],
  });
  let items = merged.items;
  if (merged.byMac !== loaded.byMac || merged.names !== names) {
    items = mergeRecent(
      [...names].flatMap(([mac, macName]) => {
        const got = loaded.byMac.get(mac)?.items;
        return got ? [{ mac, macName, items: got }] : [];
      }),
      merged.items,
    );
    setMerged({ byMac: loaded.byMac, names, items });
  }
  const failures = useMemo(
    () => [...names].flatMap(([mac, macName]) => {
      const f = loaded.byMac.get(mac)?.failed;
      return f ? [{ mac, macName, failed: f }] : [];
    }),
    [names, loaded.byMac],
  );
  const anyList = [...loaded.byMac.values()].some((a) => a.items !== null);

  // Nothing can be resumed while its Mac can't be reached, as on the Session screen.
  const renderItem = useCallback<ListRenderItem<RecentItem>>(
    ({ item }) => (
      <RecentRow
        item={item}
        macName={several ? item.macName : ""}
        now={loaded.byMac.get(item.mac)?.at ?? 0}
        disabled={locked || !ready.has(item.mac)}
        onResume={onResume}
      />
    ),
    [several, loaded.byMac, locked, ready, onResume],
  );
  const extraData = useMemo(() => ({ locked, ready }), [locked, ready]);
  const empty = useMemo(
    () =>
      anyList ? (
        <Text style={[type.body, styles.center, { color: theme.muted }]}>No recent sessions</Text>
      ) : failures.length === 0 && ready.size > 0 ? (
        <Text style={[type.body, styles.center, { color: theme.muted }]}>Loading recent sessions…</Text>
      ) : null,
    [anyList, failures.length, ready.size, theme],
  );

  const bannerState: HubState = several
    ? ready.size > 0
      ? "ready"
      : connecting
        ? "connecting"
        : "offline"
    : onlyHub;
  const oneFailed = several ? null : (failures[0]?.failed ?? null);

  return (
    <View style={[styles.screen, { backgroundColor: theme.bg }]}>
      <Stack.Screen options={{ title: "Resume a session" }} />
      <HubBanner state={bannerState} announce={focused} several={several} />
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
      {oneFailed ? (
        <View accessibilityLiveRegion="polite" style={styles.status}>
          <Text style={[type.body, { color: theme.fg }]}>{oneFailed.text}</Text>
          <Pressable
            accessibilityRole="button"
            accessibilityLabel="Try again"
            onPress={loadAll}
            style={[styles.button, { borderColor: theme.accent }]}
          >
            <Text style={[type.body, { color: theme.accent }]}>Try again</Text>
          </Pressable>
        </View>
      ) : null}
      {several
        ? failures.map((f) => (
            <View key={f.mac} testID={`recent-failed-${f.mac}`} accessibilityLiveRegion="polite" style={styles.failed}>
              <Text style={[type.caption, { color: theme.fg }]}>{f.macName}</Text>
              <Text numberOfLines={1} style={[type.caption, { color: theme.muted }]}>
                {f.failed.text}
              </Text>
            </View>
          ))
        : null}
      <FlashList
        testID="recent-list"
        data={items}
        extraData={extraData}
        renderItem={renderItem}
        keyExtractor={recentKey}
        ListEmptyComponent={empty}
        refreshing={loaded.pulling}
        onRefresh={loadAll}
        contentContainerStyle={styles.list}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  status: { paddingHorizontal: spacing.lg, paddingVertical: spacing.md, gap: spacing.sm },
  failed: { paddingHorizontal: spacing.lg, paddingVertical: spacing.xs, gap: 2 },
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
