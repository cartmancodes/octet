import { memo } from "react";
import { StyleSheet, View } from "react-native";

import { spacing, useTheme } from "../theme";
import { Chip, Label } from "../ui";

interface NewChipsProps {
  /** The Mac's name. */
  mac: string;
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

/** The four choices under the field: tap to change. Each cycles its value; a long press on the
 * mode starts the guarded full-access flow. */
export const NewChips = memo(function NewChips({
  mac,
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
  return (
    <View style={styles.box}>
      <Label style={styles.label}>Tap to change</Label>
      <View style={styles.row}>
        <Chip label={mac} dot="●" dotColor={online ? theme.accent : theme.muted} accessibilityLabel={`Mac ${mac}`} onPress={onMac} />
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
