import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { useIsFocused, useNavigationContainerRef, usePathname, useRouter } from "expo-router";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { StyleSheet, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { useAgoClock } from "../src/agoClock";
import { NO_FACE_ID, allowAll, approvalKey, createAnswerLedger, useArm } from "../src/approvals";
import { DevBuildBanner } from "../src/components/DevBuildBanner";
import { HubBanner } from "../src/components/HubBanner";
import { InboxCard } from "../src/components/InboxCard";
import { failure, useAlive, useSend } from "../src/components/send";
import { openDrawer, useDrawerOpen } from "../src/drawer";
import { successTap } from "../src/haptics";
import type { HubState } from "../src/client/HubClient";
import { useLinks, useMacNames, useMacs } from "../src/hub";
import { inboxOf, type InboxItem } from "../src/inbox";
import { fleetLine, macOfKey } from "../src/macs";
import { Mascot } from "../src/mascot/Mascot";
import { openSession } from "../src/navigation";
import type { ApprovalInfo } from "../src/protocol";
import { octetStore, useConnecting, useListed, useReadyMacs, useSessions } from "../src/store";
import { spacing, type, useTheme } from "../src/theme";
import { BAR_GAP, BAR_HEIGHT, Button, Segmented } from "../src/ui";

/** The mascot is drawn at one whole multiple of its 59-unit width. */
const MASCOT = 59;
/** The pull-to-refresh spinner stops by itself if the Mac does not answer. */
const REFRESH_MAX_MS = 8_000;
/** On a live link the list is pushed as it changes: a pull only waits this long for a sessions frame. */
export const LIVE_PULL_MS = 1_000;
/** Allow all shows from this many approvals the Inbox can answer. */
const ALL_FROM = 2;

type Tab = "needs" | "running" | "done";

const EMPTY: Record<Tab, { pose: "success" | "sleeping"; text: string; offline: string }> = {
  needs: { pose: "success", text: "Nothing needs you.", offline: "Nothing needs you (offline)." },
  running: { pose: "sleeping", text: "Nothing running.", offline: "Nothing running (offline)." },
  done: { pose: "sleeping", text: "No finished sessions.", offline: "No finished sessions (offline)." },
};

const keyOf = (item: InboxItem): string => item.id;
const NO_PULL: ReadonlyMap<string, "live" | "connect"> = new Map();

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
  const nav = useNavigationContainerRef();
  // The screen showing, for a card tap to read from a ref, so that the tap's callbacks stay the same
  // as it changes (the cards draw again only for their own data).
  const pathname = usePathname();
  const here = useRef(pathname);
  useLayoutEffect(() => {
    here.current = pathname;
  });
  const insets = useSafeAreaInsets();
  const links = useLinks();
  const macs = useMacs();
  // The paired Macs' names: with several, each card names its Mac and the header counts them.
  const names = useMacNames();
  const several = names.size > 1;
  // Offline is per Mac: a card is greyed, and answers nothing, while its own Mac is not ready.
  const ready = useReadyMacs();
  const connecting = useConnecting();
  const sessions = useSessions();
  const listed = useListed();
  const focused = useIsFocused();
  // The drawer is over the cards, or another screen is: they answer nothing then, and arm again
  // 400 ms after the Inbox is in view once more.
  const drawerOpen = useDrawerOpen();
  const covered = drawerOpen || !focused;
  const send = useSend();
  const alive = useAlive();
  // Some Mac can be reached (with one Mac: that one).
  const reachable = ready.size > 0;
  const offline = !reachable;
  const mac = macs[0]?.macName ?? "Mac";
  const online = macs.reduce((n, m) => (ready.has(m.id) ? n + 1 : n), 0);
  // What the banner over the list says: with several Macs, only when none can be reached.
  const bannerState: HubState = reachable ? "ready" : connecting ? "connecting" : "offline";
  const [tab, setTab] = useState<Tab>("needs");
  // The Macs a pull waits for: "live", pulled on a live link (until its next list); "connect",
  // pulled while that Mac could not be reached (until it can). Empty when no pull is out.
  const [pull, setPull] = useState(NO_PULL);

  // The clock the cards count "ago" from, moved on by a timer rather than read during render: the
  // drawer's too, so that it reuses these lists.
  const now = useAgoClock();
  const inbox = useMemo(() => inboxOf(sessions, several ? { mac, now, names } : { mac, now }), [sessions, mac, now, several, names]);

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
  // already answered from a card. With several Macs, only those of a Mac that can be reached (with
  // one, Allow all stays, unarmed, while it can't).
  const answerable = useMemo(
    () =>
      inbox.needs.flatMap((i) =>
        i.approval && !i.shortened && (!several || ready.has(i.mac ?? "")) && !given.has(approvalKey(i.id, i.approval))
          ? [{ session: i.id, approval: i.approval }]
          : [],
      ),
    [inbox.needs, given, ready, several],
  );
  const keys = useMemo(() => answerable.map((a) => approvalKey(a.session, a.approval)), [answerable]);
  const batchKey = keys.join("\n");

  // Allow all shows, and arms, only on Needs you: coming back to it arms it again.
  const showAll = tab === "needs" && answerable.length >= ALL_FROM;
  const allArmed = useArm(batchKey, reachable && showAll && !covered);

  // What is drawn, which Macs can be reached, and whether every link is down or the drawer is over
  // the cards, for an Allow all awaiting Face ID: layout effects, so a change is seen in the commit
  // that makes it. A Mac's count in `drops` moves on with each render while it can't be reached: a
  // link that dropped and came back during Face ID answers nothing.
  const shown = useRef<ReadonlySet<string>>(new Set());
  const down = useRef(offline);
  const readyNow = useRef(ready);
  const drops = useRef(new Map<string, number>());
  useLayoutEffect(() => {
    shown.current = new Set(showAll ? keys : []);
    down.current = offline || covered;
    readyNow.current = ready;
    for (const id of names.keys()) if (!ready.has(id)) drops.current.set(id, (drops.current.get(id) ?? 0) + 1);
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
    const since = new Map(drops.current);
    setBatchBusy(true);
    setBatchError(null);
    let taken = 0;
    try {
      const result = await allowAll(answerable, async ({ session, approval }) => {
        // Each still drawn as it was when tapped (so shown and armed), still the session's front
        // one, and not answered from its card; and the link never dropped meanwhile. A request
        // that closed or arrived since is left alone.
        const key = approvalKey(session, approval);
        const at = macOfKey(session) ?? "";
        const dropped = !readyNow.current.has(at) || (drops.current.get(at) ?? 0) !== (since.get(at) ?? 0);
        if (!alive() || down.current || dropped || !shown.current.has(key)) return;
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

  // A card opens its session the one way every other surface does: back to it when it is
  // already in the stack. A plain open spends any earlier Reply; Reply asks for the prompt.
  const open = useCallback(
    (id: string) => {
      octetStore.getState().setReplyTo(null);
      openSession(router, nav.getRootState(), here.current, id);
    },
    [router, nav],
  );
  const reply = useCallback(
    (id: string) => {
      octetStore.getState().setReplyTo(id);
      openSession(router, nav.getRootState(), here.current, id);
    },
    [router, nav],
  );
  const openNew = useCallback(() => router.push("/new"), [router]);
  const openMenu = useCallback(() => openDrawer(), []);

  // Each card is greyed by its own Mac: one Mac's link changing draws only that Mac's cards again.
  const renderItem = useCallback<ListRenderItem<InboxItem>>(
    ({ item }) => (
      <InboxCard
        item={item}
        offline={!ready.has(item.mac ?? "")}
        covered={covered}
        send={send}
        ledger={ledger}
        isFront={isFront}
        onOpen={open}
        onReply={reply}
      />
    ),
    [ready, covered, send, ledger, open, reply],
  );
  const extraData = useMemo(() => ({ ready, covered }), [ready, covered]);

  // On a live link the sessions are pushed as they change, so a pull keeps the link (no offline
  // flash) and only waits briefly for that Mac's next list. A Mac that can't be reached is tried
  // now, skipping what is left of its backoff. Every paired Mac at once.
  // Moves on with each pull: a round's timers and listeners are its own.
  const [pulled, setPulled] = useState(0);
  const refresh = useCallback(() => {
    const next = new Map<string, "live" | "connect">();
    for (const id of links.macs()) {
      const client = links.client(id);
      if (!client) continue;
      if (client.state === "ready") next.set(id, "live");
      else {
        client.retry();
        next.set(id, "connect");
      }
    }
    if (next.size === 0) return;
    setPull(next);
    setPulled((n) => n + 1);
  }, [links]);
  const pulling = pull.size > 0;
  useEffect(() => {
    if (pulled === 0 || !pulling) return;
    // The Macs this round waits for, as it began.
    const waits = pull;
    const settle = (keep: (id: string, kind: "live" | "connect") => boolean) =>
      setPull((p) => {
        const next = new Map([...p].filter(([id, kind]) => keep(id, kind)));
        return next.size === p.size ? p : next.size === 0 ? NO_PULL : next;
      });
    const timers = [
      setTimeout(() => settle((_, kind) => kind !== "live"), LIVE_PULL_MS),
      setTimeout(() => settle((_, kind) => kind !== "connect"), REFRESH_MAX_MS),
    ];
    const offs = [...waits].flatMap(([id, kind]) => {
      const client = kind === "live" ? links.client(id) : null;
      return client
        ? [
            client.onBatch((frames) => {
              if (frames.some((f) => f.type === "sessions")) settle((other) => other !== id);
            }),
          ]
        : [];
    });
    return () => {
      for (const timer of timers) clearTimeout(timer);
      for (const off of offs) off();
    };
    // A round's timers and listeners live until it is over or another begins, not with each Mac
    // that answers.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pulled, pulling, links]);
  // A Mac that was tried answered: it is done, so a later drop does not bring the spinner back.
  if ([...pull].some(([id, kind]) => kind === "connect" && ready.has(id))) {
    setPull((p) => {
      const next = new Map([...p].filter(([id, kind]) => kind !== "connect" || !ready.has(id)));
      return next.size === 0 ? NO_PULL : next;
    });
  }
  const refreshing = pulling;

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
            {several ? ` ${fleetLine(names.size, online)}` : ` ${mac} · ${reachable ? "online" : "offline"}`}
          </Text>
        </View>
        <Button label="+" accessibilityLabel="New session" variant="bare" onPress={openNew} />
      </View>
      <DevBuildBanner inset />
      <HubBanner state={bannerState} announce={focused} several={several} notices />
      <View style={styles.tabs}>
        <Segmented options={tabs} value={tab} onChange={setTab} />
      </View>
      <FlashList
        testID="inbox-list"
        data={inbox[tab]}
        extraData={extraData}
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
