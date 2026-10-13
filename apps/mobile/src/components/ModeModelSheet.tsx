import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { memo, useCallback, useLayoutEffect, useMemo, useRef, useState } from "react";
import { KeyboardAvoidingView, Modal, Pressable, StyleSheet, Text, TextInput, View } from "react-native";

import { confirmBiometric } from "../auth";
import type { RequestBody } from "../client/HubClient";
import type { CatalogInfo, Engine, Mode, ModelInfo } from "../protocol";
import { engineName } from "../sessions";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { useAction, useAlive, type Send } from "./send";

/** What the user types, exactly, to turn full access on. */
export const FULL_ACCESS_WORDS = "full access";
export const NO_FACE_ID = "Full access needs Face ID set up on this phone.";
export const FULL_ACCESS_WARNING = "The agent can run any command without asking.";
const BIOMETRIC_REASON = "Turn on full access for this session";

const MODE_NAMES: Record<Mode, string> = {
  ask: "Ask",
  "accept-edits": "Accept edits",
  auto: "Auto",
  "full-access": "Full access",
};

/** One line of the sheet's list: a heading, a mode or a model. */
type Item =
  | { kind: "head"; key: string; text: string }
  | { kind: "mode"; key: string; mode: Mode }
  | { kind: "model"; key: string; model: ModelInfo };

const keyOfModel = (m: ModelInfo): string => `model:${m.engine}:${m.selection}`;

/** The modes, then the models grouped by engine: engines in the order the catalog first names
 * them, models in catalog order within each. */
function itemsOf(catalog: CatalogInfo): Item[] {
  const items: Item[] = [{ kind: "head", key: "head:mode", text: "Mode" }];
  for (const mode of catalog.modes) items.push({ kind: "mode", key: `mode:${mode}`, mode });
  const byEngine = new Map<Engine, ModelInfo[]>();
  for (const m of catalog.models) {
    const group = byEngine.get(m.engine);
    if (group) group.push(m);
    else byEngine.set(m.engine, [m]);
  }
  if (byEngine.size > 0) items.push({ kind: "head", key: "head:model", text: "Model" });
  for (const [engine, models] of byEngine) {
    items.push({ kind: "head", key: `engine:${engine}`, text: engineName(engine) });
    for (const model of models) items.push({ kind: "model", key: keyOfModel(model), model });
  }
  return items;
}

const keyOf = (item: Item): string => item.key;
const typeOf = (item: Item): string => item.kind;

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    entry: { paddingHorizontal: spacing.lg, paddingTop: spacing.sm, flexDirection: "row" },
    button: {
      minHeight: 44,
      minWidth: 44,
      paddingHorizontal: spacing.lg,
      alignItems: "center",
      justifyContent: "center",
      borderRadius: radius.md,
      borderColor: theme.edge,
      borderWidth: 1,
    },
    primary: { backgroundColor: theme.accent, borderColor: theme.accent },
    off: { opacity: 0.4 },
    label: { ...type.body, color: theme.fg },
    primaryLabel: { ...type.body, color: theme.bg },
    root: { flex: 1, justifyContent: "flex-end" },
    scrim: { ...StyleSheet.absoluteFill, backgroundColor: theme.bg, opacity: 0.6 },
    sheet: {
      height: "85%",
      backgroundColor: theme.panel,
      borderTopLeftRadius: radius.lg,
      borderTopRightRadius: radius.lg,
      borderColor: theme.edge,
      borderWidth: 1,
      paddingTop: spacing.md,
      paddingBottom: spacing.xl,
    },
    head: {
      flexDirection: "row",
      alignItems: "center",
      justifyContent: "space-between",
      paddingHorizontal: spacing.lg,
      paddingBottom: spacing.sm,
    },
    title: { ...type.title, color: theme.fg },
    failure: { ...type.body, color: theme.fg, paddingHorizontal: spacing.lg, paddingBottom: spacing.sm },
    heading: {
      ...type.caption,
      color: theme.muted,
      fontWeight: "700",
      paddingHorizontal: spacing.lg,
      paddingTop: spacing.md,
      paddingBottom: spacing.xs,
    },
    row: {
      minHeight: 48,
      paddingHorizontal: spacing.lg,
      paddingVertical: spacing.sm,
      flexDirection: "row",
      alignItems: "center",
      gap: spacing.md,
    },
    rowText: { flex: 1, gap: 2 },
    name: { ...type.body, color: theme.fg },
    sub: { ...type.mono, color: theme.muted },
    mark: { ...type.caption, color: theme.accent, fontWeight: "700" },
    status: { ...type.caption, color: theme.muted },
    warning: { ...type.caption, color: theme.amber },
    ask: { paddingHorizontal: spacing.lg, paddingBottom: spacing.sm, gap: spacing.sm },
    askText: { ...type.caption, color: theme.fg },
    confirm: {
      marginHorizontal: spacing.lg,
      marginBottom: spacing.sm,
      padding: spacing.md,
      gap: spacing.sm,
      borderRadius: radius.md,
      borderColor: theme.amber,
      borderWidth: 1,
    },
    buttons: { flexDirection: "row", gap: spacing.md },
    input: {
      ...type.body,
      minHeight: 44,
      paddingHorizontal: spacing.md,
      paddingVertical: spacing.sm,
      color: theme.fg,
      backgroundColor: theme.bg,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.md,
    },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

interface RowProps {
  item: Item;
  current: boolean;
  /** A model of another engine was tapped once: the warning and Switch show. */
  asking: boolean;
  /** This choice is out, waiting for the Mac. */
  pending: boolean;
  /** Some choice is out: the others wait. */
  busy: boolean;
  onChoose: (item: Item) => void;
  onConfirm: (item: Item) => void;
}

const Row = memo(function Row({ item, current, asking, pending, busy, onChoose, onConfirm }: RowProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const choose = useCallback(() => onChoose(item), [onChoose, item]);
  const confirm = useCallback(() => onConfirm(item), [onConfirm, item]);
  if (item.kind === "head") {
    return (
      <Text accessibilityRole="header" style={s.heading}>
        {item.text}
      </Text>
    );
  }
  const name = item.kind === "mode" ? MODE_NAMES[item.mode] : item.model.name;
  const fullAccess = item.kind === "mode" && item.mode === "full-access";
  return (
    <View>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel={current ? `${name}, current` : name}
        accessibilityHint={fullAccess ? FULL_ACCESS_WARNING : undefined}
        accessibilityState={{ selected: current, disabled: busy, busy: pending }}
        disabled={busy}
        onPress={choose}
        style={s.row}
      >
        <View style={s.rowText}>
          <Text style={s.name}>{name}</Text>
          {item.kind === "model" ? <Text style={s.sub}>{item.model.id}</Text> : null}
          {fullAccess ? <Text style={s.warning}>{FULL_ACCESS_WARNING}</Text> : null}
        </View>
        {pending ? <Text style={s.status}>Changing…</Text> : current ? <Text style={s.mark}>Current</Text> : null}
      </Pressable>
      {item.kind === "model" && asking ? (
        <View style={s.ask}>
          <Text style={s.askText}>{`Switches to ${engineName(item.model.engine)}; the conversation carries over`}</Text>
          <View style={s.buttons}>
            <Pressable
              accessibilityRole="button"
              accessibilityLabel="Switch"
              accessibilityState={{ disabled: busy }}
              disabled={busy}
              onPress={confirm}
              style={[s.button, s.primary, busy && s.off]}
            >
              <Text style={s.primaryLabel}>Switch</Text>
            </Pressable>
          </View>
        </View>
      ) : null}
    </View>
  );
});

interface ModeModelSheetProps {
  session: string;
  /** The session's modes and models; null until the Mac has sent them. */
  catalog: CatalogInfo | null;
  /** Must be stable. */
  send: Send;
}

/**
 * The "Mode and model" button and the sheet it opens, for a live session (the screen unmounts it
 * otherwise, which is what stops a late Face ID result from sending anything). Ask, accept-edits
 * and auto are sent at once; full access asks for Face ID and then the words `full access`; a
 * model of another engine asks once more. The "current" marks come from the catalog, never from
 * here: the Mac's `catalog` frame after the ack moves them.
 */
export const ModeModelSheet = memo(function ModeModelSheet({ session, catalog, send }: ModeModelSheetProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const alive = useAlive();
  const { busy, failed, run, fail } = useAction(send);
  const [visible, setVisible] = useState(false);
  // Read after Face ID, which takes a while: the sheet may have been closed meanwhile.
  const shown = useRef(false);
  useLayoutEffect(() => {
    shown.current = visible;
  }, [visible]);
  const checking = useRef(false);
  const [asking, setAsking] = useState<string | null>(null);
  const [typing, setTyping] = useState(false);
  const [words, setWords] = useState("");
  const [pending, setPending] = useState<string | null>(null);

  const items = useMemo(() => (catalog ? itemsOf(catalog) : []), [catalog]);
  const currentMode = catalog?.current_mode;
  const currentModel = catalog?.current;

  const reset = useCallback(() => {
    setAsking(null);
    setTyping(false);
    setWords("");
    setPending(null);
  }, []);
  const openSheet = useCallback(() => {
    fail(null);
    setVisible(true);
  }, [fail]);
  const close = useCallback(() => {
    setVisible(false);
    reset();
    fail(null);
  }, [reset, fail]);
  // The Mac took the change: the sheet's work is done.
  const done = useCallback(() => {
    setVisible(false);
    reset();
  }, [reset]);

  const sendChoice = useCallback(
    (key: string, body: RequestBody) => {
      setAsking(null);
      setPending(key);
      run(body, done);
    },
    [run, done],
  );

  const startFullAccess = useCallback(async () => {
    checking.current = true;
    try {
      // Always, whatever the Face ID setting says.
      const checked = await confirmBiometric(BIOMETRIC_REASON);
      if (!alive() || !shown.current) return;
      if (checked === "ok") {
        setAsking(null);
        setWords("");
        setTyping(true);
      } else if (checked === "unavailable") {
        fail({ text: NO_FACE_ID, retry: false });
      }
    } finally {
      checking.current = false;
    }
  }, [alive, fail]);

  const onChoose = useCallback(
    (item: Item) => {
      if (busy || checking.current || item.kind === "head") return;
      fail(null);
      if (item.kind === "mode") {
        if (item.mode === currentMode) return;
        setTyping(false);
        if (item.mode === "full-access") {
          setAsking(null);
          void startFullAccess();
          return;
        }
        sendChoice(item.key, { type: "set_mode", session, mode: item.mode });
        return;
      }
      const { model } = item;
      if (currentModel && currentModel.engine === model.engine && currentModel.selection === model.selection) return;
      setTyping(false);
      if (currentModel && currentModel.engine !== model.engine) {
        setAsking(item.key); // the second tap is Switch
        return;
      }
      sendChoice(item.key, { type: "set_model", session, selection: model.selection });
    },
    [busy, fail, currentMode, currentModel, sendChoice, session, startFullAccess],
  );
  const onConfirm = useCallback(
    (item: Item) => {
      if (busy || item.kind !== "model") return;
      sendChoice(item.key, { type: "set_model", session, selection: item.model.selection });
    },
    [busy, sendChoice, session],
  );

  const exact = words === FULL_ACCESS_WORDS;
  const turnOn = useCallback(() => {
    if (busy || !exact) return;
    setPending("mode:full-access");
    run({ type: "set_mode", session, mode: "full-access", confirm: FULL_ACCESS_WORDS }, done);
  }, [busy, exact, run, session, done]);
  const cancelFull = useCallback(() => {
    setTyping(false);
    setWords("");
    fail(null);
  }, [fail]);

  const waiting = busy ? pending : null;
  const renderItem = useCallback<ListRenderItem<Item>>(
    ({ item }) => {
      const current =
        item.kind === "mode"
          ? item.mode === currentMode
          : item.kind === "model" &&
            currentModel != null &&
            currentModel.engine === item.model.engine &&
            currentModel.selection === item.model.selection;
      return (
        <Row
          item={item}
          current={current}
          asking={asking === item.key}
          pending={waiting === item.key}
          busy={busy}
          onChoose={onChoose}
          onConfirm={onConfirm}
        />
      );
    },
    [currentMode, currentModel, asking, waiting, busy, onChoose, onConfirm],
  );
  const extra = useMemo(
    () => ({ asking, waiting, busy, currentMode, currentModel }),
    [asking, waiting, busy, currentMode, currentModel],
  );

  if (!catalog) return null;
  const typingFull = typing && currentMode !== "full-access";
  return (
    <View style={s.entry}>
      <Pressable accessibilityRole="button" accessibilityLabel="Mode and model" onPress={openSheet} style={s.button}>
        <Text style={s.label}>Mode and model</Text>
      </Pressable>
      <Modal visible={visible} transparent animationType="slide" onRequestClose={close}>
        <KeyboardAvoidingView behavior="padding" style={s.root}>
          <View style={s.scrim} />
          <Pressable accessible={false} onPress={close} style={StyleSheet.absoluteFill} />
          <View style={s.sheet}>
            <View style={s.head}>
              <Text accessibilityRole="header" style={s.title}>
                Mode and model
              </Text>
              <Pressable accessibilityRole="button" accessibilityLabel="Close" onPress={close} style={s.button}>
                <Text style={s.label}>Close</Text>
              </Pressable>
            </View>
            {failed ? (
              <View accessibilityLiveRegion="polite">
                <Text style={s.failure}>{failed.text}</Text>
              </View>
            ) : null}
            {typingFull ? (
              <View style={s.confirm}>
                <Text style={s.warning}>{FULL_ACCESS_WARNING}</Text>
                <Text style={s.askText}>{`Type ${FULL_ACCESS_WORDS} to turn it on.`}</Text>
                <TextInput
                  accessibilityLabel="Type full access to confirm"
                  autoCapitalize="none"
                  autoCorrect={false}
                  spellCheck={false}
                  placeholder={FULL_ACCESS_WORDS}
                  placeholderTextColor={theme.muted}
                  value={words}
                  onChangeText={setWords}
                  style={s.input}
                />
                <View style={s.buttons}>
                  <Pressable
                    accessibilityRole="button"
                    accessibilityLabel="Cancel full access"
                    onPress={cancelFull}
                    style={s.button}
                  >
                    <Text style={s.label}>Cancel</Text>
                  </Pressable>
                  <Pressable
                    accessibilityRole="button"
                    accessibilityLabel="Turn on full access"
                    accessibilityState={{ disabled: busy || !exact, busy }}
                    disabled={busy || !exact}
                    onPress={turnOn}
                    style={[s.button, s.primary, (busy || !exact) && s.off]}
                  >
                    <Text style={s.primaryLabel}>{busy ? "Changing…" : "Turn on full access"}</Text>
                  </Pressable>
                </View>
              </View>
            ) : null}
            <FlashList
              data={items}
              extraData={extra}
              renderItem={renderItem}
              keyExtractor={keyOf}
              getItemType={typeOf}
              keyboardShouldPersistTaps="handled"
            />
          </View>
        </KeyboardAvoidingView>
      </Modal>
    </View>
  );
});
