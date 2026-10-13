import { CameraView, useCameraPermissions } from "expo-camera";
import { useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { KeyboardAvoidingView, Platform, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from "react-native";

import { hubHost, parsePairLink } from "../src/client/link";
import { DevBuildBanner } from "../src/components/DevBuildBanner";
import { useAlive } from "../src/components/send";
import { NOT_A_LINK, useHubController, useHubSnapshot } from "../src/hub";
import { Mascot } from "../src/mascot/Mascot";
import { radius, spacing, type, useTheme } from "../src/theme";

const QR_ONLY = { barcodeTypes: ["qr" as const] };
/** The Mac refuses a name over 64 characters. */
const NAME_CHARS = 64;

/** Pairing: scan the QR code the Mac shows, or paste its link. The hub's refusals and every
 * failure are shown as plain text. */
export default function Pair() {
  const theme = useTheme();
  const router = useRouter();
  const hub = useHubController();
  const { notice, offer } = useHubSnapshot();
  const [permission, requestPermission] = useCameraPermissions();
  const [link, setLink] = useState("");
  const [name, setName] = useState(() => hub.initialName());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const attempting = useRef(false);
  const lastScan = useRef<string | null>(null);

  useEffect(() => {
    if (permission && !permission.granted && permission.canAskAgain) void requestPermission();
    // Asked once, when the screen shows and the permission is still undetermined.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [permission?.status]);

  const parsed = useMemo(() => parsePairLink(link), [link]);
  const pasted = link.trim() !== "";
  const shownError = error ?? (pasted && parsed === null ? NOT_A_LINK : null);

  const pair = useCallback(async () => {
    if (attempting.current || parsed === null) return;
    attempting.current = true;
    setBusy(true);
    setError(null);
    try {
      const result = await hub.pair(link, name);
      if (result.ok) {
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
    if (parsePairLink(data) === null) {
      setError(NOT_A_LINK);
      return;
    }
    setError(null);
    setLink(data);
  }, []);

  const onLink = useCallback((text: string) => {
    lastScan.current = null;
    setError(null);
    setLink(text);
  }, []);

  // The scan stays remembered: the camera still sees the code that was just cancelled.
  const onCancel = useCallback(() => {
    setError(null);
    setLink("");
  }, []);

  if (offer) return <NotificationsStep />;

  const cameraOn = permission?.granted === true;
  const cameraOff = permission !== null && !permission.granted && !permission.canAskAgain;
  const field = [styles.field, { color: theme.fg, backgroundColor: theme.panel, borderColor: theme.edge }];

  return (
    <KeyboardAvoidingView style={{ flex: 1, backgroundColor: theme.bg }} behavior={Platform.OS === "ios" ? "padding" : undefined}>
      <ScrollView contentContainerStyle={styles.content} keyboardShouldPersistTaps="handled">
        <DevBuildBanner />
        <Mascot pose="idle" size={118} />
        <Text accessibilityRole="header" style={[type.display, { color: theme.fg }]}>
          Pair with your Mac
        </Text>
        <Text style={[type.body, { color: theme.muted }]}>
          On your Mac, run /remote-control pair, then scan the code or paste its link.
        </Text>

        {notice !== null && (
          <Text accessibilityRole="alert" style={[type.body, { color: theme.amber }]}>
            {notice}
          </Text>
        )}

        {cameraOn && (
          <View style={[styles.camera, { borderColor: theme.edge }]}>
            <CameraView
              style={StyleSheet.absoluteFill}
              facing="back"
              barcodeScannerSettings={QR_ONLY}
              onBarcodeScanned={busy ? undefined : onScan}
              accessibilityLabel="Camera for the pairing code"
            />
          </View>
        )}
        {cameraOff && (
          <Text style={[type.body, { color: theme.muted }]}>Camera access is off — paste the link instead</Text>
        )}

        <Text style={[type.caption, { color: theme.muted }]}>Paste link</Text>
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

        <Text style={[type.caption, { color: theme.muted }]}>Device name</Text>
        <TextInput
          style={field}
          value={name}
          onChangeText={setName}
          maxLength={NAME_CHARS}
          autoCorrect={false}
          accessibilityLabel="Device name"
        />

        {parsed !== null && (
          <Text style={[type.title, { color: theme.fg }]}>{`Pair with ${hubHost(parsed.url)}?`}</Text>
        )}

        {shownError !== null && (
          <Text accessibilityRole="alert" style={[type.body, { color: theme.accent }]}>
            {shownError}
          </Text>
        )}

        <Pressable
          onPress={onPair}
          disabled={busy || parsed === null}
          accessibilityRole="button"
          accessibilityLabel="Pair"
          accessibilityState={{ disabled: busy || parsed === null, busy }}
          style={[styles.button, { backgroundColor: theme.accent, opacity: busy || parsed === null ? 0.5 : 1 }]}
        >
          <Text style={[type.body, { color: theme.bg, fontWeight: "600" }]}>{busy ? "Pairing…" : "Pair"}</Text>
        </Pressable>
        {pasted && !busy && (
          <Pressable
            onPress={onCancel}
            accessibilityRole="button"
            accessibilityLabel="Cancel"
            style={[styles.button, { borderColor: theme.edge, borderWidth: 1 }]}
          >
            <Text style={[type.body, { color: theme.fg }]}>Cancel</Text>
          </Pressable>
        )}
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
    <View style={[styles.content, { flex: 1, backgroundColor: theme.bg }]}>
      <Mascot pose="idle" size={118} />
      <Text accessibilityRole="header" style={[type.display, { color: theme.fg }]}>
        Paired
      </Text>
      <Text style={[type.body, { color: theme.muted }]}>
        Get a notification when an agent needs your approval or finishes a turn, while the app is closed.
      </Text>
      {error !== null && (
        <Text accessibilityRole="alert" style={[type.body, { color: theme.accent }]}>
          {error}
        </Text>
      )}
      <Pressable
        onPress={onTurnOn}
        disabled={busy}
        accessibilityRole="button"
        accessibilityLabel="Turn on notifications"
        accessibilityState={{ disabled: busy, busy }}
        style={[styles.button, { backgroundColor: theme.accent, opacity: busy ? 0.5 : 1 }]}
      >
        <Text style={[type.body, { color: theme.bg, fontWeight: "600" }]}>
          {busy ? "Turning on…" : "Turn on notifications"}
        </Text>
      </Pressable>
      <Pressable
        onPress={leave}
        accessibilityRole="button"
        accessibilityLabel="Not now"
        style={[styles.button, { borderColor: theme.edge, borderWidth: 1 }]}
      >
        <Text style={[type.body, { color: theme.fg }]}>Not now</Text>
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  content: { padding: spacing.lg, gap: spacing.md, paddingTop: spacing.xxl },
  camera: { height: 280, borderRadius: radius.lg, borderWidth: 1, overflow: "hidden" },
  field: { minHeight: 44, borderWidth: 1, borderRadius: radius.md, paddingHorizontal: spacing.md, fontSize: 16 },
  button: { minHeight: 48, borderRadius: radius.md, alignItems: "center", justifyContent: "center" },
});
