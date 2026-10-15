import { memo, useEffect, useState } from "react";
import { Keyboard, Platform, Pressable, StyleSheet, Text, View } from "react-native";

import { dark, light, spacing, type, useTheme, type Theme } from "../theme";

type Tone = "accent" | "fg" | "muted";
type Width = "one" | "wide" | "wider";

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    bar: {
      flexDirection: "row",
      gap: 6,
      paddingTop: spacing.sm,
      paddingHorizontal: spacing.sm,
      backgroundColor: theme.panel,
      borderTopColor: theme.edge,
      borderTopWidth: 1,
    },
    key: { height: 44, minHeight: 44, minWidth: 44, alignItems: "center", justifyContent: "center", borderRadius: 8, backgroundColor: theme.bg },
    one: { flex: 1 },
    wide: { flex: 1.2 },
    wider: { flex: 1.3 },
    off: { opacity: 0.4 },
    accent: { ...type.body, color: theme.accent },
    fg: { ...type.body, color: theme.fg },
    muted: { ...type.body, color: theme.muted },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/** Each key's box for its width and state, made once. */
const boxLooks = (s: Styles) => {
  const looks = {} as Record<`${Width}${"" | "Off"}`, object[]>;
  for (const w of ["one", "wide", "wider"] as const) {
    looks[w] = [s.key, s[w]];
    looks[`${w}Off`] = [s.key, s[w], s.off];
  }
  return looks;
};
const BOXES = new Map([
  [DARK, boxLooks(DARK)],
  [LIGHT, boxLooks(LIGHT)],
]);

interface KeyProps {
  glyph: string;
  /** What a screen reader says: the action, then the key. */
  label: string;
  width: Width;
  tone: Tone;
  disabled: boolean;
  /** Its command is out. */
  busy?: boolean;
  /** Must be stable. */
  onPress: () => void;
}

const Key = memo(function Key({ glyph, label, width, tone, disabled, busy = false, onPress }: KeyProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const boxes = BOXES.get(s) ?? boxLooks(s);
  const look: keyof typeof boxes = disabled ? `${width}Off` : width;
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      accessibilityState={{ disabled, busy }}
      disabled={disabled}
      onPress={onPress}
      style={boxes[look]}
    >
      <Text style={s[tone]}>{glyph}</Text>
    </Pressable>
  );
});

export interface KeyBarProps {
  /** An approval waits: the keys are y · n · esc. */
  approval: boolean;
  /** The Mac can't be reached: every key is off. */
  offline: boolean;
  /** A turn runs: esc is in accent. */
  running: boolean;
  canCancel: boolean;
  canMode: boolean;
  canUp: boolean;
  canSlash: boolean;
  canSend: boolean;
  /** What ⏎ does, in a word or two: "Send", "Queue", "Steer", "Run /model". */
  sendLabel: string;
  /** The front approval can be answered now (armed, nothing over it, no answer out). */
  canAnswer: boolean;
  /** Which key's command is out: esc's cancel, ⇧tab's set_mode, ⏎'s prompt (or its deny), or
   * y n esc's answer. */
  cancelBusy: boolean;
  modeBusy: boolean;
  sendBusy: boolean;
  answerBusy: boolean;
  /** The safe area under the bar (the home indicator): padded only while the keyboard is down. */
  bottom: number;
  /** Each must be stable. */
  onCancel: () => void;
  onMode: () => void;
  onUp: () => void;
  onSlash: () => void;
  onSend: () => void;
  onAllow: () => void;
  onDeny: () => void;
}

/** The bar's own room at the bottom, with the keyboard up or no home indicator. */
const GAP = spacing.sm;

/** Whether the keyboard is up. iOS says so before it moves, Android after. */
function useKeyboardUp(): boolean {
  // A keyboard already up when the bar mounts (the field kept focus) counts from the start.
  const [up, setUp] = useState(() => Keyboard.isVisible());
  useEffect(() => {
    const ios = Platform.OS === "ios";
    const show = Keyboard.addListener(ios ? "keyboardWillShow" : "keyboardDidShow", () => setUp(true));
    const hide = Keyboard.addListener(ios ? "keyboardWillHide" : "keyboardDidHide", () => setUp(false));
    return () => {
      show.remove();
      hide.remove();
    };
  }, []);
  return up;
}

/**
 * The key bar under the prompt line, the terminal's keys for a thumb. With no approval waiting:
 * esc (cancel the running turn) · ⇧tab (the next mode) · ↑ (the last text sent) · / (start a
 * command) · ⏎ (send). With one waiting: y (Allow) · n (Deny) · esc (Deny). Allow is once only:
 * there is no `a` key. It only draws: each key's guard is its caller's, the same as its button's.
 */
export const KeyBar = memo(function KeyBar(p: KeyBarProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const off = p.offline;
  // The keyboard covers the home indicator: its room is needed only while the keyboard is down.
  const keyboard = useKeyboardUp();
  const pad = { paddingBottom: keyboard ? GAP : Math.max(p.bottom, GAP) };
  if (p.approval) {
    const answerOff = off || !p.canAnswer;
    return (
      <View testID="key-bar" style={[s.bar, pad]}>
        <Key glyph="y" label="Allow (y)" width="one" tone="accent" disabled={answerOff} busy={p.answerBusy} onPress={p.onAllow} />
        <Key glyph="n" label="Deny (n)" width="one" tone="fg" disabled={answerOff} busy={p.answerBusy} onPress={p.onDeny} />
        <Key glyph="esc" label="Deny (esc)" width="wide" tone="fg" disabled={answerOff} busy={p.answerBusy} onPress={p.onDeny} />
      </View>
    );
  }
  const sendOff = off || !p.canSend;
  return (
    <View testID="key-bar" style={[s.bar, pad]}>
      <Key glyph="esc" label="Cancel (esc)" width="one" tone={p.running ? "accent" : "fg"} disabled={off || !p.canCancel} busy={p.cancelBusy} onPress={p.onCancel} />
      <Key glyph="⇧tab" label="Next mode (⇧tab)" width="wider" tone="fg" disabled={off || !p.canMode} busy={p.modeBusy} onPress={p.onMode} />
      <Key glyph="↑" label="Last prompt (↑)" width="one" tone="fg" disabled={off || !p.canUp} onPress={p.onUp} />
      <Key glyph="/" label="Command (/)" width="one" tone="fg" disabled={off || !p.canSlash} onPress={p.onSlash} />
      <Key glyph="⏎" label={`${p.sendLabel} (⏎)`} width="wide" tone={sendOff ? "muted" : "accent"} disabled={sendOff} busy={p.sendBusy} onPress={p.onSend} />
    </View>
  );
});
