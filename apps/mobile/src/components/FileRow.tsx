import { memo, useCallback } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { BAR_CELLS, STATUS_WORDS, barCells, splitPath, type DiffLine } from "../changes";
import type { ListedFile } from "../client/answers";
import { MONO, dark, light, spacing, type, useTheme, type Theme } from "../theme";

/** The page's side margin, which a diff's lines run past to the screen's edges. */
export const SIDE = 20;

function makeStyles(theme: Theme) {
  const code = { fontFamily: MONO, fontSize: 12, lineHeight: 20 };
  const line = { flexDirection: "row" as const, paddingHorizontal: SIDE };
  return StyleSheet.create({
    row: {
      minHeight: 56,
      marginHorizontal: SIDE,
      flexDirection: "row",
      alignItems: "center",
      gap: spacing.md,
      borderBottomWidth: 1,
      borderBottomColor: theme.edge,
    },
    status: { ...type.body, width: 9, color: theme.muted },
    names: { flex: 1, minWidth: 0 },
    name: { ...type.body, color: theme.fg },
    dir: { ...type.label, color: theme.muted },
    lines: { ...type.body, color: theme.muted },
    bar: { ...type.body, letterSpacing: 1 },
    on: { color: theme.accent },
    off: { color: theme.edge },
    line,
    added: { ...line, backgroundColor: theme.selected },
    sign: { ...code, width: 16, color: theme.muted },
    plus: { ...code, width: 16, color: theme.accent },
    code: { ...code, flex: 1, color: theme.fg },
    muted: { ...code, flex: 1, color: theme.muted },
    struck: { ...code, flex: 1, color: theme.muted, textDecorationLine: "line-through" },
    note: { ...type.label, paddingHorizontal: SIDE, paddingVertical: spacing.sm, color: theme.muted },
    words: { ...type.body, paddingHorizontal: SIDE, paddingVertical: spacing.sm, color: theme.fg },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);
const stylesOf = (theme: Theme) => (theme === light ? LIGHT : DARK);

const OPEN = { expanded: true, disabled: false } as const;
const SHUT = { expanded: false, disabled: false } as const;
const OPEN_OFF = { expanded: true, disabled: true } as const;
const SHUT_OFF = { expanded: false, disabled: true } as const;

interface FileRowProps {
  /** `path` cleaned by the client, drawn as plain text; `raw` the hub's own, never drawn. */
  file: ListedFile;
  open: boolean;
  /** The Mac can't be reached: the row takes no tap. */
  off: boolean;
  /** Called with the file's raw path. Must be stable, or every row draws again with the list. */
  onToggle: (path: string) => void;
}

/** One changed file: its status letter (muted), name over its directory, "+a −d", and a bar of
 * five cells, one accent cell per 8 lines changed. A tap opens or closes its diff. */
export const FileRow = memo(function FileRow({ file, open, off, onToggle }: FileRowProps) {
  const s = stylesOf(useTheme());
  const { name, dir } = splitPath(file.path);
  const on = barCells(file.add, file.del);
  const toggle = useCallback(() => onToggle(file.raw), [onToggle, file.raw]);
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={`${file.path}, ${STATUS_WORDS[file.status]}, ${file.add} added, ${file.del} removed`}
      accessibilityState={off ? (open ? OPEN_OFF : SHUT_OFF) : open ? OPEN : SHUT}
      disabled={off}
      onPress={toggle}
      style={s.row}
    >
      <Text style={s.status}>{file.status}</Text>
      <View style={s.names}>
        <Text style={s.name}>{name}</Text>
        {dir !== "" ? <Text style={s.dir}>{dir}</Text> : null}
      </View>
      <Text style={s.lines}>{`+${file.add} −${file.del}`}</Text>
      <Text style={s.bar}>
        <Text testID="bar-on" style={s.on}>
          {"■".repeat(on)}
        </Text>
        <Text testID="bar-off" style={s.off}>
          {"■".repeat(BAR_CELLS - on)}
        </Text>
      </Text>
    </Pressable>
  );
});

/** One line of a diff: `+` on the selected background with an accent sign, `−` muted and struck
 * through, a hunk's `@@` muted, the rest plain. Plain text only. */
export const DiffLineRow = memo(function DiffLineRow({ line }: { line: DiffLine }) {
  const s = stylesOf(useTheme());
  const code = line.kind === "del" ? s.struck : line.kind === "hunk" ? s.muted : s.code;
  return (
    <View testID="diff-line" style={line.kind === "add" ? s.added : s.line}>
      <Text style={line.kind === "add" ? s.plus : s.sign}>{line.sign}</Text>
      <Text selectable style={code}>
        {line.code}
      </Text>
    </View>
  );
});

/** A line under an open file: loading, a note (clipped, binary), muted; or a refusal, the hub's
 * words in fg. */
export const DiffNote = memo(function DiffNote({ text, words }: { text: string; words: boolean }) {
  const s = stylesOf(useTheme());
  return <Text style={words ? s.words : s.note}>{text}</Text>;
});
