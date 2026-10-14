import { memo, useCallback, useMemo, useState } from "react";
import { KeyboardAvoidingView, Modal, Platform, Pressable, ScrollView, StyleSheet, Text, View } from "react-native";

import type { RequestBody } from "../client/HubClient";
import type { CatalogInfo, Engine, Mode, ModelInfo } from "../protocol";
import { shortId } from "../session";
import { clipLabel, engineName } from "../sessions";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { Segmented, type SegmentedOption } from "../ui";
import { FullAccessConfirm } from "./FullAccessConfirm";
import { FULL_ACCESS_WARNING, FULL_ACCESS_WORDS, useFullAccessArm } from "./fullAccess";
import { useAction, type Send } from "./send";

const BIOMETRIC_REASON = "Turn on full access for this session";
/** In place of the models and modes until the Mac sends the session's catalog. */
export const WAITING_FOR_MAC = "Waiting for the Mac…";
/** Over the open menu while the session has an approval waiting: the menu covers the approval. */
export const APPROVAL_WAITING = "An approval is waiting — close this to answer it.";

/** The line under the permission modes, for the current one. */
export const MODE_HELP: Record<Mode, string> = {
  ask: "Ask before every command and edit.",
  "accept-edits": "Edit files without asking; still ask before commands.",
  auto: "Run commands and edits without asking.",
  "full-access": FULL_ACCESS_WARNING,
};

/** The modes the segmented control offers; full access has its own guarded row. */
const PLAIN_MODES: readonly Mode[] = ["ask", "accept-edits", "auto"];

/** The scrim is dark ink in both themes. */
const INK = dark.bg;

const keyOfModel = (m: ModelInfo): string => `model:${m.engine}:${m.selection}`;

interface Group {
  engine: Engine;
  models: ModelInfo[];
}

/** The catalog's models by engine, each listed once (the first wins): the session's engine first,
 * then the others in the order the catalog first names them; models in catalog order. */
function groupsOf(catalog: CatalogInfo, engine: Engine): Group[] {
  const byEngine = new Map<Engine, ModelInfo[]>();
  const seen = new Set<string>();
  for (const m of catalog.models) {
    const key = keyOfModel(m);
    if (seen.has(key)) continue;
    seen.add(key);
    const group = byEngine.get(m.engine);
    if (group) group.push(m);
    else byEngine.set(m.engine, [m]);
  }
  const groups = [...byEngine].map(([e, models]) => ({ engine: e, models }));
  return [...groups.filter((g) => g.engine === engine), ...groups.filter((g) => g.engine !== engine)];
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    root: { flex: 1 },
    scrim: { ...StyleSheet.absoluteFill, backgroundColor: INK, opacity: 0.5 },
    panel: {
      flexShrink: 1,
      marginHorizontal: spacing.md,
      marginBottom: spacing.lg,
      backgroundColor: theme.panel,
      borderColor: theme.edge,
      borderWidth: 1,
      borderRadius: radius.card,
    },
    content: { paddingTop: 14, paddingHorizontal: 14, paddingBottom: 10, gap: 14 },
    section: { gap: spacing.xs },
    modes: { gap: 6 },
    label: { ...type.label, color: theme.muted },
    row: {
      minHeight: 44,
      flexDirection: "row",
      alignItems: "center",
      justifyContent: "space-between",
      gap: spacing.sm,
      paddingHorizontal: 10,
      marginHorizontal: -4,
      borderRadius: 8,
    },
    on: { backgroundColor: theme.selected },
    name: { ...type.body, color: theme.fg, flexShrink: 1 },
    nameOn: { ...type.body, color: theme.accent, flexShrink: 1 },
    mark: { ...type.body, color: theme.accent },
    status: { ...type.label, color: theme.muted },
    off: { opacity: 0.4 },
    help: { ...type.body, color: theme.muted },
    text: { ...type.body, color: theme.fg },
    failure: { ...type.body, color: theme.fg },
    approval: { ...type.body, color: theme.amber, fontWeight: "600" },
    ask: { gap: spacing.sm, paddingBottom: spacing.xs },
    askText: { ...type.body, color: theme.fg },
    switch: {
      alignSelf: "flex-start",
      minHeight: 44,
      minWidth: 44,
      paddingHorizontal: spacing.lg,
      alignItems: "center",
      justifyContent: "center",
      borderRadius: radius.card,
      backgroundColor: theme.accent,
    },
    switchText: { ...type.body, color: theme.bg },
    line: {
      minHeight: 44,
      flexDirection: "row",
      alignItems: "center",
      justifyContent: "space-between",
      gap: spacing.sm,
      borderTopWidth: 1,
      borderTopColor: theme.edge,
    },
    full: { paddingVertical: spacing.xs, flexShrink: 1 },
    warning: { ...type.label, color: theme.amber },
    muted: { ...type.body, color: theme.muted },
    confirm: { marginHorizontal: -spacing.lg },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

const ON = { selected: true } as const;

interface ModelRowProps {
  model: ModelInfo;
  current: boolean;
  /** Another engine's model was tapped once: the warning and Switch show. */
  asking: boolean;
  /** This choice is out, waiting for the Mac. */
  pending: boolean;
  /** Some choice is out, or the Mac can't be reached: the rows wait. */
  held: boolean;
  s: Styles;
  onChoose: (model: ModelInfo) => void;
  onConfirm: (model: ModelInfo) => void;
}

const ModelRow = memo(function ModelRow({ model, current, asking, pending, held, s, onChoose, onConfirm }: ModelRowProps) {
  const choose = useCallback(() => onChoose(model), [onChoose, model]);
  const confirm = useCallback(() => onConfirm(model), [onConfirm, model]);
  const name = clipLabel(model.name);
  return (
    <View>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel={current ? `${name}, current` : name}
        accessibilityState={{ selected: current, disabled: held, busy: pending }}
        disabled={held}
        onPress={choose}
        style={[s.row, current ? s.on : null, held && !current ? s.off : null]}
      >
        <Text numberOfLines={1} style={current ? s.nameOn : s.name}>
          {model.name}
        </Text>
        {pending ? <Text style={s.status}>Changing…</Text> : current ? <Text style={s.mark}>✓</Text> : null}
      </Pressable>
      {asking ? (
        <View style={s.ask}>
          <Text style={s.askText}>{`Switches to ${engineName(model.engine)}; the conversation carries over`}</Text>
          <Pressable
            accessibilityRole="button"
            accessibilityLabel="Switch"
            accessibilityState={{ disabled: held }}
            disabled={held}
            onPress={confirm}
            style={[s.switch, held ? s.off : null]}
          >
            <Text style={s.switchText}>Switch</Text>
          </Pressable>
        </View>
      ) : null}
    </View>
  );
});

interface ModelModeMenuProps {
  session: string;
  /** The session's engine: its models come first. */
  engine: Engine;
  /** The session's modes and models; null until the Mac has sent them. */
  catalog: CatalogInfo | null;
  /** Must be stable. */
  send: Send;
  /** The Mac can't be reached: the choices are off. */
  offline: boolean;
  /** The session has an approval waiting, which the open menu covers. */
  approval?: boolean;
  /** Where the panel hangs from: the header's bottom edge. */
  top: number;
  /** Closes the menu. Must be stable. */
  onClose: () => void;
  /** Opens the Session info sheet in the menu's place. Must be stable. */
  onInfo: () => void;
}

/**
 * The ▾ menu under the Conversation's header: the session's models (✓ on the current one), the
 * permission mode, full access, and Session info. Mounted only while open, and only for a live
 * session: unmounting it (closing, or the session ending) is what stops a late Face ID result from
 * sending anything. A model is sent at once, or after Switch for another engine's; a mode is sent
 * at once; full access asks for Face ID and then the words `full access`. The marks come from the
 * catalog, never from here: the Mac's `catalog` frame after the ack moves them.
 */
export const ModelModeMenu = memo(function ModelModeMenu({
  session,
  engine,
  catalog,
  send,
  offline,
  approval = false,
  top,
  onClose,
  onInfo,
}: ModelModeMenuProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const { busy, failed, run, fail } = useAction(send);
  const [asking, setAsking] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);

  const groups = useMemo(() => (catalog ? groupsOf(catalog, engine) : []), [catalog, engine]);
  const modes = useMemo<SegmentedOption<Mode>[]>(
    () => PLAIN_MODES.filter((m) => catalog?.modes.includes(m)).map((m) => ({ key: m, label: m })),
    [catalog],
  );
  const offersFull = catalog?.modes.includes("full-access") ?? false;
  const currentMode = catalog?.current_mode;
  const currentModel = catalog?.current;

  // Face ID approves full access for this session at this mode: when the desk changes the mode,
  // a refusal comes, the menu closes or the words are sent, the approval is gone and the next
  // try starts again from Face ID.
  const { armed, arm, disarm, checking } = useFullAccessArm({
    reason: BIOMETRIC_REASON,
    key: `${session}|${currentMode ?? ""}`,
    failed,
    fail,
  });

  // While a change is out the menu stays: its refusal (or its ack) has to land somewhere.
  const close = useCallback(() => {
    if (!busy) onClose();
  }, [busy, onClose]);

  const sendChoice = useCallback(
    (key: string, body: RequestBody, onAck?: () => void) => {
      setAsking(null);
      setPending(key);
      run(body, onAck);
    },
    [run],
  );

  const onChoose = useCallback(
    (model: ModelInfo) => {
      if (busy || offline || checking()) return;
      fail(null);
      if (currentModel && currentModel.engine === model.engine && currentModel.selection === model.selection) return;
      disarm();
      if (currentModel && currentModel.engine !== model.engine) {
        setAsking(keyOfModel(model)); // the second tap is Switch
        return;
      }
      sendChoice(keyOfModel(model), { type: "set_model", session, selection: model.selection }, onClose);
    },
    [busy, offline, checking, fail, currentModel, disarm, sendChoice, session, onClose],
  );
  const onConfirm = useCallback(
    (model: ModelInfo) => {
      if (busy || offline) return;
      sendChoice(keyOfModel(model), { type: "set_model", session, selection: model.selection }, onClose);
    },
    [busy, offline, sendChoice, session, onClose],
  );
  // A mode is sent at once and the menu stays, so its help line follows the Mac's catalog.
  const onMode = useCallback(
    (mode: Mode) => {
      if (busy || offline || checking()) return;
      fail(null);
      if (mode === currentMode) return;
      disarm();
      sendChoice(`mode:${mode}`, { type: "set_mode", session, mode });
    },
    [busy, offline, checking, fail, currentMode, disarm, sendChoice, session],
  );

  const onFull = useCallback(() => {
    if (busy || offline || checking() || currentMode === "full-access") return;
    fail(null);
    setAsking(null);
    void arm();
  }, [busy, offline, checking, currentMode, fail, arm]);
  const turnOn = useCallback(() => {
    if (busy || offline || !armed) return;
    disarm(); // sent: the approval is spent
    setPending("mode:full-access");
    run({ type: "set_mode", session, mode: "full-access", confirm: FULL_ACCESS_WORDS }, onClose);
  }, [busy, offline, armed, disarm, run, session, onClose]);
  const cancelFull = useCallback(() => {
    disarm();
    fail(null);
  }, [disarm, fail]);

  const info = useCallback(() => {
    if (!busy) onInfo();
  }, [busy, onInfo]);

  const waiting = busy ? pending : null;
  const held = busy || offline;
  const fullCurrent = currentMode === "full-access";
  const short = shortId(session);

  return (
    <Modal testID="menu" visible transparent animationType="fade" onRequestClose={close}>
      <KeyboardAvoidingView behavior={Platform.OS === "ios" ? "padding" : undefined} style={s.root}>
        <View style={[s.scrim, { top }]} />
        <Pressable testID="menu-scrim" accessible={false} onPress={close} style={StyleSheet.absoluteFill} />
        <View style={[s.panel, { marginTop: top + 2 }]}>
          <ScrollView keyboardShouldPersistTaps="handled" contentContainerStyle={s.content}>
            {approval ? (
              <View accessibilityLiveRegion="polite">
                <Text style={s.approval}>{APPROVAL_WAITING}</Text>
              </View>
            ) : null}
            {failed ? (
              <View accessibilityLiveRegion="polite">
                <Text style={s.failure}>{failed.text}</Text>
              </View>
            ) : null}
            {catalog === null ? (
              <Text style={s.muted}>{WAITING_FOR_MAC}</Text>
            ) : (
              <>
                {groups.map((g) => (
                  <View key={g.engine} style={s.section}>
                    <Text accessibilityRole="header" style={s.label}>{`MODEL · ${g.engine}`}</Text>
                    {g.models.map((m) => {
                      const key = keyOfModel(m);
                      const current = currentModel != null && currentModel.engine === m.engine && currentModel.selection === m.selection;
                      return (
                        <ModelRow
                          key={key}
                          model={m}
                          current={current}
                          asking={asking === key}
                          pending={waiting === key}
                          held={held}
                          s={s}
                          onChoose={onChoose}
                          onConfirm={onConfirm}
                        />
                      );
                    })}
                  </View>
                ))}
                {modes.length > 0 ? (
                  <View style={s.modes}>
                    <Text accessibilityRole="header" style={s.label}>
                      PERMISSION MODE
                    </Text>
                    <Segmented options={modes} value={currentMode ?? "ask"} onChange={onMode} disabled={held} />
                    <Text style={fullCurrent ? s.warning : s.help}>
                      {waiting?.startsWith("mode:") ? "Changing…" : MODE_HELP[currentMode ?? "ask"]}
                    </Text>
                  </View>
                ) : null}
                {offersFull ? (
                  <View>
                    <Pressable
                      accessibilityRole="button"
                      accessibilityLabel={fullCurrent ? "Full access, current" : "Full access…"}
                      accessibilityHint={FULL_ACCESS_WARNING}
                      accessibilityState={fullCurrent ? { ...ON, disabled: held } : { disabled: held }}
                      disabled={held}
                      onPress={onFull}
                      style={[s.line, held ? s.off : null]}
                    >
                      <View style={s.full}>
                        <Text style={fullCurrent ? s.nameOn : s.text}>{fullCurrent ? "Full access" : "Full access…"}</Text>
                        <Text style={s.warning}>{FULL_ACCESS_WARNING}</Text>
                      </View>
                      {fullCurrent ? <Text style={s.mark}>✓</Text> : null}
                    </Pressable>
                    {armed ? (
                      <View style={s.confirm}>
                        <FullAccessConfirm busy={held} onTurnOn={turnOn} onCancel={cancelFull} />
                      </View>
                    ) : null}
                  </View>
                ) : null}
              </>
            )}
            <Pressable
              accessibilityRole="button"
              accessibilityLabel={`Session info, ${short}`}
              accessibilityState={{ disabled: busy }}
              disabled={busy}
              onPress={info}
              style={s.line}
            >
              <Text style={s.text}>Session info</Text>
              <Text style={s.muted}>{`${short} ›`}</Text>
            </Pressable>
          </ScrollView>
        </View>
      </KeyboardAvoidingView>
    </Modal>
  );
});
