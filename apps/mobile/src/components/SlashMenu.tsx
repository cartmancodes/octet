import { memo, useCallback } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";

/** A command the phone runs itself, from the prompt line. */
export interface SlashCommand {
  cmd: "/model" | "/mode" | "/new" | "/session" | "/export" | "/help";
  desc: string;
}

/** `/model` opens the ▾ menu, `/mode` moves to the next mode (as ⇧tab), and `/new`, `/session`,
 * `/export` and `/help` are the desk's own handlers, run through the hub's `command` request. Any
 * other `/` text is the desk's alone (`isSlash`), and is not sent. */
export const SLASH_COMMANDS: readonly SlashCommand[] = [
  { cmd: "/model", desc: "Switch model" },
  { cmd: "/mode", desc: "Permissions" },
  { cmd: "/new", desc: "Fresh context" },
  { cmd: "/session", desc: "Session info" },
  { cmd: "/export", desc: "Save journal" },
  { cmd: "/help", desc: "List commands" },
];

const NONE: readonly SlashCommand[] = [];

/**
 * The commands `text` could be: it is one `/` word (leading spaces aside, no space after it), and
 * each command starts with it, ignoring case. A command it names exactly comes first; the rest
 * keep their order.
 */
export function slashMatches(text: string): readonly SlashCommand[] {
  const word = text.trimStart().toLowerCase();
  if (!word.startsWith("/") || /\s/.test(word)) return NONE;
  const found = SLASH_COMMANDS.filter((c) => c.cmd.startsWith(word));
  const exact = found.findIndex((c) => c.cmd === word);
  if (exact <= 0) return found;
  return [found[exact] as SlashCommand, ...found.filter((_, i) => i !== exact)];
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    menu: {
      marginHorizontal: 14,
      marginBottom: spacing.sm,
      padding: 6,
      backgroundColor: theme.panel,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.card,
    },
    row: { height: 44, minHeight: 44, flexDirection: "row", alignItems: "center", paddingHorizontal: 10, borderRadius: 8 },
    top: { backgroundColor: theme.selected },
    mark: { ...type.body, width: 20, color: theme.accent },
    cmd: { ...type.body, width: 92, color: theme.fg },
    cmdTop: { ...type.body, width: 92, color: theme.accent },
    desc: { ...type.body, flex: 1, color: theme.fg },
    descTop: { ...type.body, flex: 1, color: theme.accent },
    value: { ...type.body, color: theme.muted, flexShrink: 1 },
    off: { opacity: 0.4 },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/** A row's style for each state, made once: a list row gets no new style object per draw. */
const rowLooks = (s: Styles) => ({
  top: [s.row, s.top],
  plain: [s.row],
  topOff: [s.row, s.top, s.off],
  plainOff: [s.row, s.off],
});
const ROWS = new Map([
  [DARK, rowLooks(DARK)],
  [LIGHT, rowLooks(LIGHT)],
]);
const rowStyle = (s: Styles, top: boolean, off: boolean) => {
  const looks = ROWS.get(s) ?? rowLooks(s);
  return off ? (top ? looks.topOff : looks.plainOff) : top ? looks.top : looks.plain;
};

interface SlashMenuProps {
  /** `slashMatches` of the prompt's text; the first is marked ▸. */
  matches: readonly SlashCommand[];
  /** Each command's current value, where it has one: the model's name, the mode. */
  values: Readonly<Partial<Record<SlashCommand["cmd"], string>>>;
  /** The Mac can't be reached: the rows do nothing. */
  disabled: boolean;
  /** Runs a command. Must be stable. */
  onRun: (cmd: SlashCommand["cmd"]) => void;
}

/** Over the prompt line while its text is a `/` word: ▸ /cmd, what it does, its current value. */
export const SlashMenu = memo(function SlashMenu({ matches, values, disabled, onRun }: SlashMenuProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  if (matches.length === 0) return null;
  return (
    <View testID="slash-menu" style={s.menu}>
      {matches.map((c, i) => (
        <SlashRow key={c.cmd} command={c} top={i === 0} value={values[c.cmd] ?? ""} disabled={disabled} s={s} onRun={onRun} />
      ))}
    </View>
  );
});

interface RowProps {
  command: SlashCommand;
  top: boolean;
  value: string;
  disabled: boolean;
  s: Styles;
  onRun: (cmd: SlashCommand["cmd"]) => void;
}

const SlashRow = memo(function SlashRow({ command, top, value, disabled, s, onRun }: RowProps) {
  const run = useCallback(() => onRun(command.cmd), [onRun, command.cmd]);
  const label = value === "" ? `${command.cmd}, ${command.desc}` : `${command.cmd}, ${command.desc}, ${value}`;
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      accessibilityState={{ disabled, selected: top }}
      disabled={disabled}
      onPress={run}
      style={rowStyle(s, top, disabled)}
    >
      <Text style={s.mark}>{top ? "▸" : " "}</Text>
      <Text style={top ? s.cmdTop : s.cmd}>{command.cmd}</Text>
      <Text numberOfLines={1} style={top ? s.descTop : s.desc}>
        {command.desc}
      </Text>
      {value !== "" ? (
        <Text numberOfLines={1} style={s.value}>
          {value}
        </Text>
      ) : null}
    </Pressable>
  );
});
