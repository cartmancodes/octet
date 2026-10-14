import Constants from "expo-constants";
import { memo } from "react";
import { StyleSheet, Text, View } from "react-native";

import { Mascot } from "../mascot/Mascot";
import type { PoseName } from "../mascot/poses";
import { TEXT_SIZES, type TextSize } from "../persist";
import type { Mode } from "../protocol";
import { homePath, poseWord } from "../session";
import { MONO, dark, light, spacing, useTheme, type Theme } from "../theme";

/** The mascot over the transcript, in points. */
export const TRANSCRIPT_MASCOT = 44;

function makeStyles(theme: Theme, size: TextSize) {
  const text = { fontFamily: MONO, fontSize: size, lineHeight: size + 5 };
  return StyleSheet.create({
    head: { flexDirection: "row", alignItems: "center", gap: 10, paddingHorizontal: spacing.lg, paddingTop: 6, paddingBottom: spacing.sm },
    lines: { flex: 1, minWidth: 0 },
    text: { ...text, color: theme.muted },
    accentBold: { ...text, color: theme.accent, fontWeight: "700" },
    mutedBold: { ...text, color: theme.muted, fontWeight: "700" },
    accent: { ...text, color: theme.accent },
    amber: { ...text, color: theme.amber },
    muted: { ...text, color: theme.muted },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const STYLES: Record<"dark" | "light", Record<TextSize, Styles>> = {
  dark: Object.fromEntries(TEXT_SIZES.map((n) => [n, makeStyles(dark, n)])) as Record<TextSize, Styles>,
  light: Object.fromEntries(TEXT_SIZES.map((n) => [n, makeStyles(light, n)])) as Record<TextSize, Styles>,
};

const VERSION = Constants.expoConfig?.version;

interface TranscriptHeaderProps {
  pose: PoseName;
  mode: Mode;
  /** "claude / Sonnet". */
  engineModel: string;
  workspace: string;
  /** The Mac's name. */
  mac: string;
  size: TextSize;
}

/**
 * The lines over the transcript, as the terminal UI's: the mascot in the session's pose, "Octet
 * v<version> · <mac>", the mode (accent unless ask) with the engine and model, and the phase
 * (accent; amber for an approval or an error) with the workspace. Every string is the Mac's, drawn
 * as text.
 */
export const TranscriptHeader = memo(function TranscriptHeader({ pose, mode, engineModel, workspace, mac, size }: TranscriptHeaderProps) {
  const theme = useTheme();
  const s = STYLES[theme === light ? "light" : "dark"][size];
  const alarm = pose === "approval" || pose === "error";
  return (
    <View testID="transcript-header" style={s.head}>
      <View testID="session-mascot">
        <Mascot pose={pose} size={TRANSCRIPT_MASCOT} />
      </View>
      <View style={s.lines}>
        <Text style={s.text}>
          <Text style={s.accentBold}>Octet</Text>
          <Text style={s.muted}>{VERSION ? ` v${VERSION} · ${mac}` : ` · ${mac}`}</Text>
        </Text>
        <Text style={s.text}>
          <Text style={mode === "ask" ? s.mutedBold : s.accentBold}>{mode}</Text>
          <Text style={s.muted}>{` · ${engineModel}`}</Text>
        </Text>
        <Text numberOfLines={1} style={s.text}>
          <Text style={alarm ? s.amber : s.accent}>{poseWord(pose)}</Text>
          <Text style={s.muted}>{`  ·  ${homePath(workspace)}`}</Text>
        </Text>
      </View>
    </View>
  );
});
