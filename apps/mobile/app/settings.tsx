import Constants from "expo-constants";
import { useCallback, useEffect, useRef, useState } from "react";
import { Alert, Pressable, StyleSheet, Switch, Text, View } from "react-native";

import { confirmBiometric } from "../src/auth";
import { hubHost } from "../src/client/link";
import { useAlive } from "../src/components/send";
import { useHubController, useHubSnapshot } from "../src/hub";
import { NO_PROJECT_ID, projectId } from "../src/notifications";
import { NOT_SAVED, loadSettings, saveSettings, type Settings as Saved } from "../src/persist";
import { radius, spacing, type, useTheme } from "../src/theme";

const TURN_OFF_REASON = "Turn off Face ID for allowing";
export const UNPAIR_TITLE = "Unpair this phone?";
export const UNPAIR_BODY =
  "You'll need a new pairing code from the Mac. The Mac keeps listing this phone until you run `octet hub revoke <name>` there.";

/** This phone's settings: where it is paired, whether Allow asks for Face ID, and Unpair. */
export default function Settings() {
  const theme = useTheme();
  const hub = useHubController();
  const alive = useAlive();
  const [saved, setSaved] = useState<Saved | null>(null);
  const [error, setError] = useState(false);
  const checking = useRef(false);
  const { pushNotice } = useHubSnapshot();
  const [pushError, setPushError] = useState<string | null>(null);
  const pushing = useRef(false);

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

  const changePush = useCallback(
    async (on: boolean) => {
      if (pushing.current) return;
      pushing.current = true;
      try {
        setPushError(null);
        const result = await (on ? hub.turnOnNotifications() : hub.turnOffNotifications());
        if (!alive()) return;
        if (result.ok) setSaved((s) => (s ? { ...s, notifications: on } : s));
        else setPushError(result.message);
      } finally {
        pushing.current = false;
      }
    },
    [alive, hub],
  );

  const flipPush = useCallback(
    (on: boolean) => {
      void changePush(on);
    },
    [changePush],
  );

  const unpair = useCallback(() => {
    Alert.alert(UNPAIR_TITLE, UNPAIR_BODY, [
      { text: "Cancel", style: "cancel" },
      { text: "Unpair", style: "destructive", onPress: () => void hub.unpair() },
    ]);
  }, [hub]);

  if (saved === null) return <View style={[styles.screen, { backgroundColor: theme.bg }]} />;
  const version = Constants.expoConfig?.version;
  const hasProject = projectId() !== null;
  const pushWords = pushError ?? (hasProject ? pushNotice : NO_PROJECT_ID);
  return (
    <View style={[styles.screen, { backgroundColor: theme.bg }]}>
      <View style={[styles.card, { backgroundColor: theme.panel, borderColor: theme.edge }]}>
        <Line label="Mac" value={saved.hubUrl === null ? "Not paired" : hubHost(saved.hubUrl)} />
        <Line label="This phone" value={saved.deviceName ?? hub.initialName()} />
      </View>
      <View style={[styles.card, { backgroundColor: theme.panel, borderColor: theme.edge }]}>
        <View style={styles.switchRow}>
          <Text style={[type.body, styles.grow, { color: theme.fg }]}>Face ID to allow</Text>
          <Switch
            accessibilityRole="switch"
            accessibilityLabel="Face ID to allow"
            value={saved.faceId}
            onValueChange={flip}
            hitSlop={HIT}
            trackColor={{ false: theme.edge, true: theme.accent }}
          />
        </View>
        <Text style={[type.caption, { color: theme.muted }]}>
          {"Allowing an agent's request asks for Face ID. Full access always asks."}
        </Text>
        {error ? (
          <View accessibilityLiveRegion="polite">
            <Text style={[type.caption, { color: theme.fg }]}>{NOT_SAVED}</Text>
          </View>
        ) : null}
      </View>
      <View style={[styles.card, { backgroundColor: theme.panel, borderColor: theme.edge }]}>
        <View style={styles.switchRow}>
          <Text style={[type.body, styles.grow, { color: theme.fg }]}>Notifications</Text>
          <Switch
            accessibilityRole="switch"
            accessibilityLabel="Notifications"
            value={hasProject && saved.notifications}
            disabled={!hasProject}
            onValueChange={flipPush}
            hitSlop={HIT}
            trackColor={{ false: theme.edge, true: theme.accent }}
          />
        </View>
        <Text style={[type.caption, { color: theme.muted }]}>
          Get a notification when an agent needs your approval or finishes a turn, while the app is closed.
        </Text>
        {pushWords !== null ? (
          <View accessibilityLiveRegion="polite">
            <Text style={[type.caption, { color: theme.fg }]}>{pushWords}</Text>
          </View>
        ) : null}
      </View>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel="Unpair"
        onPress={unpair}
        style={[styles.button, { borderColor: theme.accent }]}
      >
        <Text style={[type.body, { color: theme.accent }]}>Unpair</Text>
      </Pressable>
      {version ? <Text style={[type.caption, styles.version, { color: theme.muted }]}>{`Version ${version}`}</Text> : null}
    </View>
  );
}

function Line({ label, value }: { label: string; value: string }) {
  const theme = useTheme();
  return (
    <View style={styles.line}>
      <Text style={[type.caption, { color: theme.muted }]}>{label}</Text>
      <Text style={[type.body, { color: theme.fg }]}>{value}</Text>
    </View>
  );
}

const HIT = { top: 8, bottom: 8, left: 8, right: 8 } as const;

const styles = StyleSheet.create({
  screen: { flex: 1, padding: spacing.lg, gap: spacing.lg },
  card: { padding: spacing.lg, gap: spacing.md, borderRadius: radius.md, borderWidth: StyleSheet.hairlineWidth },
  line: { gap: 2 },
  switchRow: { flexDirection: "row", alignItems: "center", gap: spacing.md, minHeight: 44 },
  grow: { flex: 1 },
  button: {
    minHeight: 44,
    minWidth: 44,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
    borderWidth: 1,
  },
  version: { textAlign: "center" },
});
