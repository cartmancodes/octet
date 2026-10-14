import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { useIsFocused, useRouter } from "expo-router";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { StyleSheet, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { NO_FACE_ID, allowAll, approvalKey, createAnswerLedger, useArm } from "../src/approvals";
import { DevBuildBanner } from "../src/components/DevBuildBanner";
import { HubBanner } from "../src/components/HubBanner";
import { InboxCard } from "../src/components/InboxCard";
import { failure, useAlive, useSend } from "../src/components/send";
import { openDrawer, useDrawerOpen } from "../src/drawer";
import { successTap } from "../src/haptics";
import { useHubClient } from "../src/hub";
import { classify, macName, type InboxItem } from "../src/inbox";
import { Mascot } from "../src/mascot/Mascot";
import type { ApprovalInfo } from "../src/protocol";
import { octetStore, useHub, useListed, useSessions } from "../src/store";
import { spacing, type, useTheme } from "../src/theme";
import { BAR_GAP, BAR_HEIGHT, Button, Segmented } from "../src/ui";

/** The mascot is drawn at one whole multiple of its 59-unit width. */
const MASCOT = 59;
/** The pull-to-refresh spinner stops by itself if the Mac does not answer. */
const REFRESH_MAX_MS = 8_000;
/** On a live link the list is pushed as it changes: a pull only waits this long for a sessions frame. */
export const LIVE_PULL_MS = 1_000;
/** How often the cards' "how long ago" moves on. */
const AGO_TICK_MS = 30_000;
/** Allow all shows from this many approvals the Inbox can answer. */
const ALL_FROM = 2;

type Tab = "needs" | "running" | "done";

const EMPTY: Record<Tab, { pose: "success" | "sleeping"; text: string; offline: string }> = {
  needs: { pose: "success", text: "Nothing needs you.", offline: "Nothing needs you (offline)." },
  running: { pose: "sleeping", text: "Nothing running.", offline: "Nothing running (offline)." },
  done: { pose: "sleeping", text: "No finished sessions.", offline: "No finished sessions (offline)." },
};

const keyOf = (item: InboxItem): string => item.id;
const readClock = (): number => Date.now();

/** Whether `id` is still `session`'s front approval, as the store has it this instant. */
const isFront = (session: string, id: number): boolean =>
  octetStore.getState().sessions.find((s) => s.id === session)?.approval?.id === id;

/** The approval `session` shows now, as the store has it this instant, or null. */
const frontNow = (session: string): ApprovalInfo | null =>
  octetStore.getState().sessions.find((s) => s.id === session)?.approval ?? null;

/** The Inbox: the live sessions in three tabs (Needs you, Running, Done), approvals answered right
 * on their cards. */
export default function Inbox() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const client = useHubClient();
  const sessions = useSessions();
  const hub = useHub();
  const listed = useListed();
  const focused = useIsFocused();
  // The drawer is over the cards: they answer nothing while it is up, and arm again once it closes.
  const covered = useDrawerOpen();
  const send = useSend();
  const alive = useAlive();
  const reachable = hub === "ready";
  const offline = !reachable;
  const mac = macName(client?.url);
  const [tab, setTab] = useState<Tab>("needs");
  // "live": pulled on a live link; "connect": pulled while the Mac could not be reached.
  const [pull, setPull] = useState<"live" | "connect" | null>(null);

  // The clock the cards count "ago" from, moved on by a timer rather than read during render.
  const [now, setNow] = useState(readClock);
  useEffect(() => {
    const timer = setInterval(() => setNow(readClock()), AGO_TICK_MS);
    return () => clearInterval(timer);
  }, []);
  const inbox = useMemo(() => {
    const fronts = new Map(sessions.map((s) => [s.id, s.approval ?? null]));
    return classify(sessions, fronts, { mac, now });
  }, [sessions, mac, now]);

  // The answers out and given from this screen, shared by the cards and Allow all. What it has
  // given is read as a store, so Allow all's count drops the moment a card's answer is taken.
  const [ledger] = useState(createAnswerLedger);
  const given = useSyncExternalStore(ledger.subscribe, ledger.snapshot, ledger.snapshot);
  // Every front approval listed: the ledger forgets an answer once its request is gone.
  const listedKeys = useMemo(
    () => inbox.needs.flatMap((i) => (i.approval ? [approvalKey(i.id, i.approval)] : [])),
    [inbox.needs],
  );
  useEffect(() => ledger.keep(listedKeys), [ledger, listedKeys]);
  // What Allow all may answer: whole requests (a shortened one is reviewed in its session) not
  // already answered from a card.
  const answerable = useMemo(
    () =>
      inbox.needs.flatMap((i) =>
        i.approval && !i.shortened && !given.has(approvalKey(i.id, i.approval)) ? [{ session: i.id, approval: i.approval }] : [],
      ),
    [inbox.needs, given],
  );
  const keys = useMemo(() => answerable.map((a) => approvalKey(a.session, a.approval)), [answerable]);
  const batchKey = keys.join("\n");

  // Allow all shows, and arms, only on Needs you: coming back to it arms it again.
  const showAll = tab === "needs" && answerable.length >= ALL_FROM;
  const allArmed = useArm(batchKey, reachable && showAll && !covered);

  // What is drawn, and whether the link is down or the drawer is over the cards, for an Allow all
  // awaiting Face ID: layout effects, so a change is seen in the commit that makes it. `drops` moves
  // on with each render while the Mac can't be reached: a link that dropped and came back during
  // Face ID answers nothing.
  const shown = useRef<ReadonlySet<string>>(new Set());
  const down = useRef(offline);
  const drops = useRef(0);
  useLayoutEffect(() => {
    shown.current = new Set(showAll ? keys : []);
    down.current = offline || covered;
    if (offline) drops.current += 1;
  });

  const [batchBusy, setBatchBusy] = useState(false);
  // Belongs to the requests it was for: it goes when they change.
  const [batchError, setBatchError] = useState<{ batch: string; text: string } | null>(null);
  const batchFailure = batchError && batchError.batch === batchKey ? batchError.text : null;
  const batchOut = useRef(false);
  const allowEvery = useCallback(async () => {
    if (batchOut.current || !allArmed || down.current) return;
    batchOut.current = true;
    const batch = batchKey;
    const since = drops.current;
    setBatchBusy(true);
    setBatchError(null);
    let taken = 0;
    try {
      const result = await allowAll(answerable, async ({ session, approval }) => {
        // Each still drawn as it was when tapped (so shown and armed), still the session's front
        // one, and not answered from its card; and the link never dropped meanwhile. A request
        // that closed or arrived since is left alone.
        const key = approvalKey(session, approval);
        if (!alive() || down.current || drops.current !== since || !shown.current.has(key)) return;
        const front = frontNow(session);
        if (!front || approvalKey(session, front) !== key) return;
        if (!ledger.claim(key)) return;
        let ok = false;
        try {
          await send({ type: "answer", session, approval: approval.id, allow: true });
          ok = true;
          taken += 1;
        } catch (e) {
          if (alive()) setBatchError({ batch, text: failure(e).text });
        } finally {
          ledger.finish(key, ok);
        }
      });
      if (result === "unavailable" && alive()) setBatchError({ batch, text: NO_FACE_ID });
      if (taken > 0) successTap();
    } finally {
      batchOut.current = false;
      if (alive()) setBatchBusy(false);
    }
  }, [allArmed, answerable, batchKey, alive, ledger, send]);
  const pressAll = useCallback(() => void allowEvery(), [allowEvery]);

  const open = useCallback((id: string) => router.push({ pathname: "/session/[id]", params: { id } }), [router]);
  const openNew = useCallback(() => router.push("/new"), [router]);
  const openMenu = useCallback(() => openDrawer(), []);

  const renderItem = useCallback<ListRenderItem<InboxItem>>(
    ({ item }) => <InboxCard item={item} offline={offline} covered={covered} send={send} ledger={ledger} isFront={isFront} onOpen={open} />,
    [offline, covered, send, ledger, open],
  );

  // On a live link the sessions are pushed as they change, so a pull keeps the link (no offline
  // flash) and only waits briefly for the next list. Otherwise it tries the Mac now, skipping
  // what is left of the backoff.
  const refresh = useCallback(() => {
    if (!client) return;
    if (client.state === "ready") {
      setPull("live");
      return;
    }
    client.retry();
    setPull("connect");
  }, [client]);
  useEffect(() => {
    if (pull === null) return;
    const timer = setTimeout(() => setPull(null), pull === "live" ? LIVE_PULL_MS : REFRESH_MAX_MS);
    const off =
      pull === "live" && client
        ? client.onBatch((frames) => {
            if (frames.some((f) => f.type === "sessions")) setPull(null);
          })
        : null;
    return () => {
      clearTimeout(timer);
      off?.();
    };
  }, [pull, client]);
  // The Mac answered: the pull is over, so a later drop does not bring the spinner back.
  if (pull === "connect" && reachable) setPull(null);
  const refreshing = pull === "live" || (pull === "connect" && !reachable);

  const tabs = useMemo(
    () => [
      { key: "needs" as const, label: counted("Needs you", inbox.needs.length) },
      { key: "running" as const, label: counted("Running", inbox.running.length) },
      { key: "done" as const, label: "Done" },
    ],
    [inbox.needs.length, inbox.running.length],
  );

  const empty = useMemo(
    () =>
      !listed ? (
        reachable ? (
          <View style={styles.empty}>
            <Text style={[type.body, { color: theme.muted }]}>Loading sessions…</Text>
          </View>
        ) : null
      ) : (
        <View style={styles.empty}>
          <Mascot pose={EMPTY[tab].pose} size={MASCOT} />
          <Text style={[type.body, { color: theme.muted }]}>{reachable ? EMPTY[tab].text : EMPTY[tab].offline}</Text>
        </View>
      ),
    [listed, reachable, tab, theme],
  );

  return (
    <View style={[styles.screen, { backgroundColor: theme.bg, paddingTop: insets.top }]}>
      <View style={styles.header}>
        <Button label="☰" accessibilityLabel="Menu" variant="bare" onPress={openMenu} />
        <View style={styles.title}>
          <Text style={[type.body, styles.bold, { color: theme.fg }]}>Inbox</Text>
          <Text style={[type.label, { color: theme.muted }]}>
            <Text style={{ color: reachable ? theme.accent : theme.muted }}>●</Text>
            {` ${mac} · ${reachable ? "online" : "offline"}`}
          </Text>
        </View>
        <Button label="+" accessibilityLabel="New session" variant="bare" onPress={openNew} />
      </View>
      <DevBuildBanner inset />
      <HubBanner state={hub} announce={focused} />
      <View style={styles.tabs}>
        <Segmented options={tabs} value={tab} onChange={setTab} />
      </View>
      <FlashList
        testID="inbox-list"
        data={inbox[tab]}
        extraData={Number(offline) * 2 + Number(covered)}
        renderItem={renderItem}
        keyExtractor={keyOf}
        ItemSeparatorComponent={Gap}
        ListEmptyComponent={empty}
        refreshing={refreshing}
        onRefresh={refresh}
        contentContainerStyle={styles.list}
      />
      <View style={[styles.bottom, { paddingBottom: Math.max(insets.bottom, BAR_GAP) }]}>
        {batchFailure ? <Text style={[type.body, { color: theme.amber }]}>{batchFailure}</Text> : null}
        {showAll ? (
          <Button
            label={`Allow all ${answerable.length} once`}
            variant="primary"
            disabled={!allArmed || batchBusy}
            busy={batchBusy}
            onPress={pressAll}
            style={styles.main}
          />
        ) : (
          <Button label="New session" accessibilityLabel="Start a new session" variant="primary" onPress={openNew} style={styles.main} />
        )}
      </View>
    </View>
  );
}

/** "Running 2"; just "Running" for none. */
const counted = (label: string, n: number): string => (n > 0 ? `${label} ${n}` : label);

const Gap = () => <View style={styles.gap} />;

const styles = StyleSheet.create({
  screen: { flex: 1 },
  header: { flexDirection: "row", alignItems: "center", gap: spacing.xs, paddingHorizontal: spacing.sm, paddingBottom: spacing.md },
  title: { flex: 1, alignItems: "center" },
  bold: { fontWeight: "700" },
  tabs: { marginHorizontal: 20, marginBottom: 14 },
  list: { paddingHorizontal: 20, paddingTop: 2, paddingBottom: spacing.md },
  gap: { height: spacing.md },
  empty: { alignItems: "center", gap: 14, padding: spacing.xl },
  bottom: { paddingHorizontal: 20, paddingTop: spacing.md, gap: spacing.sm },
  main: { minHeight: BAR_HEIGHT },
});
