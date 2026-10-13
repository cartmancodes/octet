import { FlashList, type FlashListRef, type ListRenderItem } from "@shopify/flash-list";
import { Stack, useFocusEffect, useIsFocused, useLocalSearchParams, useRouter } from "expo-router";
import { useCallback, useRef, useState } from "react";
import { KeyboardAvoidingView, Pressable, StyleSheet, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { Refusal } from "../../src/client/HubClient";
import { ApprovalSheet } from "../../src/components/ApprovalSheet";
import { Composer } from "../../src/components/Composer";
import { useFollow } from "../../src/components/follow";
import { HubBanner } from "../../src/components/HubBanner";
import type { Send } from "../../src/components/send";
import { SessionControls } from "../../src/components/SessionControls";
import { TaskStrip } from "../../src/components/TaskStrip";
import { TranscriptRow } from "../../src/components/TranscriptRow";
import { useHubClient } from "../../src/hub";
import type { Entry } from "../../src/model/transcript";
import { nowLine, queueWords } from "../../src/session";
import { sessionName } from "../../src/sessions";
import {
  octetStore,
  releaseSession,
  unwatch,
  useApprovals,
  useEntries,
  useHub,
  useListed,
  useSessionInfo,
  useSessionState,
  useSessionStatus,
  useTaskOpen,
  watch,
} from "../../src/store";
import { radius, spacing, type, useTheme } from "../../src/theme";

const keyOf = (entry: Entry): string => entry.key;
const typeOf = (entry: Entry): string => entry.kind;
/** FlashList 2 keeps the visible rows in place as rows above change (old ones dropped); the
 * transcript opens at its end. Following the output is `useFollow`'s. */
const KEEP_POSITION = { startRenderingFromBottom: true } as const;
const NO_OPEN: ReadonlySet<string> = new Set();

/**
 * One session: its Task strip and transcript. Shown, it streams the session (`watch`); left, it
 * stops (`unwatch`) and lets an ended session go (`releaseSession`). It reads only this
 * session's part of the store, so other sessions' output never draws it again.
 */
export default function Session() {
  const params = useLocalSearchParams<{ id: string }>();
  const id = typeof params.id === "string" ? params.id : "";
  const theme = useTheme();
  const router = useRouter();
  const client = useHubClient();
  const focused = useIsFocused();
  const info = useSessionInfo(id);
  const entries = useEntries(id);
  const state = useSessionState(id);
  const status = useSessionStatus(id);
  const listed = useListed();
  const hub = useHub();
  const taskOpen = useTaskOpen();
  const approvals = useApprovals(id);
  const insets = useSafeAreaInsets();
  const setTaskOpen = octetStore.getState().setTaskOpen;

  useFocusEffect(
    useCallback(() => {
      if (!client || id === "") return;
      // An ended session's view is what it left behind: there is nothing more to stream.
      if (!octetStore.getState().bySession.get(id)?.ended) watch(octetStore, client, id);
      return () => {
        unwatch(client, id);
        releaseSession(octetStore, client, id);
      };
    }, [client, id]),
  );

  // The title outlives the session's place in the list.
  const [workspace, setWorkspace] = useState(info?.workspace);
  if (info && info.workspace !== workspace) setWorkspace(info.workspace);
  const title = sessionName(workspace);

  // Tools entries the user opened, by key: kept here, not in the row, which FlashList recycles.
  const [open, setOpen] = useState<ReadonlySet<string>>(NO_OPEN);
  // A replay from the start numbers the entries again: the keys opened mean nothing now.
  const [epoch, setEpoch] = useState(status.epoch);
  if (epoch !== status.epoch) {
    setEpoch(status.epoch);
    setOpen(NO_OPEN);
  }
  const toggle = useCallback((key: string) => {
    setOpen((prev) => {
      const next = new Set(prev);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  }, []);
  const renderItem = useCallback<ListRenderItem<Entry>>(
    ({ item }) => <TranscriptRow entry={item} open={open.has(item.key)} onToggle={toggle} />,
    [open, toggle],
  );

  const list = useRef<FlashListRef<Entry>>(null);
  const follow = useFollow(list, entries[entries.length - 1]);

  const back = useCallback(() => {
    if (router.canGoBack()) router.back();
    else router.replace("/");
  }, [router]);

  // Not listed, and nothing to show: never heard of (once the list came), or refused with no
  // transcript (a deep link to an id the Mac does not have).
  const gone = info === undefined && ((listed && !status.known) || (status.failed !== null && entries.length === 0));
  const now = nowLine(state, info);

  // Commands go to the Mac through the one client; with none (not paired yet) they are unsent.
  const send = useCallback<Send>(
    (body) => (client ? client.request(body) : Promise.reject(new Refusal("unsent", "Can't reach your Mac"))),
    [client],
  );
  // Only a live session takes commands: not an ended one, one the hub refused, or one that is gone.
  const live = !gone && !status.ended && status.failed === null;
  const phase = state?.phase ?? info?.phase ?? "idle";
  const queued = queueWords(state).count;
  // The home indicator's room is only needed under the composer; the keyboard covers it.
  const [offset, setOffset] = useState(0);
  const root = useRef<View>(null);
  const measure = useCallback(() => root.current?.measureInWindow((_x, y) => setOffset(y)), []);

  return (
    <View ref={root} onLayout={measure} style={[styles.screen, { backgroundColor: theme.bg }]}>
      <KeyboardAvoidingView behavior="padding" keyboardVerticalOffset={offset} style={styles.screen}>
        <Stack.Screen options={{ title }} />
        <HubBanner state={hub} announce={focused} />
        {gone ? (
          <View style={styles.gone}>
            <Text style={[type.body, styles.center, { color: theme.muted }]}>
              This session isn&apos;t running anymore
            </Text>
            <Pressable
              accessibilityRole="button"
              accessibilityLabel="Back to sessions"
              onPress={back}
              style={[styles.button, { borderColor: theme.accent }]}
            >
              <Text style={[type.body, { color: theme.accent }]}>Back to sessions</Text>
            </Pressable>
          </View>
        ) : (
          <>
            {status.ended ? <Notice text="This session ended" /> : null}
            {status.failed !== null ? <Notice text={status.failed} /> : null}
            <TaskStrip state={state} now={now} goal={info?.goal ?? null} open={taskOpen} onOpen={setTaskOpen} />
            <View style={styles.transcript}>
              <FlashList
                ref={list}
                testID="transcript"
                data={entries}
                extraData={open}
                renderItem={renderItem}
                keyExtractor={keyOf}
                getItemType={typeOf}
                maintainVisibleContentPosition={KEEP_POSITION}
                onScroll={follow.onScroll}
                onScrollBeginDrag={follow.onScrollBeginDrag}
                onScrollEndDrag={follow.onScrollEndDrag}
                onMomentumScrollBegin={follow.onMomentumScrollBegin}
                onMomentumScrollEnd={follow.onMomentumScrollEnd}
                onContentSizeChange={follow.onContentSizeChange}
                scrollEventThrottle={32}
                contentContainerStyle={styles.list}
              />
              {follow.below ? (
                <Pressable
                  accessibilityRole="button"
                  accessibilityLabel="New output below"
                  onPress={follow.jump}
                  style={[styles.pill, { backgroundColor: theme.selected, borderColor: theme.accent }]}
                >
                  <Text style={[type.caption, { color: theme.fg }]}>New output below ↓</Text>
                </Pressable>
              ) : null}
            </View>
            {live ? (
              <>
                <ApprovalSheet session={id} approvals={approvals} send={send} />
                <SessionControls session={id} phase={phase} engine={info?.engine ?? "claude"} send={send} />
                <View style={{ paddingBottom: Math.max(insets.bottom, spacing.sm) }}>
                  <Composer session={id} phase={phase} queued={queued} send={send} />
                </View>
              </>
            ) : null}
          </>
        )}
      </KeyboardAvoidingView>
    </View>
  );
}

/** A line over the transcript saying why it is not live. */
function Notice({ text }: { text: string }) {
  const theme = useTheme();
  return (
    <View
      accessible
      accessibilityLabel={text}
      style={[styles.notice, { backgroundColor: theme.panel, borderColor: theme.edge }]}
    >
      <Text style={[type.body, { color: theme.muted }]}>{text}</Text>
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  transcript: { flex: 1 },
  list: { paddingVertical: spacing.sm },
  gone: { flex: 1, alignItems: "center", justifyContent: "center", gap: spacing.lg, padding: spacing.xl },
  center: { textAlign: "center" },
  button: {
    minHeight: 44,
    minWidth: 44,
    paddingHorizontal: spacing.xl,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
    borderWidth: 1,
  },
  notice: {
    paddingVertical: spacing.sm,
    paddingHorizontal: spacing.lg,
    borderRadius: radius.md,
    borderWidth: StyleSheet.hairlineWidth,
    marginHorizontal: spacing.lg,
    marginBottom: spacing.sm,
  },
  pill: {
    position: "absolute",
    bottom: spacing.lg,
    alignSelf: "center",
    minHeight: 44,
    justifyContent: "center",
    paddingHorizontal: spacing.lg,
    borderRadius: radius.lg,
    borderWidth: 1,
  },
});
