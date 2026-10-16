import { memo, useCallback } from "react";
import { StyleSheet, View, type AccessibilityActionEvent } from "react-native";

import { spacing, useTheme } from "../theme";
import { Chip, Label } from "../ui";

interface NewChipsProps {
  /** The Mac's name. */
  mac: string;
  /** Why the Mac can't be reached ("asleep", "connecting…"), said after its name; null when it can,
   * and with one Mac (its banner says so). */
  status?: string | null;
  /** The connection is up: the Mac's dot is lit. */
  online: boolean;
  /** The folder of the chosen workspace; empty when none is known. */
  folder: string;
  /** The Mac's folders have not arrived: the folder chip says so. */
  loading: boolean;
  /** "claude / Sonnet". */
  engineModel: string;
  /** "default mode", "ask", "accept-edits", "auto" or "full access". */
  mode: string;
  /** Full access is armed: the mode chip is amber. */
  fullAccess: boolean;
  /** Every handler must be stable. */
  onMac: () => void;
  onFolder: () => void;
  onEngine: () => void;
  onMode: () => void;
  onFullAccess: () => void;
}

/** A screen reader's way to the mode chip's long press. */
const FULL_ACCESS_ACTIONS = [{ name: "fullAccess", label: "full access" }] as const;
const FULL_ACCESS_HINT = "Long-press for full access";

/** The four choices under the field: tap to change. Each cycles its value; a long press on the
 * mode (or its "full access" action, for a screen reader) starts the guarded full-access flow. */
export const NewChips = memo(function NewChips({
  mac,
  status = null,
  online,
  folder,
  loading,
  engineModel,
  mode,
  fullAccess,
  onMac,
  onFolder,
  onEngine,
  onMode,
  onFullAccess,
}: NewChipsProps) {
  const theme = useTheme();
  const onAction = useCallback(
    (event: AccessibilityActionEvent) => {
      if (event.nativeEvent.actionName === "fullAccess") onFullAccess();
    },
    [onFullAccess],
  );
  return (
    <View style={styles.box}>
      <Label style={styles.label}>Tap to change</Label>
      <View style={styles.row}>
        <Chip
          label={status === null ? mac : `${mac} · ${status}`}
          dot="●"
          dotColor={online ? theme.accent : theme.muted}
          accessibilityLabel={status === null ? `Mac ${mac}` : `Mac ${mac}, ${status}`}
          onPress={onMac}
        />
        <Chip
          label={loading ? "loading…" : folder === "" ? "no folder" : folder}
          accessibilityLabel={`Folder ${loading ? "loading…" : folder === "" ? "none" : folder}`}
          onPress={onFolder}
        />
        <Chip label={engineModel} accessibilityLabel={`Engine and model ${engineModel}`} onPress={onEngine} />
        <Chip
          label={mode}
          color={fullAccess ? theme.amber : undefined}
          accessibilityLabel={`Mode ${mode}`}
          accessibilityHint={FULL_ACCESS_HINT}
          accessibilityActions={FULL_ACCESS_ACTIONS}
          onAccessibilityAction={onAction}
          onPress={onMode}
          onLongPress={onFullAccess}
        />
      </View>
    </View>
  );
});

const styles = StyleSheet.create({
  box: { gap: spacing.sm, paddingHorizontal: spacing.lg },
  label: { paddingHorizontal: spacing.xs },
  row: { flexDirection: "row", flexWrap: "wrap", gap: 6 },
});
