import Constants from "expo-constants";
import { memo, useCallback, useEffect, useRef, useState } from "react";
import { Alert, ScrollView, StyleSheet, Switch, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { confirmBiometric } from "../src/auth";
import { DevBuildBanner } from "../src/components/DevBuildBanner";
import { useAlive } from "../src/components/send";
import { openDrawer } from "../src/drawer";
import { useFleet, useHubController } from "../src/hub";
import { macName } from "../src/inbox";
import { NO_PROJECT_ID, projectId } from "../src/notifications";
import { NOT_SAVED, loadSettings, saveSettings, type Settings as Saved } from "../src/persist";
import type { PushKinds } from "../src/protocol";
import { useHub } from "../src/store";
import { spacing, type, useTheme } from "../src/theme";
import { Button, Card } from "../src/ui";

const TURN_OFF_REASON = "Turn off Face ID for allowing";
export const UNPAIR_TITLE = "Unpair this phone?";
export const UNPAIR_BODY =
  "You'll need a new pairing code from the Mac. Notifications to this phone stop. The Mac keeps listing this phone until you run `octet hub revoke <name>` there.";

/** The footer's words, after the app's name and version. */
const FOOTER = "talks to your Macs directly over your tailnet; nothing passes through a relay.";

/** This phone's settings: the Mac it is paired with (and Unpair), what to notify it about, and
 * whether Allow asks for Face ID. */
export default function Settings() {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const hub = useHubController();
  const online = useHub() === "ready";
  const alive = useAlive();
  const [saved, setSaved] = useState<Saved | null>(null);
  const [error, setError] = useState(false);
  const checking = useRef(false);
  const { pushNotice, macs } = useFleet();
  const [pushError, setPushError] = useState<string | null>(null);
  const pushing = useRef(false);
  /** The switch is being worked on: "on" or "off"; null when idle. */
  const [working, setWorking] = useState<"on" | "off" | null>(null);

  useEffect(() => {
    let live = true;
    void loadSettings().then((s) => live && setSaved(s));
    return () => {
      live = false;
    };
  }, []);

  const change = useCallback(
    async (on: boolean) => {
      if (checking.current) return;
      checking.current = true;
      try {
        setError(false);
        if (!on) {
          // Turning it off is the one change worth proving it is the owner. A phone with no
          // biometrics cannot answer, and Face ID cannot protect anything there: it may be turned off.
          const checked = await confirmBiometric(TURN_OFF_REASON);
          if (!alive() || checked === "failed") return;
        }
        const kept = await saveSettings({ faceId: on });
        if (!alive()) return;
        if (kept) setSaved((s) => (s ? { ...s, faceId: on } : s));
        else setError(true);
      } finally {
        checking.current = false;
      }
    },
    [alive],
  );

  const flip = useCallback(
    (on: boolean) => {
      void change(on);
    },
    [change],
  );

  // One kind of push on or off: the Mac is told first, and the switch moves on its ack.
  const changeKind = useCallback(
    async (kind: keyof PushKinds, on: boolean) => {
      if (pushing.current) return;
      pushing.current = true;
      try {
        setPushError(null);
        setWorking(on ? "on" : "off");
        const result = await hub.setPushKind(kind, on);
        if (!alive()) return;
        if (result.ok) {
          const now = await loadSettings();
          if (alive()) setSaved((s) => (s ? { ...s, notifications: now.notifications, pushKinds: now.pushKinds } : s));
        } else setPushError(result.message);
      } finally {
        pushing.current = false;
        if (alive()) setWorking(null);
      }
    },
    [alive, hub],
  );

  const flipApprovals = useCallback((on: boolean) => void changeKind("approvals", on), [changeKind]);
  const flipTurns = useCallback((on: boolean) => void changeKind("turns", on), [changeKind]);
  const flipErrors = useCallback((on: boolean) => void changeKind("errors", on), [changeKind]);

  const openMenu = useCallback(() => openDrawer(), []);

  const unpair = useCallback(() => {
    Alert.alert(UNPAIR_TITLE, UNPAIR_BODY, [
      { text: "Cancel", style: "cancel" },
      { text: "Unpair", style: "destructive", onPress: () => void hub.unpair() },
    ]);
  }, [hub]);

  if (saved === null) return <View style={[styles.screen, { backgroundColor: theme.bg }]} />;
  const version = Constants.expoConfig?.version;
  const hasProject = projectId() !== null;
  // With notifications off every switch shows off, whatever kinds are kept.
  const shows = (kind: keyof PushKinds): boolean => hasProject && saved.notifications && saved.pushKinds[kind];
  const stuck = !hasProject || working !== null;
  const pushWords =
    working === "on" ? "Turning on…" : working === "off" ? "Turning off…" : (pushError ?? (hasProject ? pushNotice : NO_PROJECT_ID));
  // The first Mac paired, until Machines lists each (Phase D, Task 4).
  const first = macs[0];
  const mac = first?.macName ?? macName(null);
  return (
    <View style={[styles.screen, { backgroundColor: theme.bg, paddingTop: insets.top }]}>
      <View style={styles.header}>
        <Button label="☰" accessibilityLabel="Menu" variant="bare" onPress={openMenu} />
        <Text accessibilityRole="header" style={[type.body, styles.title, { color: theme.fg }]}>
          Settings
        </Text>
        <View style={styles.spacer} />
      </View>
      <DevBuildBanner inset />
      <ScrollView contentContainerStyle={[styles.content, { paddingBottom: Math.max(insets.bottom, spacing.lg) }]}>
        <Card title="Machines">
          <View style={styles.row}>
            <Text numberOfLines={1} style={[type.body, styles.grow, { color: online ? theme.fg : theme.muted }]}>
              <Text style={{ color: online ? theme.accent : theme.muted }}>{online ? "●" : "○"}</Text>
              {` ${mac}`}
            </Text>
            <Text style={[type.body, { color: theme.muted }]}>{online ? "online" : "offline"}</Text>
          </View>
          <Text style={[type.label, { color: theme.muted }]}>{first === undefined ? "Not paired" : first.host}</Text>
          <View style={[styles.row, styles.rule, { borderTopColor: theme.edge }]}>
            <Text style={[type.body, { color: theme.muted }]}>This phone</Text>
            <Text numberOfLines={1} style={[type.body, styles.grow, styles.right, { color: theme.fg }]}>
              {saved.deviceName ?? hub.initialName()}
            </Text>
          </View>
          <View style={styles.action}>
            <Button label="Unpair" onPress={unpair} />
          </View>
        </Card>

        <Card title="Notify me when">
          <SwitchRow label="An action needs approval" value={shows("approvals")} disabled={stuck} onValueChange={flipApprovals} />
          <SwitchRow label="A turn finishes" value={shows("turns")} disabled={stuck} first={false} onValueChange={flipTurns} />
          <SwitchRow label="A session hits an error" value={shows("errors")} disabled={stuck} first={false} onValueChange={flipErrors} />
          {pushWords !== null ? (
            <View accessibilityLiveRegion="polite" style={styles.words}>
              <Text style={[type.caption, { color: theme.fg }]}>{pushWords}</Text>
            </View>
          ) : null}
        </Card>

        <Card title="Face ID to allow">
          <SwitchRow label="Ask before allowing" accessibilityLabel="Face ID to allow" value={saved.faceId} onValueChange={flip} />
          <Text style={[type.caption, styles.words, { color: theme.muted }]}>
            {"Allowing an agent's request asks for Face ID. Full access always asks."}
          </Text>
          {error ? (
            <View accessibilityLiveRegion="polite" style={styles.words}>
              <Text style={[type.caption, { color: theme.fg }]}>{NOT_SAVED}</Text>
            </View>
          ) : null}
        </Card>

        <Text style={[type.caption, { color: theme.muted }]}>{version ? `Octet v${version} · ${FOOTER}` : `Octet · ${FOOTER}`}</Text>
      </ScrollView>
    </View>
  );
}

interface SwitchRowProps {
  label: string;
  /** What a screen reader says for the switch; the label when not given. */
  accessibilityLabel?: string;
  value: boolean;
  disabled?: boolean;
  /** Not the first row of its card: a rule above it. */
  first?: boolean;
  onValueChange?: (on: boolean) => void;
}

/** One row of switches: the label on the left, the switch on the right. */
const SwitchRow = memo(function SwitchRow({ label, accessibilityLabel, value, disabled = false, first = true, onValueChange }: SwitchRowProps) {
  const theme = useTheme();
  return (
    <View style={[styles.switchRow, first ? null : [styles.rule, { borderTopColor: theme.edge }]]}>
      <View style={styles.grow}>
        <Text style={[type.body, { color: disabled ? theme.muted : theme.fg }]}>{label}</Text>
      </View>
      <Switch
        accessibilityRole="switch"
        accessibilityLabel={accessibilityLabel ?? label}
        value={value}
        disabled={disabled}
        onValueChange={onValueChange}
        hitSlop={HIT}
        trackColor={{ false: theme.edge, true: theme.accent }}
      />
    </View>
  );
});

const HIT = { top: 8, bottom: 8, left: 8, right: 8 } as const;

const styles = StyleSheet.create({
  screen: { flex: 1 },
  header: { flexDirection: "row", alignItems: "center", gap: spacing.xs, paddingHorizontal: spacing.sm, paddingBottom: spacing.md },
  title: { flex: 1, textAlign: "center", fontWeight: "700" },
  spacer: { width: 44 },
  content: { paddingHorizontal: 20, paddingTop: spacing.lg, gap: spacing.xl },
  row: { minHeight: 44, flexDirection: "row", alignItems: "center", justifyContent: "space-between", gap: spacing.md },
  rule: { borderTopWidth: 1 },
  right: { textAlign: "right" },
  action: { alignItems: "flex-start", paddingVertical: spacing.sm },
  switchRow: { minHeight: 52, flexDirection: "row", alignItems: "center", gap: spacing.md },
  grow: { flex: 1 },
  words: { paddingBottom: spacing.sm },
});
