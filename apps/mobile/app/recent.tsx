import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { Stack, useIsFocused, useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { HubBanner } from "../src/components/HubBanner";
import { NewForm, NO_WORKSPACE, type StartBody } from "../src/components/NewForm";
import { RecentRow } from "../src/components/RecentRow";
import { failure, useAction, useAlive, useSend, type Failure } from "../src/components/send";
import type { RecentInfo } from "../src/protocol";
import { tmuxOfAck, workspaceChoices } from "../src/recent";
import { useCatalogs, useHub, useListed, useSessions } from "../src/store";
import { radius, spacing, type, useTheme } from "../src/theme";

/** A launched session that has not shown in Sessions by now is probably not coming. */
export const APPEAR_MS = 30_000;
const ENGINES: readonly string[] = ["claude", "codex", "demo"];

const keyOf = (r: RecentInfo): string => `${r.engine}:${r.workspace}:${r.session}`;

/** What the hub sent, kept only if it has the shape of a `recent` answer. */
function itemsOf(frame: unknown): RecentInfo[] | null {
  if (typeof frame !== "object" || frame === null) return null;
  const { type: kind, items } = frame as { type?: unknown; items?: unknown };
  if (kind !== "recent" || !Array.isArray(items)) return null;
  return items.filter(
    (r): r is RecentInfo =>
      typeof r === "object" &&
      r !== null &&
      ENGINES.includes((r as RecentInfo).engine) &&
      typeof (r as RecentInfo).workspace === "string" &&
      typeof (r as RecentInfo).session === "string" &&
      typeof (r as RecentInfo).title === "string" &&
      typeof (r as RecentInfo).started === "number",
  );
}

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

/** A start whose session is awaited: `tmux` is the name from the ack, null when the hub's words
 * did not carry one; `late` once 30 seconds have passed. */
interface Launch {
  tmux: string | null;
  late: boolean;
}

/** Resume a recent session, or start a new one. Both ask the hub for a terminal UI in tmux on the
 * Mac; when it shows in Sessions the app opens it. */
export default function RecentAndNew() {
  const theme = useTheme();
  const router = useRouter();
  const send = useSend();
  const alive = useAlive();
  const hub = useHub();
  const listed = useListed();
  const focused = useIsFocused();
  const sessions = useSessions();
  const catalogs = useCatalogs();
  const [tab, setTab] = useState<"recent" | "new">("recent");
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading", items: [], pulling: false, failed: null, at: 0 });
  const loadId = useRef(0);
  const { busy, failed, run, fail } = useAction(send);
  const [launch, setLaunch] = useState<Launch | null>(null);
  const opened = useRef(false);

  // Asks the Mac. Reading is always safe to repeat, so a failure of any kind offers Try again;
  // only the newest request's answer is kept.
  const ask = useCallback(() => {
    const mine = ++loadId.current;
    send({ type: "recent" }).then(
      (frame) => {
        if (!alive() || loadId.current !== mine) return;
        const items = itemsOf(frame);
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

  const locked = busy || (launch !== null && launch.tmux !== null && !launch.late);

  const started = useCallback((ack: unknown) => setLaunch({ tmux: tmuxOfAck(ack), late: false }), []);
  const onResume = useCallback(
    (r: RecentInfo) => {
      opened.current = false;
      setLaunch(null); // the first start's session is no longer the one to open
      run({ type: "start", engine: r.engine, workspace: r.workspace, resume: r.session }, started);
    },
    [run, started],
  );
  const onStart = useCallback(
    (body: StartBody) => {
      opened.current = false;
      setLaunch(null);
      run(body, started);
    },
    [run, started],
  );

  // Watch for the launched session: open it when it is listed, say so if it is slow. Leaving the
  // screen ends both.
  const tmux = launch?.tmux ?? null;
  useEffect(() => {
    if (tmux === null) return;
    const timer = setTimeout(() => setLaunch((l) => (l && l.tmux === tmux ? { ...l, late: true } : l)), APPEAR_MS);
    return () => clearTimeout(timer);
  }, [tmux]);
  useEffect(() => {
    if (tmux === null || opened.current) return;
    const found = sessions.find((s) => s.tmux === tmux);
    if (!found) return;
    opened.current = true;
    router.replace({ pathname: "/session/[id]", params: { id: found.id } });
  }, [tmux, sessions, router]);

  const notice =
    launch === null
      ? null
      : launch.tmux === null
        ? "Started — it will appear in Sessions"
        : launch.late
          ? `${launch.tmux} hasn't appeared yet. Check tmux on the Mac.`
          : `Starting ${launch.tmux} — it will appear in Sessions`;

  const workspaces = useMemo(() => workspaceChoices(sessions, loaded.items), [sessions, loaded.items]);

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
  const showRecent = useCallback(() => setTab("recent"), []);
  const showNew = useCallback(() => setTab("new"), []);

  return (
    <View style={[styles.screen, { backgroundColor: theme.bg }]}>
      <Stack.Screen options={{ title: "Recent and new" }} />
      <HubBanner state={hub} announce={focused} />
      <View accessibilityRole="tablist" style={styles.tabs}>
        <Tab label="Recent" on={tab === "recent"} onPress={showRecent} />
        <Tab label="New" on={tab === "new"} onPress={showNew} />
      </View>
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
      {tab === "recent" ? (
        <>
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
        </>
      ) : workspaces.length > 0 ? (
        <NewForm workspaces={workspaces} catalogs={catalogs} locked={locked} failed={failed} onStart={onStart} onFail={fail} />
      ) : (
        <Text style={[type.body, styles.center, { color: theme.muted }]}>
          {loaded.state !== "ready" && loaded.failed === null && !listed ? "Loading…" : NO_WORKSPACE}
        </Text>
      )}
    </View>
  );
}

function Tab({ label, on, onPress }: { label: string; on: boolean; onPress: () => void }) {
  const theme = useTheme();
  return (
    <Pressable
      accessibilityRole="tab"
      accessibilityLabel={label}
      accessibilityState={{ selected: on }}
      onPress={onPress}
      style={[styles.tab, { borderColor: on ? theme.accent : theme.edge, backgroundColor: on ? theme.selected : "transparent" }]}
    >
      <Text style={[type.body, { color: on ? theme.fg : theme.muted }]}>{label}</Text>
    </Pressable>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  tabs: { flexDirection: "row", gap: spacing.sm, padding: spacing.lg },
  tab: {
    flex: 1,
    minHeight: 44,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
    borderWidth: 1,
  },
  status: { paddingHorizontal: spacing.lg, paddingBottom: spacing.md, gap: spacing.sm },
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
