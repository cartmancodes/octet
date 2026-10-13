import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { memo, useCallback, useMemo, useState } from "react";
import { KeyboardAvoidingView, Platform, Pressable, StyleSheet, Text, View } from "react-native";

import type { RequestBody } from "../client/HubClient";
import type { CatalogInfo, Engine, Mode } from "../protocol";
import { modelsFor } from "../recent";
import { clipLabel, engineName, sessionName } from "../sessions";
import { radius, spacing, type, useTheme } from "../theme";
import { FullAccessConfirm } from "./FullAccessConfirm";
import { FULL_ACCESS_WARNING, FULL_ACCESS_WORDS, useFullAccessArm } from "./fullAccess";
import { MODE_NAMES } from "./ModeModelSheet";
import type { Failure } from "./send";

export type StartBody = Extract<RequestBody, { type: "start" }>;

const BIOMETRIC_REASON = "Start a session with full access";
export const NO_WORKSPACE = "Start a session on the Mac first, or add a workspace to hub.json.";

const ENGINES: readonly Engine[] = ["claude", "codex", "demo"];
const MODES: readonly Mode[] = ["ask", "accept-edits", "auto", "full-access"];

type Group = "engine" | "workspace" | "model" | "mode";

/** One line of the form: a heading or a choice in one of the four groups (null is "Default"). */
type Line =
  | { kind: "head"; key: string; text: string }
  | { kind: "option"; key: string; group: Group; value: string | null; label: string; speak?: string; sub?: string; warning?: boolean };

interface Choice {
  engine: Engine;
  workspace: string | null;
  model: string | null;
  mode: Mode | null;
}

const keyOf = (line: Line): string => line.key;
const typeOf = (line: Line): string => line.kind;

function linesOf(workspaces: readonly string[], catalogs: readonly CatalogInfo[], engine: Engine): Line[] {
  const lines: Line[] = [{ kind: "head", key: "head:engine", text: "Engine" }];
  for (const e of ENGINES) {
    lines.push({ kind: "option", key: `engine:${e}`, group: "engine", value: e, label: engineName(e) });
  }
  lines.push({ kind: "head", key: "head:workspace", text: "Workspace" });
  const folders = new Map<string, number>();
  for (const w of workspaces) folders.set(sessionName(w), (folders.get(sessionName(w)) ?? 0) + 1);
  for (const w of workspaces) {
    const folder = sessionName(w);
    // Two folders of one name are told apart by their paths.
    const speak = (folders.get(folder) ?? 0) > 1 ? `${folder}, ${w}` : folder;
    lines.push({ kind: "option", key: `workspace:${w}`, group: "workspace", value: w, label: folder, speak, sub: w });
  }
  lines.push({ kind: "head", key: "head:model", text: "Model" });
  lines.push({ kind: "option", key: "model:", group: "model", value: null, label: "Default" });
  for (const m of modelsFor(catalogs, engine)) {
    lines.push({ kind: "option", key: `model:${m.selection}`, group: "model", value: m.selection, label: m.name, sub: m.id });
  }
  lines.push({ kind: "head", key: "head:mode", text: "Mode" });
  lines.push({ kind: "option", key: "mode:", group: "mode", value: null, label: "Default" });
  for (const mode of MODES) {
    lines.push({
      kind: "option",
      key: `mode:${mode}`,
      group: "mode",
      value: mode,
      label: MODE_NAMES[mode],
      warning: mode === "full-access",
    });
  }
  return lines;
}

interface RowProps {
  line: Line;
  selected: boolean;
  onChoose: (line: Line) => void;
}

const Row = memo(function Row({ line, selected, onChoose }: RowProps) {
  const theme = useTheme();
  const choose = useCallback(() => onChoose(line), [onChoose, line]);
  if (line.kind === "head") {
    return (
      <Text accessibilityRole="header" style={[type.caption, styles.heading, { color: theme.muted }]}>
        {line.text}
      </Text>
    );
  }
  return (
    <Pressable
      testID={line.group === "workspace" ? line.key : undefined}
      accessibilityRole="radio"
      accessibilityLabel={clipLabel(line.speak ?? line.label)}
      accessibilityHint={line.warning ? FULL_ACCESS_WARNING : undefined}
      accessibilityState={selected ? CHECKED : UNCHECKED}
      onPress={choose}
      style={styles.option}
    >
      <View style={[styles.dot, { borderColor: selected ? theme.accent : theme.edge }]}>
        {selected ? <View style={[styles.dotFill, { backgroundColor: theme.accent }]} /> : null}
      </View>
      <View style={styles.optionText}>
        <Text numberOfLines={1} style={[type.body, { color: theme.fg }]}>
          {line.label}
        </Text>
        {line.sub ? (
          <Text numberOfLines={1} ellipsizeMode="head" style={[type.mono, { color: theme.muted }]}>
            {line.sub}
          </Text>
        ) : null}
        {line.warning ? <Text style={[type.caption, { color: theme.amber }]}>{FULL_ACCESS_WARNING}</Text> : null}
      </View>
    </Pressable>
  );
});

const CHECKED = { checked: true } as const;
const UNCHECKED = { checked: false } as const;

interface NewFormProps {
  /** The workspaces the hub may start in; never empty (the screen says so instead). */
  workspaces: readonly string[];
  /** The live sessions' catalogs, for the models. */
  catalogs: readonly CatalogInfo[];
  /** A start is out or a launched session is awaited. */
  locked: boolean;
  /** The last start was refused: the full-access panel closes, so the next try asks for Face ID. */
  failed: Failure | null;
  /** Must be stable. */
  onStart: (body: StartBody) => void;
  /** Shows (or, null, clears) a failure in the screen's status area. Must be stable. */
  onFail: (failed: Failure | null) => void;
}

/**
 * A new session: engine, workspace, optional model and mode, and Start. Full access asks for Face
 * ID and then the words `full access` before it sends anything, every time.
 */
export const NewForm = memo(function NewForm({ workspaces, catalogs, locked, failed, onStart, onFail }: NewFormProps) {
  const theme = useTheme();
  const [choice, setChoice] = useState<Choice>({ engine: "claude", workspace: null, model: null, mode: null });

  const lines = useMemo(() => linesOf(workspaces, catalogs, choice.engine), [workspaces, catalogs, choice.engine]);
  // What will be sent: a workspace or model that has left the lists is replaced (the first
  // workspace; no model), and the full-access approval below goes with it.
  const workspace = choice.workspace !== null && workspaces.includes(choice.workspace) ? choice.workspace : (workspaces[0] ?? null);
  const model =
    choice.model !== null && modelsFor(catalogs, choice.engine).some((m) => m.selection === choice.model) ? choice.model : null;

  const bodyOf = useCallback(
    (mode: Mode | null): StartBody | null => {
      if (workspace === null) return null;
      return {
        type: "start",
        engine: choice.engine,
        workspace,
        ...(model !== null ? { model } : {}),
        ...(mode !== null ? { mode } : {}),
        ...(mode === "full-access" ? { confirm: FULL_ACCESS_WORDS } : {}),
      };
    },
    [choice.engine, workspace, model],
  );

  // Face ID approves exactly this start: the engine, workspace, model and (no) resume it will
  // send, and the mode. If any of them changes, the approval is gone.
  const full = useFullAccessArm({
    reason: BIOMETRIC_REASON,
    key: JSON.stringify([choice.engine, workspace, model, null, choice.mode]),
    failed,
    fail: onFail,
  });
  const { armed, arm, disarm, checking } = full;

  const onChoose = useCallback(
    (line: Line) => {
      if (line.kind !== "option") return;
      disarm();
      onFail(null);
      setChoice((c) => {
        switch (line.group) {
          case "engine":
            return c.engine === line.value ? c : { ...c, engine: line.value as Engine, model: null };
          case "workspace":
            return { ...c, workspace: line.value };
          case "model":
            return { ...c, model: line.value };
          case "mode":
            return { ...c, mode: line.value as Mode | null };
        }
      });
    },
    [onFail, disarm],
  );

  const start = useCallback(() => {
    if (locked || checking()) return;
    onFail(null);
    if (choice.mode !== "full-access") {
      const body = bodyOf(choice.mode);
      if (body) onStart(body);
      return;
    }
    disarm();
    void arm(); // Face ID, always, whatever the setting says
  }, [locked, checking, choice.mode, bodyOf, onStart, onFail, arm, disarm]);

  const turnOn = useCallback(() => {
    if (locked || !armed) return;
    const body = bodyOf("full-access");
    disarm(); // sent: the approval is spent
    if (body) onStart(body);
  }, [locked, armed, bodyOf, onStart, disarm]);
  const cancelFull = useCallback(() => {
    disarm();
    onFail(null);
  }, [disarm, onFail]);

  // What each group has chosen now: the workspace and model as they will be sent.
  const chosen = useCallback(
    (group: Group): string | null => {
      switch (group) {
        case "engine":
          return choice.engine;
        case "workspace":
          return workspace;
        case "model":
          return model;
        case "mode":
          return choice.mode;
      }
    },
    [choice.engine, choice.mode, workspace, model],
  );
  const renderItem = useCallback<ListRenderItem<Line>>(
    ({ item }) => {
      const selected = item.kind === "option" && item.value === chosen(item.group);
      return <Row line={item} selected={selected} onChoose={onChoose} />;
    },
    [chosen, onChoose],
  );

  const off = locked || workspace === null;
  return (
    <KeyboardAvoidingView behavior={Platform.OS === "ios" ? "padding" : undefined} style={styles.form}>
      <FlashList
        testID="new-list"
        data={lines}
        extraData={chosen}
        renderItem={renderItem}
        keyExtractor={keyOf}
        getItemType={typeOf}
        keyboardShouldPersistTaps="handled"
      />
      {armed ? <FullAccessConfirm busy={locked} onTurnOn={turnOn} onCancel={cancelFull} /> : null}
      <View style={[styles.bar, { backgroundColor: theme.panel, borderColor: theme.edge }]}>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Start session"
          accessibilityState={{ disabled: off, busy: locked }}
          disabled={off}
          onPress={start}
          style={[styles.start, { backgroundColor: theme.accent }, off && styles.off]}
        >
          <Text style={[type.body, { color: theme.bg }]}>Start session</Text>
        </Pressable>
      </View>
    </KeyboardAvoidingView>
  );
});

const styles = StyleSheet.create({
  form: { flex: 1 },
  heading: {
    fontWeight: "700",
    paddingHorizontal: spacing.lg,
    paddingTop: spacing.md,
    paddingBottom: spacing.xs,
  },
  option: {
    minHeight: 48,
    flexDirection: "row",
    alignItems: "center",
    gap: spacing.md,
    paddingHorizontal: spacing.lg,
    paddingVertical: spacing.sm,
  },
  optionText: { flex: 1, gap: 2 },
  dot: { width: 22, height: 22, borderRadius: 11, borderWidth: 2, alignItems: "center", justifyContent: "center" },
  dotFill: { width: 10, height: 10, borderRadius: 5 },
  bar: { padding: spacing.lg, borderTopWidth: StyleSheet.hairlineWidth },
  start: {
    minHeight: 48,
    alignItems: "center",
    justifyContent: "center",
    borderRadius: radius.md,
  },
  off: { opacity: 0.4 },
});
