import { Stack, useIsFocused, useRouter } from "expo-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { KeyboardAvoidingView, Platform, Pressable, StyleSheet, Text, TextInput, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { FullAccessConfirm } from "../src/components/FullAccessConfirm";
import { FULL_ACCESS_WORDS, useFullAccessArm } from "../src/components/fullAccess";
import { HubBanner } from "../src/components/HubBanner";
import { NewChips } from "../src/components/NewChips";
import { failure, isSlash, PROMPT_LIMIT, SLASH_NOTE, TOO_LONG, utf8Bytes } from "../src/components/send";
import { lightTap } from "../src/haptics";
import { useMacs } from "../src/hub";
import type { Engine, Mode } from "../src/protocol";
import { modelsFor } from "../src/recent";
import { sessionName } from "../src/sessions";
import { NO_WORKSPACE, useStartSession, useWorkspaceChoices, type StartBody } from "../src/start";
import { octetStore, useCatalogs, useHub } from "../src/store";
import { spacing, type, useTheme } from "../src/theme";
import { showToast } from "../src/toast";

const BIOMETRIC_REASON = "Start a session with full access";
const EMPTY_PROMPT = "Write a prompt first.";
const LOADING_FOLDERS = "Loading the Mac's folders…";
const KEPT_TEXT = "Your text is in the prompt.";
const ARMED_HINT = `Type ${FULL_ACCESS_WORDS} below to start.`;

const ENGINES: readonly Engine[] = ["claude", "codex"];
/** What a tap on the mode chip cycles through, null (the Mac's default) first: full access is
 * never one of these. */
const MODES: readonly (Mode | null)[] = [null, "ask", "accept-edits", "auto"];

/** Why `text` cannot be the first prompt, or null. */
function refusal(text: string): string | null {
  if (text.trim() === "") return EMPTY_PROMPT;
  if (isSlash(text)) return SLASH_NOTE;
  if (utf8Bytes(text) > PROMPT_LIMIT) return TOO_LONG;
  return null;
}

/**
 * A new session: write the first prompt, choose where and how it runs, and Start. The Mac starts
 * the session; when it is listed the prompt goes to it and the conversation opens. Full access
 * asks for Face ID and then the typed words before it sends anything, every time.
 */
export default function NewSession() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const focused = useIsFocused();
  // Where the session starts: the Mac chosen on the chip, while it is online; else the first
  // online Mac; else the first paired. With one Mac, that one.
  const macs = useMacs();
  const [chosen, setChosen] = useState<string | null>(null);
  const online = useMemo(() => macs.filter((m) => m.state === "ready"), [macs]);
  const target = online.find((m) => m.id === chosen) ?? online[0] ?? macs[0];
  const targetId = target?.id;
  const targetName = target?.macName ?? "Mac";
  const hub = useHub(targetId ?? "");
  const catalogs = useCatalogs(targetId);
  const { choices: workspaces, loaded: foldersLoaded } = useWorkspaceChoices(targetId);
  const [text, setText] = useState("");
  const [engine, setEngine] = useState<Engine>("claude");
  const [folder, setFolder] = useState<string | null>(null);
  const [mode, setMode] = useState<Mode | null>(null);
  // The prompt to send when the session shows: what the user started with.
  const pending = useRef("");

  const sendPrompt = useRef<(id: string, text: string) => void>(() => undefined);
  // The first prompt goes to the launched session, and the conversation opens whether or not the
  // Mac takes it. A refusal is a toast in the hub's words, and the text goes back to that
  // session's prompt box.
  const listed = useCallback(
    (id: string) => {
      sendPrompt.current(id, pending.current);
      router.replace({ pathname: "/session/[id]", params: { id } });
    },
    [router],
  );
  const { send, start, failed, fail, locked, notice } = useStartSession(listed, targetId);
  useEffect(() => {
    sendPrompt.current = (id, prompt) => {
      send({ type: "prompt", session: id, text: prompt }).catch((error: unknown) => {
        octetStore.getState().setDraft(id, prompt);
        showToast(`${failure(error).text.replace(/\.$/, "")}. ${KEPT_TEXT}`);
      });
    };
  }, [send]);

  // A refused start (or a phone with no Face ID) is a toast.
  useEffect(() => {
    if (failed) showToast(failed.text);
  }, [failed]);

  // What will be sent: a workspace that has left the list is replaced by the first, and the
  // full-access approval goes with it (it is part of its key).
  const workspace = folder !== null && workspaces.includes(folder) ? folder : (workspaces[0] ?? null);
  const model = modelsFor(catalogs, engine)[0] ?? null;
  const selection = model?.selection ?? null;

  const full = useFullAccessArm({
    reason: BIOMETRIC_REASON,
    key: JSON.stringify(["new", targetId, engine, workspace, selection, "full-access"]),
    failed,
    fail,
  });
  const { armed, arm, disarm, checking } = full;

  const bodyOf = useCallback(
    (full: boolean): StartBody | null => {
      if (workspace === null) return null;
      return {
        type: "start",
        engine,
        workspace,
        ...(selection !== null ? { model: selection } : {}),
        // No mode unless one was chosen: the Mac's own default stands.
        ...(full ? { mode: "full-access" as const, confirm: FULL_ACCESS_WORDS } : mode !== null ? { mode } : {}),
      };
    },
    [engine, workspace, selection, mode],
  );

  /** Checks the prompt and the workspace, then starts. Nothing is spent unless it sends. */
  const begin = useCallback(
    (fullAccess: boolean): boolean => {
      const why = refusal(text);
      if (why !== null) {
        showToast(why);
        return false;
      }
      const body = bodyOf(fullAccess);
      if (body === null) {
        showToast(foldersLoaded ? NO_WORKSPACE : LOADING_FOLDERS);
        return false;
      }
      if (!start(body)) return false; // a start is already out: this press is a second tap
      pending.current = text;
      lightTap();
      return true;
    },
    [text, bodyOf, start, foldersLoaded],
  );

  const onStart = useCallback(() => {
    if (locked || checking()) return;
    if (armed) {
      showToast(ARMED_HINT);
      return;
    }
    begin(false);
  }, [locked, checking, armed, begin]);
  const turnOn = useCallback(() => {
    if (locked || !armed) return;
    if (begin(true)) disarm(); // sent: the approval is spent
  }, [locked, armed, begin, disarm]);
  const cancelFull = useCallback(() => {
    disarm();
    fail(null);
  }, [disarm, fail]);

  // With several Macs online, a tap moves to the next online one; otherwise it says where
  // sessions start.
  const onMac = useCallback(() => {
    if (online.length < 2) {
      showToast(`Sessions start on ${targetName}.`);
      return;
    }
    disarm();
    const at = online.findIndex((m) => m.id === targetId);
    setChosen(online[(at + 1) % online.length]?.id ?? null);
  }, [online, targetId, targetName, disarm]);
  const onFolder = useCallback(() => {
    if (workspaces.length === 0) {
      showToast(foldersLoaded ? NO_WORKSPACE : LOADING_FOLDERS);
      return;
    }
    disarm();
    const at = workspace === null ? -1 : workspaces.indexOf(workspace);
    setFolder(workspaces[(at + 1) % workspaces.length] ?? null);
  }, [workspaces, workspace, disarm, foldersLoaded]);
  const onEngine = useCallback(() => {
    disarm();
    setEngine((e) => ENGINES[(ENGINES.indexOf(e) + 1) % ENGINES.length] ?? "claude");
  }, [disarm]);
  const onMode = useCallback(() => {
    disarm();
    setMode((m) => MODES[(MODES.indexOf(m) + 1) % MODES.length] ?? null);
  }, [disarm]);
  // Face ID asks for exactly this form; a phone that is offline or busy asks nothing.
  const onFullAccess = useCallback(() => {
    if (locked || checking()) return;
    fail(null);
    disarm();
    void arm();
  }, [locked, checking, fail, disarm, arm]);

  const cancel = useCallback(() => {
    if (router.canGoBack()) router.back();
    else router.replace("/");
  }, [router]);
  // Resume takes New's place, so back from the resumed session (which takes Recent's) is the Inbox.
  const resume = useCallback(() => router.replace("/recent"), [router]);

  const engineModel = `${engine} / ${model?.name ?? "default"}`;
  const reachable = hub === "ready";
  const startOff = locked;
  const hasText = text.trim() !== "";
  const startColor = hasText && !startOff ? theme.accent : theme.muted;
  const styles = useMemo(() => makeStyles(insets.top, insets.bottom), [insets.top, insets.bottom]);

  return (
    <KeyboardAvoidingView
      behavior={Platform.OS === "ios" ? "padding" : undefined}
      style={[styles.screen, { backgroundColor: theme.bg }]}
    >
      <Stack.Screen options={{ title: "New session", headerShown: false }} />
      <View style={styles.header}>
        <Pressable accessibilityRole="button" accessibilityLabel="Cancel" onPress={cancel} style={styles.side}>
          <Text style={[type.body, { color: theme.muted }]}>Cancel</Text>
        </Pressable>
        <Text accessibilityRole="header" style={[type.body, styles.title, { color: theme.fg }]}>
          New session
        </Text>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Start session"
          accessibilityState={{ disabled: startOff, busy: locked }}
          disabled={startOff}
          onPress={onStart}
          style={styles.side}
        >
          <Text style={[type.body, styles.start, { color: startColor }]}>Start ↑</Text>
        </Pressable>
      </View>
      <HubBanner state={hub} announce={focused} />
      <TextInput
        accessibilityLabel="What should the agent do?"
        placeholder="What should the agent do?"
        placeholderTextColor={theme.muted}
        multiline
        autoFocus
        spellCheck={false}
        textAlignVertical="top"
        selectionColor={theme.accent}
        cursorColor={theme.accent}
        value={text}
        onChangeText={setText}
        style={[type.body, styles.field, { color: theme.fg }]}
      />
      {notice ? (
        <Text accessibilityLiveRegion="polite" style={[type.body, styles.notice, { color: theme.muted }]}>
          {notice}
        </Text>
      ) : null}
      {armed ? <FullAccessConfirm busy={locked} onTurnOn={turnOn} onCancel={cancelFull} /> : null}
      <NewChips
        mac={targetName}
        online={reachable}
        folder={workspace === null ? "" : sessionName(workspace)}
        loading={workspace === null && !foldersLoaded}
        engineModel={engineModel}
        mode={armed ? "full access" : (mode ?? "default mode")}
        fullAccess={armed}
        onMac={onMac}
        onFolder={onFolder}
        onEngine={onEngine}
        onMode={onMode}
        onFullAccess={onFullAccess}
      />
      <Pressable accessibilityRole="button" accessibilityLabel="Resume a past session" onPress={resume} style={styles.resume}>
        <Text style={[type.body, { color: theme.muted }]}>Resume a past session ›</Text>
      </Pressable>
    </KeyboardAvoidingView>
  );
}

function makeStyles(top: number, bottom: number) {
  return StyleSheet.create({
    screen: { flex: 1, paddingTop: top },
    header: {
      height: 48,
      flexDirection: "row",
      alignItems: "center",
      justifyContent: "space-between",
      paddingHorizontal: spacing.md,
    },
    side: { minHeight: 44, minWidth: 44, justifyContent: "center", paddingHorizontal: spacing.sm },
    title: { fontWeight: "700" },
    start: { fontWeight: "700" },
    field: { flex: 1, paddingHorizontal: 20, paddingVertical: spacing.lg },
    notice: { paddingHorizontal: spacing.lg, paddingBottom: spacing.sm },
    resume: {
      minHeight: 44,
      justifyContent: "center",
      paddingHorizontal: spacing.lg + spacing.xs,
      marginBottom: Math.max(bottom, spacing.lg),
    },
  });
}
