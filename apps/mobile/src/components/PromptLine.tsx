import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Pressable, StyleSheet, Text, TextInput, View, type NativeSyntheticEvent, type TextInputKeyPressEventData } from "react-native";

import { requestMenu } from "../convoMenu";
import { lightTap } from "../haptics";
import type { Engine, Mode, Phase } from "../protocol";
import { octetStore, useDraft, useSent } from "../store";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { KeyBar } from "./KeyBar";
import type { Permission } from "./PermissionCard";
import { PROMPT_LIMIT, SLASH_NOTE, TOO_LONG, isSlash, useAction, useAlive, utf8Bytes, type Send } from "./send";
import { SlashMenu, slashMatches, type SlashCommand } from "./SlashMenu";

/** The prompt's placeholder, by state (the spec's words). */
export const PLACEHOLDER = {
  approval: "Or say what to do instead",
  running: "Working… esc cancels",
  question: "Answer, or describe a change",
  idle: "Ask, describe a change, or /help",
} as const;

export function placeholderFor({ approval, running, question }: { approval: boolean; running: boolean; question: boolean }): string {
  if (approval) return PLACEHOLDER.approval;
  if (running) return PLACEHOLDER.running;
  if (question) return PLACEHOLDER.question;
  return PLACEHOLDER.idle;
}

/** The modes ⇧tab and /mode cycle through. Full access is never among them: it has its own
 * guarded flow in the ▾ menu. */
const CYCLE: readonly Mode[] = ["ask", "accept-edits", "auto"];

/**
 * The mode after `current`: ask → accept-edits → auto → ask, keeping to the modes the session
 * offers (`offered`, from its catalog; null before the catalog, when all three are tried). From
 * full access it goes to the first. Null when there is no other mode to go to.
 */
export function nextMode(current: Mode, offered: readonly Mode[] | null): Mode | null {
  const cycle = CYCLE.filter((m) => offered === null || offered.includes(m));
  if (cycle.length === 0) return null;
  const next = cycle[(cycle.indexOf(current) + 1) % cycle.length];
  return next === undefined || next === current ? null : next;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    notes: { marginHorizontal: 14, marginBottom: spacing.xs, gap: spacing.xs },
    note: { ...type.label, color: theme.muted },
    failure: { flexDirection: "row", alignItems: "center", gap: spacing.md },
    failureText: { ...type.label, color: theme.fg, flex: 1 },
    retry: { minHeight: 44, minWidth: 44, alignItems: "center", justifyContent: "center", paddingHorizontal: spacing.sm },
    retryText: { ...type.body, color: theme.accent },
    off: { opacity: 0.4 },
    line: {
      flexDirection: "row",
      alignItems: "center",
      gap: spacing.sm,
      marginHorizontal: 14,
      marginBottom: spacing.sm,
      minHeight: 44,
      paddingHorizontal: 10,
      borderWidth: 1,
      borderRadius: radius.md,
      borderColor: theme.edge,
    },
    ready: { borderColor: theme.accent },
    caret: { ...type.body, color: theme.accent },
    input: { ...type.body, flex: 1, minWidth: 0, minHeight: 44, paddingVertical: 0, color: theme.fg },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);
const lineLooks = (s: Styles) => ({ ready: [s.line, s.ready], waiting: [s.line] });
const LINES = new Map([
  [DARK, lineLooks(DARK)],
  [LIGHT, lineLooks(LIGHT)],
]);

interface PromptLineProps {
  session: string;
  phase: Phase;
  /** Codex steers a running turn; the others queue a prompt behind it. */
  engine: Engine;
  /** The current mode, and the ones the session offers (its catalog's; null before it comes). */
  mode: Mode;
  modes: readonly Mode[] | null;
  /** The current model's name, for /model's row. */
  model: string | null;
  /** The front approval and its guards, shared with the Permission card. */
  permission: Permission;
  /** Must be stable. */
  send: Send;
  /** The Mac can't be reached: the words stay, and nothing is sent. */
  offline: boolean;
  /** The agent asked the user something (Phase B). */
  question?: boolean;
  /** The safe area under the key bar. */
  bottom: number;
}

/**
 * The prompt line: `>` and one field, the slash menu over it and the key bar under it. What it
 * sends follows the state: a `prompt` (which queues behind a running Claude turn), a `steer` while
 * a Codex turn runs, and, while an approval waits, a deny of the front one and then the text as a
 * prompt once the deny is acked. The text clears only when the Mac acks it, so a refusal or a lost
 * connection never costs the user their words. One command goes at a time; offline, none does.
 * A slash command other than the phone's own (/model, /mode) is the desk's, and is not sent.
 */
export const PromptLine = memo(function PromptLine({
  session,
  phase,
  engine,
  mode,
  modes,
  model,
  permission,
  send,
  offline,
  question = false,
  bottom,
}: PromptLineProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const alive = useAlive();
  const field = useRef<TextInput>(null);
  const [text, setText] = useState("");
  const prompt = useAction(send);
  const control = useAction(send);
  const { run: runPrompt, fail: failPrompt } = prompt;
  const { run: runControl } = control;
  // A deny ahead of a prompt is out: the prompt waits for its ack.
  const [denying, setDenying] = useState(false);
  const denyOut = useRef(false);
  // Read after an await: whether the link is up now.
  const down = useRef(offline);
  useLayoutEffect(() => {
    down.current = offline;
  });

  // A first prompt the Mac did not take comes back into the field, ahead of anything typed,
  // whether it was refused before this screen opened or after.
  const restore = useCallback((words: string) => setText((current) => (current === "" ? words : `${words}\n${current}`)), []);
  const draft = useDraft(session);
  useEffect(() => {
    if (draft === undefined) return;
    const words = octetStore.getState().takeDraft(session);
    if (words !== "") restore(words);
  }, [draft, session, restore]);

  // The mode ⇧tab last set, until the Mac's catalog says what it is.
  const [assumed, setAssumed] = useState<Mode | null>(null);
  const [seenMode, setSeenMode] = useState(mode);
  if (seenMode !== mode) {
    setSeenMode(mode);
    setAssumed(null);
  }
  const currentMode = assumed ?? mode;

  // ↑: how far back the field's text is in what was sent (0 the newest), while it is unchanged.
  const history = useSent(session);
  const [recall, setRecall] = useState<number | null>(null);

  const front = permission.front;
  const { deny: denyFront, allow: allowFront, canAnswer, busy: answering } = permission.answer;
  const running = phase === "running";
  const steers = running && engine === "codex";
  const matches = slashMatches(text);
  const target = matches[0] ?? null;
  const desk = target === null && isSlash(text);
  const busy = prompt.busy || control.busy || denying || answering;
  const blank = text.trim() === "";
  const canSend = !offline && !busy && !blank && !desk;

  const onChange = useCallback(
    (next: string) => {
      setText(next);
      failPrompt(null);
    },
    [failPrompt],
  );

  const sendText = useCallback(
    (kind: "prompt" | "steer", sent: string) => {
      runPrompt({ type: kind, session, text: sent }, () => {
        // Words typed while it was out are the user's: only an unchanged field is cleared.
        setText((current) => (current === sent ? "" : current));
        setRecall(null);
        octetStore.getState().remember(session, sent);
      });
    },
    [runPrompt, session],
  );

  const denyThenPrompt = useCallback(
    async (sent: string) => {
      if (denyOut.current) return;
      denyOut.current = true;
      setDenying(true);
      let denied = false;
      try {
        denied = await denyFront();
      } finally {
        denyOut.current = false;
        if (alive()) setDenying(false);
      }
      // The deny is the Mac's now; the prompt goes only if it took it and the link is still up.
      if (denied && alive() && !down.current) sendText("prompt", sent);
    },
    [denyFront, alive, sendText],
  );

  const cycle = useCallback(() => {
    if (down.current || busy) return;
    const next = nextMode(currentMode, modes);
    if (next === null) return;
    runControl({ type: "set_mode", session, mode: next }, () => setAssumed(next));
  }, [busy, currentMode, modes, runControl, session]);

  const runSlash = useCallback(
    (cmd: SlashCommand["cmd"]) => {
      if (down.current || busy) return;
      setText("");
      if (cmd === "/model") requestMenu(session);
      else cycle();
    },
    [busy, session, cycle],
  );

  const submit = useCallback(() => {
    if (down.current || busy || blank) return;
    const sent = text;
    const hit = slashMatches(sent)[0];
    if (hit) {
      runSlash(hit.cmd);
      return;
    }
    if (isSlash(sent)) return; // the desk's; the note says so
    if (utf8Bytes(sent) > PROMPT_LIMIT) {
      failPrompt({ text: TOO_LONG, retry: false });
      return;
    }
    if (front) {
      // Say what to do instead: the front approval is denied first, through its own guards.
      if (!canAnswer) return;
      lightTap();
      void denyThenPrompt(sent);
      return;
    }
    lightTap();
    sendText(steers ? "steer" : "prompt", sent);
  }, [busy, blank, text, runSlash, failPrompt, front, canAnswer, denyThenPrompt, sendText, steers]);

  const cancel = useCallback(() => {
    if (down.current || busy || !running) return;
    runControl({ type: "cancel", session });
  }, [busy, running, runControl, session]);

  // Where ↑ stands: -1 for an empty field, the entry shown, or null for the user's own words.
  const n = history.length;
  const at = text === "" ? -1 : recall !== null && text === history[n - 1 - recall] ? recall : null;
  const canUp = at !== null && at + 1 < n;
  const up = useCallback(() => {
    if (at === null) return;
    const entry = history[history.length - 2 - at];
    if (entry === undefined) return;
    setText(entry);
    setRecall(at + 1);
    field.current?.focus();
  }, [at, history]);

  const canSlash = text === "";
  const slash = useCallback(() => {
    if (!canSlash) return;
    setText("/");
    field.current?.focus();
  }, [canSlash]);

  const onKeyPress = useCallback(
    (e: NativeSyntheticEvent<TextInputKeyPressEventData>) => {
      // A hardware keyboard's Tab completes the top match.
      if (e.nativeEvent.key === "Tab" && target) setText(target.cmd);
    },
    [target],
  );

  const allow = useCallback(() => void allowFront(), [allowFront]);
  const deny = useCallback(() => void denyFront(), [denyFront]);

  const values = useMemo(() => ({ "/model": model ?? "", "/mode": currentMode }), [model, currentMode]);
  const sendLabel = target ? `Run ${target.cmd}` : steers ? "Steer" : running ? "Queue" : "Send";
  const failed = prompt.failed ?? control.failed;
  const looks = LINES.get(s) ?? lineLooks(s);
  return (
    <View>
      <SlashMenu matches={matches} values={values} disabled={offline} onRun={runSlash} />
      {desk || failed ? (
        <View style={s.notes}>
          {desk ? <Text style={s.note}>{SLASH_NOTE}</Text> : null}
          {failed ? (
            <View style={s.failure} accessibilityLiveRegion="polite">
              <Text style={s.failureText}>{failed.text}</Text>
              {prompt.failed?.retry ? (
                <Pressable
                  accessibilityRole="button"
                  accessibilityLabel="Try again"
                  accessibilityState={{ disabled: offline }}
                  disabled={offline}
                  onPress={submit}
                  style={[s.retry, offline && s.off]}
                >
                  <Text style={s.retryText}>Try again</Text>
                </Pressable>
              ) : null}
            </View>
          ) : null}
        </View>
      ) : null}
      <View style={front || running ? looks.waiting : looks.ready}>
        <Text style={s.caret}>&gt;</Text>
        <TextInput
          ref={field}
          accessibilityLabel="Message"
          placeholder={placeholderFor({ approval: front !== null, running, question })}
          placeholderTextColor={theme.muted}
          value={text}
          onChangeText={onChange}
          onSubmitEditing={submit}
          onKeyPress={onKeyPress}
          submitBehavior="submit"
          returnKeyType="send"
          style={s.input}
        />
      </View>
      <KeyBar
        approval={front !== null}
        offline={offline}
        running={running}
        canCancel={running && !busy}
        canMode={!busy && nextMode(currentMode, modes) !== null}
        canUp={canUp}
        canSlash={canSlash}
        canSend={canSend}
        sendLabel={sendLabel}
        canAnswer={canAnswer}
        bottom={bottom}
        onCancel={cancel}
        onMode={cycle}
        onUp={up}
        onSlash={slash}
        onSend={submit}
        onAllow={allow}
        onDeny={deny}
      />
    </View>
  );
});
