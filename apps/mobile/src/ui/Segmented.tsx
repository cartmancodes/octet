import { memo, useCallback } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { radius, type, useTheme, type Theme } from "../theme";

export interface SegmentedOption<K extends string> {
  key: K;
  label: string;
}

interface SegmentedProps<K extends string> {
  options: readonly SegmentedOption<K>[];
  value: K;
  /** Must be stable. */
  onChange: (key: K) => void;
  /** Dimmed, and a tap does nothing. */
  disabled?: boolean;
}

/** A row of tabs inside one bordered box; the selected one is filled and in accent. */
function SegmentedImpl<K extends string>({ options, value, onChange, disabled = false }: SegmentedProps<K>) {
  const theme = useTheme();
  return (
    <View accessibilityRole="tablist" style={[styles.box, { borderColor: theme.edge }, disabled ? styles.off : null]}>
      {options.map((o) => (
        <Segment key={o.key} option={o} selected={o.key === value} onChange={onChange} theme={theme} disabled={disabled} />
      ))}
    </View>
  );
}

export const Segmented = memo(SegmentedImpl) as typeof SegmentedImpl;

interface SegmentProps<K extends string> {
  option: SegmentedOption<K>;
  selected: boolean;
  onChange: (key: K) => void;
  theme: Theme;
  disabled: boolean;
}

function SegmentImpl<K extends string>({ option, selected, onChange, theme, disabled }: SegmentProps<K>) {
  const press = useCallback(() => onChange(option.key), [onChange, option.key]);
  return (
    <Pressable
      accessibilityRole="tab"
      accessibilityLabel={option.label}
      accessibilityState={{ selected, disabled }}
      disabled={disabled}
      onPress={press}
      style={[styles.segment, selected ? { backgroundColor: theme.selected } : null]}
    >
      <Text numberOfLines={1} style={[type.body, { color: selected ? theme.accent : theme.fg }]}>
        {option.label}
      </Text>
    </Pressable>
  );
}

const Segment = memo(SegmentImpl) as typeof SegmentImpl;

const styles = StyleSheet.create({
  box: { flexDirection: "row", gap: 4, padding: 3, borderWidth: 1, borderRadius: radius.card },
  off: { opacity: 0.4 },
  segment: { flex: 1, minHeight: 44, borderRadius: 9, alignItems: "center", justifyContent: "center", paddingHorizontal: 4 },
});
