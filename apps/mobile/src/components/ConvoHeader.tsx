import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { AccessibilityInfo, Modal, Pressable, StyleSheet, Text, View } from "react-native";

import { onMenuRequest } from "../convoMenu";
import { openDrawer } from "../drawer";
import type { Phase } from "../protocol";
import { engineModel, headEngine, headLine, headMode } from "../session";
import { clipLabel, sessionName } from "../sessions";
import { useCatalog, useSessionHead } from "../store";
import { cycleTextSize } from "../textSize";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { Button, Label } from "../ui";
import { ModelModeMenu } from "./ModelModeMenu";
import { useAction, type Send } from "./send";

/** The header's height under the safe area. */
export const HEADER_HEIGHT = 44;

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    bar: { height: HEADER_HEIGHT, flexDirection: "row", alignItems: "center", paddingHorizontal: spacing.sm },
    title: { flex: 1, height: HEADER_HEIGHT, alignItems: "center", justifyContent: "center" },
    name: { ...type.body, color: theme.fg },
    bold: { fontWeight: "700" },
    caret: { color: theme.muted },
    sub: { ...type.label, lineHeight: 14, color: theme.muted },
    glyph: { width: 44, minWidth: 44, height: HEADER_HEIGHT, minHeight: HEADER_HEIGHT, alignItems: "center", justifyContent: "center" },
    aa: { ...type.body, color: theme.accent },
    more: { ...type.body, color: theme.fg },
    root: { flex: 1, justifyContent: "flex-end" },
    scrim: { ...StyleSheet.absoluteFill, backgroundColor: dark.bg, opacity: 0.5 },
    sheet: {
      backgroundColor: theme.panel,
      borderTopLeftRadius: radius.lg,
      borderTopRightRadius: radius.lg,
      borderColor: theme.edge,
      borderWidth: 1,
      paddingHorizontal: 20,
      paddingTop: spacing.md,
      paddingBottom: spacing.xxl,
      gap: spacing.md,
    },
    head: { flexDirection: "row", alignItems: "center", justifyContent: "space-between" },
    heading: { ...type.body, fontWeight: "700", color: theme.fg },
    field: { gap: 2 },
    value: { ...type.body, color: theme.fg },
    end: { gap: spacing.sm, borderTopWidth: 1, borderTopColor: theme.edge, paddingTop: spacing.md },
    endRow: { flexDirection: "row", flexWrap: "wrap", alignItems: "center", gap: spacing.sm },
    question: { ...type.body, color: theme.fg },
    failure: { ...type.body, color: theme.fg },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

interface ConvoHeaderProps {
  session: string;
  /** Must be stable. */
  send: Send;
  /** The safe area above the header. */
  top: number;
  /** The session takes commands: the ▾ menu is offered. */
  live: boolean;
  /** The Mac can't be reached: the menu's choices are off. */
  offline: boolean;
  /** The session has an approval waiting. */
  approval: boolean;
  /** Where the session's connection is: End is offered when it is idle. */
  phase?: Phase;
  /** The screen is in front: only then does the prompt line's `/model` open its menu, so a copy
   * of the conversation behind another screen never does. */
  focused: boolean;
  /** Told when the menu or the info sheet opens and closes: what is under them must not be
   * answered meanwhile. Must be stable. */
  onCovered: (covered: boolean) => void;
}

/**
 * The Conversation's header: ☰ (the drawer), the session's name ▾ with "model · mode" under it
 * (the model and mode menu), Aa (the transcript's text size) and ⋯ (Session info). It reads only
 * the session's listed name, engine, model and mode and its catalog, so streamed output never
 * draws it again; Aa changes the size without reading it, so the header does not draw again for
 * that either.
 */
export const ConvoHeader = memo(function ConvoHeader({ session, send, top, live, offline, approval, phase, focused, onCovered }: ConvoHeaderProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const head = useSessionHead(session);
  const catalog = useCatalog(session);

  // The title outlives the session's place in the list.
  const [workspace, setWorkspace] = useState(head.workspace);
  if (head.workspace !== undefined && head.workspace !== workspace) setWorkspace(head.workspace);
  const title = sessionName(workspace);
  const line = headLine(catalog, head);

  const [menu, setMenu] = useState(false);
  // The menu is leaving for the Session info sheet, which opens once its Modal has gone.
  const [toInfo, setToInfo] = useState(false);
  const [info, setInfo] = useState(false);
  // A session that stops taking commands takes its menu with it (and any Face ID still up).
  if (!live && (menu || toInfo)) {
    setMenu(false);
    setToInfo(false);
  }
  useEffect(() => onCovered(menu || toInfo || info), [menu, toInfo, info, onCovered]);

  const toggleMenu = useCallback(() => {
    if (!live) return;
    setToInfo(false);
    setMenu((open) => !open);
  }, [live]);
  // The prompt line's /model opens the menu, on the screen in front only (a session that takes
  // no commands has none: the check above closes it again). Not over Session info: iOS shows one
  // Modal at a time.
  const infoUp = useRef(info);
  useLayoutEffect(() => {
    infoUp.current = info;
  });
  useEffect(() => {
    if (!focused) return;
    return onMenuRequest(session, () => {
      if (infoUp.current) return;
      setToInfo(false);
      setMenu(true);
    });
  }, [session, focused]);
  const closeMenu = useCallback(() => setMenu(false), []);
  const menuToInfo = useCallback(() => {
    setMenu(false);
    setToInfo(true);
  }, []);
  const menuGone = useCallback(() => {
    setToInfo(false);
    setInfo(true);
  }, []);
  const showInfo = useCallback(() => setInfo(true), []);

  // End, in Session info, for an idle session; asked first. A new phase is a new situation: a
  // half-made End means nothing now, and an End the Mac took waits for the session to leave.
  const control = useAction(send);
  const { run } = control;
  const [ending, setEnding] = useState(false);
  const [ended, setEnded] = useState(false);
  const [seen, setSeen] = useState(phase);
  if (seen !== phase) {
    setSeen(phase);
    setEnding(false);
    setEnded(false);
  }
  const hideInfo = useCallback(() => {
    setInfo(false);
    setEnding(false);
  }, []);
  const askEnd = useCallback(() => {
    if (!offline) setEnding(true);
  }, [offline]);
  const keep = useCallback(() => setEnding(false), []);
  const end = useCallback(() => {
    if (offline) return;
    setEnding(false);
    run({ type: "end", session }, () => setEnded(true));
  }, [offline, run, session]);
  const canEnd = live && phase === "idle";
  const endOff = control.busy || ended || offline;

  return (
    <View style={{ paddingTop: top }}>
      <View style={s.bar}>
        <Button label="☰" accessibilityLabel="Menu" variant="bare" onPress={openDrawer} />
        <Pressable
          testID="convo-title"
          accessibilityRole="button"
          accessibilityLabel={`${clipLabel(title)}, ${clipLabel(line)}`}
          accessibilityHint={live ? "Changes the model and mode" : undefined}
          accessibilityState={live ? { expanded: menu } : { disabled: true }}
          disabled={!live}
          onPress={toggleMenu}
          style={s.title}
        >
          <Text numberOfLines={1} style={s.name}>
            <Text style={s.bold}>{title}</Text>
            {live ? <Text style={s.caret}>{menu ? " ▴" : " ▾"}</Text> : null}
          </Text>
          <Text numberOfLines={1} style={s.sub}>
            {line}
          </Text>
        </Pressable>
        <Pressable accessibilityRole="button" accessibilityLabel="Text size" onPress={cycleSize} style={s.glyph}>
          <Text style={s.aa}>Aa</Text>
        </Pressable>
        <Pressable accessibilityRole="button" accessibilityLabel="Session info" onPress={showInfo} style={s.glyph}>
          <Text style={s.more}>⋯</Text>
        </Pressable>
      </View>
      {live && (menu || toInfo) ? (
        <ModelModeMenu
          session={session}
          engine={headEngine(catalog, head)}
          catalog={catalog}
          send={send}
          offline={offline}
          approval={approval}
          top={top + HEADER_HEIGHT}
          visible={menu}
          onDismissed={menuGone}
          onClose={closeMenu}
          onInfo={menuToInfo}
        />
      ) : null}
      {info ? (
        <Modal testID="info" visible transparent animationType="slide" onRequestClose={hideInfo}>
          <View style={s.root}>
            <View style={s.scrim} />
            <Pressable testID="info-scrim" accessible={false} onPress={hideInfo} style={StyleSheet.absoluteFill} />
            <View style={s.sheet}>
              <View style={s.head}>
                <Text accessibilityRole="header" style={s.heading}>
                  Session info
                </Text>
                <Button label="Close" onPress={hideInfo} />
              </View>
              <Field label="Engine / model" value={engineModel(catalog, head)} />
              <Field label="Mode" value={headMode(catalog, head)} />
              <Field label="Workspace" value={workspace ?? ""} />
              <Field label="Session" value={session} />
              {canEnd ? (
                <View style={s.end}>
                  {ending ? (
                    <View style={s.endRow}>
                      <Text style={s.question}>End this session?</Text>
                      <Button label="Yes, end it" variant="accent" onPress={end} disabled={offline} />
                      <Button label="Keep it" onPress={keep} />
                    </View>
                  ) : (
                    <View style={s.endRow}>
                      <Button
                        label={ended ? "Ending…" : "End session"}
                        onPress={askEnd}
                        disabled={endOff}
                        busy={control.busy || ended}
                      />
                    </View>
                  )}
                  {control.failed ? (
                    <View accessibilityLiveRegion="polite">
                      <Text style={s.failure}>{control.failed.text}</Text>
                    </View>
                  ) : null}
                </View>
              ) : null}
            </View>
          </View>
        </Modal>
      ) : null}
    </View>
  );
});

/** Aa: the next text size, said aloud (the button's label stays, so the header does not draw again). */
function cycleSize() {
  const size = cycleTextSize();
  AccessibilityInfo.announceForAccessibility?.(`Text size ${size}`);
}

function Field({ label, value }: { label: string; value: string }) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  return (
    <View style={s.field}>
      <Label>{label}</Label>
      <Text selectable style={s.value}>
        {value}
      </Text>
    </View>
  );
}
