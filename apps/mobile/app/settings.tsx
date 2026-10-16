import Constants from "expo-constants";
import { useRouter } from "expo-router";
import { memo, useCallback, useEffect, useRef, useState } from "react";
import { ScrollView, StyleSheet, Switch, Text, TextInput, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { confirmBiometric } from "../src/auth";
import { DevBuildBanner } from "../src/components/DevBuildBanner";
import { useAlive } from "../src/components/send";
import { confirmUnpair } from "../src/components/unpair";
import { openDrawer } from "../src/drawer";
import { useHubController, useMacs, useNotices, type MacInfo } from "../src/hub";
import { MAC_NAME_CHARS, MAX_MACS, cleanMacName, machineWords } from "../src/macs";
import { NO_PROJECT_ID, projectId } from "../src/notifications";
import { NOT_SAVED, loadSettings, saveSettings, type Settings as Saved } from "../src/persist";
import { useHub, useTried } from "../src/store";
import type { PushKinds } from "../src/protocol";
import { radius, spacing, type, useTheme } from "../src/theme";
import { Button, Card } from "../src/ui";

const TURN_OFF_REASON = "Turn off Face ID for allowing";
/** Under "+ Pair another machine" when it is off. */
export const MACS_FULL = "4 Macs is the most";

/** The footer's words, after the app's name and version. */
const FOOTER = "talks to your Macs directly over your tailnet; nothing passes through a relay.";

/** This phone's settings: the Macs it is paired with (each to rename or unpair, and another to
 * pair), what to notify it about, and whether Allow asks for Face ID. */
export default function Settings() {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const router = useRouter();
  const hub = useHubController();
  const alive = useAlive();
  const [saved, setSaved] = useState<Saved | null>(null);
  const [error, setError] = useState(false);
  const checking = useRef(false);
  // Why some Mac did not take the push registration: one line each.
  const notices = useNotices();
  const pushNotice = notices.length > 0 ? notices.join("\n") : null;
  const macs = useMacs();
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

  const rename = useCallback((mac: string, name: string) => hub.rename(mac, name), [hub]);
  // Only that Mac goes: its sessions, views, drafts and toasts leave the phone, and the screens
  // showing them go back to the Inbox. The last one going shows Pair.
  const unpair = useCallback((mac: MacInfo) => confirmUnpair(mac.macName, () => void hub.unpair(mac.id)), [hub]);
  const full = macs.length >= MAX_MACS;
  const addMac = useCallback(() => {
    if (!full) router.push({ pathname: "/pair", params: { add: "1" } });
  }, [full, router]);

  if (saved === null) return <View style={[styles.screen, { backgroundColor: theme.bg }]} />;
  const version = Constants.expoConfig?.version;
  const hasProject = projectId() !== null;
  // With notifications off every switch shows off, whatever kinds are kept.
  const shows = (kind: keyof PushKinds): boolean => hasProject && saved.notifications && saved.pushKinds[kind];
  const stuck = !hasProject || working !== null;
  const pushWords =
    working === "on" ? "Turning on…" : working === "off" ? "Turning off…" : (pushError ?? (hasProject ? pushNotice : NO_PROJECT_ID));
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
          {macs.length === 0 ? <Text style={[type.body, styles.row, { color: theme.muted }]}>Not paired</Text> : null}
          {macs.map((m, i) => (
            <MachineRow key={m.id} mac={m} first={i === 0} onRename={rename} onUnpair={unpair} />
          ))}
          <View style={[styles.row, styles.rule, { borderTopColor: theme.edge }]}>
            <Text style={[type.body, { color: theme.muted }]}>This phone</Text>
            <Text numberOfLines={1} style={[type.body, styles.grow, styles.right, { color: theme.fg }]}>
              {saved.deviceName ?? hub.initialName()}
            </Text>
          </View>
          <View style={[styles.rule, { borderTopColor: theme.edge }]}>
            <Button
              label="+ Pair another machine"
              accessibilityLabel="Pair another machine"
              variant="link"
              disabled={full}
              onPress={addMac}
              style={styles.add}
            />
            {full ? <Text style={[type.caption, styles.words, { color: theme.muted }]}>{MACS_FULL}</Text> : null}
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

interface MachineRowProps {
  mac: MacInfo;
  /** The first row of its card: no rule above it. */
  first: boolean;
  /** Renames Mac `id`; false when the name was not kept. Stable. */
  onRename: (id: string, name: string) => Promise<boolean>;
  /** Asks, then unpairs `mac`. Stable. */
  onUnpair: (mac: MacInfo) => void;
}

/** One paired Mac: its name and its own link's state, its host (muted) under it, and Rename and
 * Unpair. Rename opens a field in place: 1 to 40 characters, cleaned as the Fleet keeps them. */
const MachineRow = memo(function MachineRow({ mac, first, onRename, onUnpair }: MachineRowProps) {
  const theme = useTheme();
  const alive = useAlive();
  const [draft, setDraft] = useState<string | null>(null);
  const [error, setError] = useState(false);
  const saving = useRef(false);
  const hub = useHub(mac.id);
  const tried = useTried(mac.id);
  const online = hub === "ready";
  const name = draft === null ? null : cleanMacName(draft);

  const startRename = useCallback(() => {
    setError(false);
    setDraft(mac.macName);
  }, [mac.macName]);
  const cancel = useCallback(() => setDraft(null), []);
  const save = useCallback(async () => {
    if (saving.current || name === null) return;
    saving.current = true;
    try {
      const kept = await onRename(mac.id, name);
      if (!alive()) return;
      setDraft(null);
      setError(!kept);
    } finally {
      saving.current = false;
    }
  }, [alive, mac.id, name, onRename]);
  const onSave = useCallback(() => void save(), [save]);
  const unpair = useCallback(() => onUnpair(mac), [mac, onUnpair]);

  return (
    <View testID={`machine-${mac.id}`} style={first ? null : [styles.rule, { borderTopColor: theme.edge }]}>
      <View style={styles.row}>
        <Text numberOfLines={1} style={[type.body, styles.grow, { color: online ? theme.fg : theme.muted }]}>
          <Text style={{ color: online ? theme.accent : theme.muted }}>{online ? "●" : "○"}</Text>
          {` ${mac.macName}`}
        </Text>
        <Text style={[type.body, { color: theme.muted }]}>{machineWords(hub, tried)}</Text>
      </View>
      <Text numberOfLines={1} style={[type.label, { color: theme.muted }]}>
        {mac.host}
      </Text>
      {draft === null ? (
        <View style={styles.actions}>
          <Button label="Rename" accessibilityLabel={`Rename ${mac.macName}`} onPress={startRename} />
          <Button label="Unpair" accessibilityLabel={`Unpair ${mac.macName}`} onPress={unpair} />
        </View>
      ) : (
        <View style={styles.rename}>
          <TextInput
            style={[type.body, styles.field, { color: theme.fg, backgroundColor: theme.panel, borderColor: theme.edge }]}
            value={draft}
            onChangeText={setDraft}
            maxLength={MAC_NAME_CHARS}
            autoFocus
            autoCorrect={false}
            accessibilityLabel={`New name for ${mac.macName}`}
          />
          <View style={styles.actions}>
            <Button label="Save" accessibilityLabel="Save the name" variant="primary" disabled={name === null} onPress={onSave} />
            <Button label="Cancel" accessibilityLabel="Cancel renaming" onPress={cancel} />
          </View>
        </View>
      )}
      {error ? (
        <View accessibilityLiveRegion="polite" style={styles.words}>
          <Text style={[type.caption, { color: theme.fg }]}>{NOT_SAVED}</Text>
        </View>
      ) : null}
    </View>
  );
});

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
  actions: { flexDirection: "row", gap: spacing.md, paddingVertical: spacing.sm },
  rename: { gap: spacing.xs, paddingTop: spacing.sm },
  field: { minHeight: 44, borderWidth: 1, borderRadius: radius.card, paddingHorizontal: spacing.md },
  add: { alignSelf: "flex-start", paddingHorizontal: 0 },
  switchRow: { minHeight: 52, flexDirection: "row", alignItems: "center", gap: spacing.md },
  grow: { flex: 1 },
  words: { paddingBottom: spacing.sm },
});
