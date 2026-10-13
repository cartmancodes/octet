import { CameraView, useCameraPermissions } from "expo-camera";
import { useRouter } from "expo-router";
import { useCallback, useEffect, useRef, useState } from "react";
import { KeyboardAvoidingView, Platform, Pressable, ScrollView, StyleSheet, Text, TextInput, View } from "react-native";

import { useHubController, useHubSnapshot } from "../src/hub";
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
  const { notice } = useHubSnapshot();
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

  const attempt = useCallback(
    async (text: string) => {
      if (attempting.current) return;
      attempting.current = true;
      setBusy(true);
      setError(null);
      try {
        const result = await hub.pair(text, name);
        if (result.ok) router.replace("/");
        else setError(result.message);
      } finally {
        attempting.current = false;
        setBusy(false);
      }
    },
    [hub, name, router],
  );

  const onScan = useCallback(
    ({ data }: { data: string }) => {
      // The camera reports the same code many times a second: one look at each.
      if (attempting.current || data === lastScan.current) return;
      lastScan.current = data;
      setLink(data);
      void attempt(data);
    },
    [attempt],
  );

  const onPair = useCallback(() => void attempt(link), [attempt, link]);
  const onLink = useCallback((text: string) => {
    lastScan.current = null;
    setLink(text);
  }, []);

  const cameraOn = permission?.granted === true;
  const cameraOff = permission !== null && !permission.granted && !permission.canAskAgain;
  const field = [styles.field, { color: theme.fg, backgroundColor: theme.panel, borderColor: theme.edge }];

  return (
    <KeyboardAvoidingView style={{ flex: 1, backgroundColor: theme.bg }} behavior={Platform.OS === "ios" ? "padding" : undefined}>
      <ScrollView contentContainerStyle={styles.content} keyboardShouldPersistTaps="handled">
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

        {error !== null && (
          <Text accessibilityRole="alert" style={[type.body, { color: theme.accent }]}>
            {error}
          </Text>
        )}

        <Pressable
          onPress={onPair}
          disabled={busy}
          accessibilityRole="button"
          accessibilityLabel="Pair"
          accessibilityState={{ disabled: busy, busy }}
          style={[styles.button, { backgroundColor: theme.accent, opacity: busy ? 0.6 : 1 }]}
        >
          <Text style={[type.body, { color: theme.bg, fontWeight: "600" }]}>{busy ? "Pairing…" : "Pair"}</Text>
        </Pressable>
      </ScrollView>
    </KeyboardAvoidingView>
  );
}

const styles = StyleSheet.create({
  content: { padding: spacing.lg, gap: spacing.md, paddingTop: spacing.xxl },
  camera: { height: 280, borderRadius: radius.lg, borderWidth: 1, overflow: "hidden" },
  field: { minHeight: 44, borderWidth: 1, borderRadius: radius.md, paddingHorizontal: spacing.md, fontSize: 16 },
  button: { minHeight: 48, borderRadius: radius.md, alignItems: "center", justifyContent: "center" },
});
