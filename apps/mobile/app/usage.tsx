import { useLocalSearchParams, useRouter } from "expo-router";
import { memo, useCallback, useMemo, useState } from "react";
import { Pressable, ScrollView, StyleSheet, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { FreshConfirm } from "../src/components/CommandSheet";
import { useDeskCommand } from "../src/components/deskCommand";
import { useSend } from "../src/components/send";
import { engineModel, shortId } from "../src/session";
import { sessionName } from "../src/sessions";
import type { SessionInfo } from "../src/protocol";
import {
  useCatalog,
  useHub,
  useJournals,
  useSessionHead,
  useSessionInfo,
  useSessionStatus,
  useSessionUsage,
  useTurns,
  useUsageRows,
} from "../src/store";
import { radius, spacing, type, useTheme } from "../src/theme";
import { Button, Card } from "../src/ui";

/** Said for USAGE, and for Today, when no session has reported any. */
export const NO_USAGE = "No usage reported yet";
/** Said for JOURNAL before the hub has listed one. */
export const NO_JOURNAL = "None yet";

/** Today's lines: the sessions that report usage, the most recently active first. A session with
 * no recorded activity goes last; ties keep the list's order. */
function byActivity(rows: readonly SessionInfo[]): SessionInfo[] {
  return rows
    .map((row, at) => ({ row, at }))
    .sort((a, b) => (b.row.last_activity ?? -Infinity) - (a.row.last_activity ?? -Infinity) || a.at - b.at)
    .map((x) => x.row);
}

/**
 * Usage, opened from ⋯ in a conversation (`/usage?id=<session>`): this session's engine, latest
 * usage line, turns and newest journal; every session's usage line for today; and Export journal
 * and Fresh context, which are the desk's /export and /new. It shows what the phone holds of the
 * session (the conversation, behind it, is not streaming meanwhile). There is no context bar:
 * the desk reports no context size. Everything the hub sent is drawn as plain text.
 */
export default function Usage() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const params = useLocalSearchParams<{ id?: string | string[] }>();
  const id = typeof params.id === "string" ? params.id : "";
  const info = useSessionInfo(id);
  const head = useSessionHead(id);
  const catalog = useCatalog(id);
  const status = useSessionStatus(id);
  const usage = useSessionUsage(id);
  const turns = useTurns(id);
  const journals = useJournals(id);
  const rows = useUsageRows();
  const hub = useHub();

  const send = useSend();
  const desk = useDeskCommand(id, send);
  const { run, busy } = desk;
  const [asking, setAsking] = useState(false);

  const back = useCallback(() => {
    if (router.canGoBack()) router.back();
    else router.replace("/");
  }, [router]);
  const exportJournal = useCallback(() => void run("export"), [run]);
  const askFresh = useCallback(() => setAsking(true), []);
  const cancelFresh = useCallback(() => setAsking(false), []);
  // The new conversation is the hub's replay of the new journal: back in the conversation it shows.
  const startFresh = useCallback(() => {
    setAsking(false);
    run("new", back);
  }, [run, back]);

  const today = useMemo(() => byActivity(rows), [rows]);
  const s = useMemo(
    () =>
      StyleSheet.create({
        screen: { flex: 1, backgroundColor: theme.bg },
        bar: { height: 48, flexDirection: "row", alignItems: "center", paddingHorizontal: spacing.sm },
        title: { flex: 1, alignItems: "center" },
        heading: { ...type.body, fontWeight: "700", color: theme.fg },
        sub: { ...type.label, color: theme.muted },
        spacer: { width: 44 },
        body: { gap: spacing.xl, paddingHorizontal: 20, paddingVertical: spacing.lg },
        row: { flexDirection: "row", gap: spacing.sm },
        label: { ...type.body, width: 80, color: theme.muted },
        value: { ...type.body, flex: 1, color: theme.fg },
        line: { ...type.body, color: theme.fg },
        lineUsage: { ...type.body, color: theme.muted },
        failure: { ...type.body, color: theme.fg },
        buttons: { gap: spacing.sm, paddingHorizontal: 20, paddingTop: spacing.md },
        action: {
          height: 52,
          minHeight: 44,
          flexDirection: "row",
          alignItems: "center",
          justifyContent: "space-between",
          paddingHorizontal: spacing.lg,
          borderWidth: 1,
          borderRadius: radius.card,
          borderColor: theme.edge,
        },
        off: { opacity: 0.4 },
        actionName: { ...type.body, color: theme.fg },
        actionCmd: { ...type.body, color: theme.muted },
      }),
    [theme],
  );

  const known = id !== "";
  // The Mac must be reachable, and the session listed and taking commands.
  const live = info !== undefined && !status.ended && status.failed === null;
  const off = hub !== "ready" || !live || busy || asking;
  const journal = journals[journals.length - 1];

  return (
    <View style={[s.screen, { paddingTop: insets.top }]}>
      <View style={s.bar}>
        <Button label="‹" accessibilityLabel="Back" variant="bare" onPress={back} />
        <View style={s.title}>
          <Text accessibilityRole="header" style={s.heading}>
            Usage
          </Text>
          {known ? (
            <Text numberOfLines={1} style={s.sub}>
              {`${sessionName(head.workspace)} · ${shortId(id)}`}
            </Text>
          ) : null}
        </View>
        <View style={s.spacer} />
      </View>
      {known ? (
        <>
          <ScrollView contentContainerStyle={s.body}>
            <Card title="This session" testID="this-session">
              <View style={s.row}>
                <Text style={s.label}>ENGINE</Text>
                <Text style={s.value}>{engineModel(catalog, head)}</Text>
              </View>
              <View style={s.row}>
                <Text style={s.label}>USAGE</Text>
                <Text style={s.value}>{usage ?? NO_USAGE}</Text>
              </View>
              <View style={s.row}>
                <Text style={s.label}>TURNS</Text>
                <Text style={s.value}>{String(turns)}</Text>
              </View>
              <View style={s.row}>
                <Text style={s.label}>JOURNAL</Text>
                <Text style={s.value}>{journal ?? NO_JOURNAL}</Text>
              </View>
            </Card>
            <Card title="Today" testID="today">
              {today.length === 0 ? <Text style={s.lineUsage}>{NO_USAGE}</Text> : null}
              {today.map((row) => (
                <UsageLine key={row.id} name={sessionName(row.workspace)} usage={row.usage ?? ""} nameStyle={s.line} usageStyle={s.lineUsage} />
              ))}
            </Card>
          </ScrollView>
          {desk.failed ? (
            <View accessibilityLiveRegion="polite" style={s.buttons}>
              <Text style={s.failure}>{desk.failed.text}</Text>
            </View>
          ) : null}
          <View style={[s.buttons, { paddingBottom: Math.max(insets.bottom, spacing.xl) }]}>
            <ActionRow name="Export journal" command="/export" off={off} onPress={exportJournal} nameStyle={s.actionName} cmdStyle={s.actionCmd} boxStyle={s.action} offStyle={s.off} />
            <ActionRow name="Fresh context" command="/new" off={off} onPress={askFresh} nameStyle={s.actionName} cmdStyle={s.actionCmd} boxStyle={s.action} offStyle={s.off} />
          </View>
          {asking ? <FreshConfirm offline={hub !== "ready"} onStart={startFresh} onCancel={cancelFresh} /> : null}
        </>
      ) : (
        <Text style={[s.body, s.lineUsage]}>No session</Text>
      )}
    </View>
  );
}

type TextLook = ReturnType<typeof StyleSheet.create>[string];

/** One of Today's lines: "<name> <usage>". */
const UsageLine = memo(function UsageLine({
  name,
  usage,
  nameStyle,
  usageStyle,
}: {
  name: string;
  usage: string;
  nameStyle: TextLook;
  usageStyle: TextLook;
}) {
  return (
    <Text testID="today-line" style={nameStyle}>
      {name}
      <Text style={usageStyle}>{` ${usage}`}</Text>
    </Text>
  );
});

/** A full-width action: its name, and the desk command it runs, muted. */
const ActionRow = memo(function ActionRow({
  name,
  command,
  off,
  onPress,
  nameStyle,
  cmdStyle,
  boxStyle,
  offStyle,
}: {
  name: string;
  command: string;
  off: boolean;
  onPress: () => void;
  nameStyle: TextLook;
  cmdStyle: TextLook;
  boxStyle: TextLook;
  offStyle: TextLook;
}) {
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={`${name} ${command}`}
      accessibilityState={{ disabled: off }}
      disabled={off}
      onPress={onPress}
      style={off ? [boxStyle, offStyle] : boxStyle}
    >
      <Text style={nameStyle}>{name}</Text>
      <Text style={cmdStyle}>{command}</Text>
    </Pressable>
  );
});
