import { CameraView, useCameraPermissions } from "expo-camera";
import { useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { KeyboardAvoidingView, Platform, ScrollView, StyleSheet, Text, TextInput, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { hubHost, pairLinkProblem, parsePairLink } from "../src/client/link";
import { DevBuildBanner } from "../src/components/DevBuildBanner";
import { useAlive } from "../src/components/send";
import { NOT_A_LINK, useHubController, useHubSnapshot } from "../src/hub";
import { macName } from "../src/inbox";
import { Mascot } from "../src/mascot/Mascot";
import { loadSettings } from "../src/persist";
import { radius, spacing, type, useTheme } from "../src/theme";
import { showToast } from "../src/toast";
import { Button } from "../src/ui";

const QR_ONLY = { barcodeTypes: ["qr" as const] };
/** The Mac refuses a name over 64 characters. */
const NAME_CHARS = 64;
/** The scan frame, and its corner brackets, in points. */
const FRAME = 240;
const CORNER = 36;
const CORNER_LINE = 3;

/** Pairing: scan the QR code the Mac shows, or enter its link. The hub's refusals and every
 * failure are shown as plain text. */
export default function Pair() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const hub = useHubController();
  const { notice, offer } = useHubSnapshot();
  const [permission, requestPermission] = useCameraPermissions();
  const [link, setLink] = useState("");
  const [name, setName] = useState(() => hub.initialName());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // "Enter code instead" was pressed, or a scan or paste filled the link: the paste and name fields show.
  const [entering, setEntering] = useState(false);
  // The Mac this phone was paired with before, when it pairs again.
  const [before, setBefore] = useState<string | null>(null);
  const attempting = useRef(false);
  const lastScan = useRef<string | null>(null);

  useEffect(() => {
    if (permission && !permission.granted && permission.canAskAgain) void requestPermission();
    // Asked once, when the screen shows and the permission is still undetermined.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [permission?.status]);

  useEffect(() => {
    let live = true;
    void loadSettings().then((s) => live && setBefore(s.hubUrl === null ? null : macName(s.hubUrl)));
    return () => {
      live = false;
    };
  }, []);

  const parsed = useMemo(() => parsePairLink(link), [link]);
  const pasted = link.trim() !== "";
  const shownError = error ?? (pasted && parsed === null ? (pairLinkProblem(link) ?? NOT_A_LINK) : null);

  const pair = useCallback(async () => {
    if (attempting.current || parsed === null) return;
    attempting.current = true;
    setBusy(true);
    setError(null);
    try {
      const result = await hub.pair(link, name);
      if (result.ok) {
        showToast(`Paired with ${macName(parsed.url)}`);
        // With a notifications step to show, Pair stays up until the user answers it.
        if (!hub.snapshot().offer) router.replace("/");
        return;
      }
      setError(result.message);
      if (result.clear) {
        // The secret is spent or refused: forget it, so that the camera can offer the next code.
        setLink("");
        lastScan.current = null;
      }
    } finally {
      attempting.current = false;
      setBusy(false);
    }
  }, [hub, link, name, parsed, router]);
  // Not the promise itself: a press handler's returned promise would be awaited by the caller.
  const onPair = useCallback(() => void pair(), [pair]);

  const onScan = useCallback(({ data }: { data: string }) => {
    // The camera reports the same code many times a second: one look at each. A code only
    // fills the field; the user confirms the Mac it names, and pairing waits for the tap.
    if (attempting.current || data === lastScan.current) return;
    lastScan.current = data;
    const problem = pairLinkProblem(data);
    if (problem !== null) {
      setError(problem);
      return;
    }
    setError(null);
    setEntering(true);
    setLink(data);
  }, []);

  const onLink = useCallback((text: string) => {
    lastScan.current = null;
    setError(null);
    setEntering(true);
    setLink(text);
  }, []);

  // The scan stays remembered: the camera still sees the code that was just cancelled.
  const onCancel = useCallback(() => {
    setError(null);
    setLink("");
  }, []);

  const onEnter = useCallback(() => setEntering(true), []);

  if (offer) return <NotificationsStep />;

  const cameraOn = permission?.granted === true;
  const cameraOff = permission !== null && !permission.granted && !permission.canAskAgain;
  // The fields show once asked for (or once a scan or a paste filled the link), and stay; with no camera they are all there is.
  const fields = entering || cameraOff;
  const field = [styles.field, { color: theme.fg, backgroundColor: theme.panel, borderColor: theme.edge }];
  const corner = { borderColor: theme.accent };

  return (
    <KeyboardAvoidingView style={{ flex: 1, backgroundColor: theme.panel }} behavior={Platform.OS === "ios" ? "padding" : undefined}>
      <ScrollView
        contentContainerStyle={[styles.content, { paddingTop: insets.top }]}
        keyboardShouldPersistTaps="handled"
        showsVerticalScrollIndicator={false}
      >
        <DevBuildBanner inset />
        <Text accessibilityRole="header" style={[type.body, styles.heading, { color: theme.fg }]}>
          {"Scan the code from "}
          <Text style={styles.bold}>octet pair</Text>
        </Text>

        <View style={styles.scan}>
          <View testID="scan-frame" style={styles.frame}>
            {cameraOn && (
              <CameraView
                style={StyleSheet.absoluteFill}
                facing="back"
                barcodeScannerSettings={QR_ONLY}
                onBarcodeScanned={busy ? undefined : onScan}
                accessibilityLabel="Camera for the pairing code"
              />
            )}
            {cameraOff && (
              <Text style={[type.body, styles.off, { color: theme.muted }]}>Camera access is off — paste the link instead</Text>
            )}
            <View pointerEvents="none" style={StyleSheet.absoluteFill}>
              <View testID="scan-corner-tl" style={[styles.corner, styles.tl, corner]} />
              <View testID="scan-corner-tr" style={[styles.corner, styles.tr, corner]} />
              <View testID="scan-corner-bl" style={[styles.corner, styles.bl, corner]} />
              <View testID="scan-corner-br" style={[styles.corner, styles.br, corner]} />
            </View>
          </View>
        </View>

        <View
          style={[
            styles.sheet,
            { backgroundColor: theme.bg, borderColor: theme.edge, paddingBottom: Math.max(insets.bottom, spacing.lg) + spacing.lg },
          ]}
        >
          <Text style={[type.label, { color: theme.muted }]}>PAIRED</Text>
          <View style={styles.mac}>
            <Text style={[type.body, { color: before === null ? theme.muted : theme.fg }]}>{before === null ? "No Mac yet" : `○ ${before}`}</Text>
          </View>

          {notice !== null && (
            <Text accessibilityRole="alert" style={[type.body, { color: theme.amber }]}>
              {notice}
            </Text>
          )}

          {fields ? (
            <>
              <Text style={[type.label, { color: theme.muted }]}>Paste link</Text>
              <TextInput
                style={field}
                value={link}
                onChangeText={onLink}
                placeholder="https://…#pair=…"
                placeholderTextColor={theme.muted}
                autoCapitalize="none"
                autoCorrect={false}
                spellCheck={false}
                keyboardType="url"
                accessibilityLabel="Pairing link"
              />

              <Text style={[type.label, { color: theme.muted }]}>Device name</Text>
              <TextInput
                style={field}
                value={name}
                onChangeText={setName}
                maxLength={NAME_CHARS}
                autoCorrect={false}
                accessibilityLabel="Device name"
              />

              {parsed !== null && <Text style={[type.title, { color: theme.fg }]}>{`Pair with ${hubHost(parsed.url)}?`}</Text>}
            </>
          ) : null}

          {shownError !== null && (
            <Text accessibilityRole="alert" style={[type.body, { color: theme.amber }]}>
              {shownError}
            </Text>
          )}

          {fields ? (
            <>
              <Button
                label={busy ? "Pairing…" : "Pair"}
                accessibilityLabel="Pair"
                variant="primary"
                onPress={onPair}
                disabled={busy || parsed === null}
                busy={busy}
              />
              {pasted && !busy && <Button label="Cancel" onPress={onCancel} />}
            </>
          ) : (
            <Button label="Enter code instead" variant="link" onPress={onEnter} style={styles.enter} />
          )}
        </View>
      </ScrollView>
    </KeyboardAvoidingView>
  );
}

/** After a pairing, once: get a notification when an agent needs approval or finishes a turn.
 * Skippable; Settings has the same switch. */
function NotificationsStep() {
  const theme = useTheme();
  const router = useRouter();
  const hub = useHubController();
  const alive = useAlive();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const working = useRef(false);

  const leave = useCallback(() => {
    hub.finishOffer();
    router.replace("/");
  }, [hub, router]);

  const turnOn = useCallback(async () => {
    if (working.current) return;
    working.current = true;
    setBusy(true);
    setError(null);
    try {
      const result = await hub.turnOnNotifications();
      if (!alive()) return;
      if (result.ok) leave();
      else setError(result.message);
    } finally {
      working.current = false;
      if (alive()) setBusy(false);
    }
  }, [alive, hub, leave]);
  const onTurnOn = useCallback(() => void turnOn(), [turnOn]);

  return (
    <View style={[styles.step, { flex: 1, backgroundColor: theme.bg }]}>
      <Mascot pose="idle" size={118} />
      <Text accessibilityRole="header" style={[type.display, { color: theme.fg }]}>
        Paired
      </Text>
      <Text style={[type.body, { color: theme.muted }]}>
        Get a notification when an agent needs your approval or finishes a turn, while the app is closed.
      </Text>
      {error !== null && (
        <Text accessibilityRole="alert" style={[type.body, { color: theme.amber }]}>
          {error}
        </Text>
      )}
      <Button
        label={busy ? "Turning on…" : "Turn on notifications"}
        accessibilityLabel="Turn on notifications"
        variant="primary"
        onPress={onTurnOn}
        disabled={busy}
        busy={busy}
      />
      <Button label="Not now" onPress={leave} />
    </View>
  );
}

const styles = StyleSheet.create({
  content: { flexGrow: 1 },
  heading: { paddingHorizontal: spacing.xl, paddingVertical: spacing.sm },
  bold: { fontWeight: "700" },
  scan: { flex: 1, minHeight: FRAME + 2 * spacing.xl, alignItems: "center", justifyContent: "center" },
  frame: { width: FRAME, height: FRAME, borderRadius: radius.card, overflow: "hidden", alignItems: "center", justifyContent: "center" },
  off: { textAlign: "center", paddingHorizontal: spacing.xl },
  corner: { position: "absolute", width: CORNER, height: CORNER },
  tl: { left: 0, top: 0, borderLeftWidth: CORNER_LINE, borderTopWidth: CORNER_LINE, borderTopLeftRadius: radius.card },
  tr: { right: 0, top: 0, borderRightWidth: CORNER_LINE, borderTopWidth: CORNER_LINE, borderTopRightRadius: radius.card },
  bl: { left: 0, bottom: 0, borderLeftWidth: CORNER_LINE, borderBottomWidth: CORNER_LINE, borderBottomLeftRadius: radius.card },
  br: { right: 0, bottom: 0, borderRightWidth: CORNER_LINE, borderBottomWidth: CORNER_LINE, borderBottomRightRadius: radius.card },
  sheet: {
    borderTopWidth: 1,
    borderTopLeftRadius: radius.card,
    borderTopRightRadius: radius.card,
    paddingHorizontal: spacing.xl,
    paddingTop: spacing.lg,
    gap: spacing.sm,
  },
  mac: { minHeight: 44, justifyContent: "center" },
  enter: { alignSelf: "flex-start", paddingHorizontal: 0 },
  field: { minHeight: 44, borderWidth: 1, borderRadius: radius.card, paddingHorizontal: spacing.md, fontSize: 16 },
  step: { padding: spacing.lg, gap: spacing.md, paddingTop: spacing.xxl },
});
