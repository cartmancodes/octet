import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Pressable, StyleSheet, Text, TextInput, View, type NativeSyntheticEvent, type TextInputKeyPressEventData } from "react-native";

import { requestMenu } from "../convoMenu";
import { lightTap } from "../haptics";
import type { CommandName, Engine, Mode, Phase } from "../protocol";
import { shortId } from "../session";
import { octetStore, useDraft, useReplying, useSent } from "../store";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { CommandSheet, FreshConfirm } from "./CommandSheet";
import { useDeskCommand } from "./deskCommand";
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

/** Said beside the prompt for a return in the 400 ms after an approval came: nothing was sent. */
export const JUST_APPEARED = "Wait a moment — the approval just appeared";
/** The field grows to this many lines, then scrolls. */
export const PROMPT_LINES = 4;
/** The field's room above and below its text: one line of it is 44 pt tall. */
export const PROMPT_PAD = 12;
export const PROMPT_MAX_HEIGHT = PROMPT_LINES * type.body.lineHeight + 2 * PROMPT_PAD;

/** The hub's `command` name of each slash row that runs a desk handler (/new, which asks first,
 * goes through `startFresh`). */
const DESK_COMMAND = {
  "/session": "session",
  "/export": "export",
  "/help": "help",
} as const satisfies Readonly<Partial<Record<SlashCommand["cmd"], CommandName>>>;

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
      alignItems: "flex-start",
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
    caret: { ...type.body, paddingTop: PROMPT_PAD, color: theme.accent },
    input: {
      ...type.body,
      flex: 1,
      minWidth: 0,
      minHeight: type.body.lineHeight + 2 * PROMPT_PAD,
      maxHeight: PROMPT_MAX_HEIGHT,
      paddingTop: PROMPT_PAD,
      paddingBottom: PROMPT_PAD,
      textAlignVertical: "top",
      color: theme.fg,
    },
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
  /** The agent's last turn ended on a question: the placeholder asks for an answer. */
  question?: boolean;
  /** The safe area under the key bar. */
  bottom: number;
  /** Told when a sheet this line opened (a command's text, the /new ask) goes up and down: an
   * approval under it must not be answered meanwhile. Must be stable. */
  onCovered?: (covered: boolean) => void;
}

/**
 * The prompt line: `>` and a field of up to four lines (then it scrolls), the slash menu over it
 * and the key bar under it. What it
 * sends follows the state: a `prompt` (which queues behind a running Claude turn), a `steer` while
 * a Codex turn runs, and, while an approval waits, a deny of the front one and then the text as a
 * prompt once the deny is acked — only for the approval that was at the front when the user sent,
 * and only if no other approval is at the front by then. The text clears only when the Mac acks it, so a refusal or a lost
 * connection never costs the user their words. One command goes at a time; offline, none does.
 * The phone's own slash commands are /model and /mode, and the desk's four safe ones (/new,
 * /session, /export, /help) through the hub's `command` request; any other is the desk's alone,
 * and is not sent.
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
  onCovered,
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
  // The desk's commands (/new, /session, /export, /help): one at a time, like the others.
  const desk = useDeskCommand(session, send);
  const { run: runDesk, fail: failDesk, closeSheet } = desk;
  // The ask before /new is up.
  const [asking, setAsking] = useState(false);
  const covering = asking || desk.sheet !== null;
  useEffect(() => {
    onCovered?.(covering);
    return () => onCovered?.(false);
  }, [covering, onCovered]);
  // A deny ahead of a prompt is out: the prompt waits for its ack.
  const [denying, setDenying] = useState(false);
  const denyOut = useRef(false);
  // Which of the control's commands is out, for its key's busy state.
  const [controlKind, setControlKind] = useState<"cancel" | "mode" | null>(null);
  // The last prompt or steer sent, as it went: Try again sends exactly this.
  const lastBody = useRef<{ kind: "prompt" | "steer"; text: string } | null>(null);
  // The approvals waiting, front first, as of the last commit: read after the deny's await.
  const waitingNow = useRef<readonly number[]>(permission.waiting);
  // Read after an await: whether the link is up now.
  const down = useRef(offline);
  useLayoutEffect(() => {
    down.current = offline;
    waitingNow.current = permission.waiting;
  });

  // A first prompt the Mac did not take comes back into the field, ahead of anything typed,
  // whether it was refused before this screen opened or after.
  const restore = useCallback((words: string) => setText((current) => (current === "" ? words : `${words}\n${current}`)), []);
  // The Inbox's Reply: the field is focused as the conversation opens (`autoFocus`, from what was
  // asked when it mounted) or, when it is already open under another screen, as the ask comes.
  // The ask is spent once used, so that a second Reply focuses again; and when this goes.
  const replying = useReplying(session);
  const [focusOnMount] = useState(replying);
  useEffect(() => {
    if (!replying) return;
    field.current?.focus();
    octetStore.getState().setReplyTo(null);
  }, [replying]);
  useEffect(
    () => () => {
      if (octetStore.getState().replyTo === session) octetStore.getState().setReplyTo(null);
    },
    [session],
  );
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
  const { deny: denyFront, allow: allowFront, canAnswer, armed, busy: answering } = permission.answer;
  // A return before the approval armed: the request it was for, so the note goes with it, and
  // once the arm passes.
  const [early, setEarly] = useState<string | null>(null);
  const tooEarly = early !== null && early === permission.look && !armed;
  // Once armed the note has done its work: it does not come back when the arm is lost and
  // regained (the link dropped), since nothing was sent in between.
  if (early !== null && armed) setEarly(null);
  const running = phase === "running";
  const steers = running && engine === "codex";
  const matches = slashMatches(text);
  const target = matches[0] ?? null;
  const deskNote = target === null && isSlash(text);
  const busy = prompt.busy || control.busy || desk.busy || denying || answering;
  const blank = text.trim() === "";
  const canSend = !offline && !busy && !blank && !deskNote;

  const onChange = useCallback(
    (next: string) => {
      setText(next);
      failPrompt(null);
      failDesk(null);
    },
    [failPrompt, failDesk],
  );

  const sendText = useCallback(
    (kind: "prompt" | "steer", sent: string) => {
      lastBody.current = { kind, text: sent };
      runPrompt({ type: kind, session, text: sent }, () => {
        // Words typed while it was out are the user's: only an unchanged field is cleared.
        setText((current) => (current === sent ? "" : current));
        setRecall(null);
        octetStore.getState().remember(session, sent);
        // Whatever it said, the user has answered: the question goes until the Mac asks again.
        octetStore.getState().markAnswered(session);
      });
    },
    [runPrompt, session],
  );

  const denyThenPrompt = useCallback(
    async (sent: string, pinned: number) => {
      // Only the approval at the front when the user sent: if another is there now, nothing goes.
      if (denyOut.current || waitingNow.current[0] !== pinned) return;
      denyOut.current = true;
      setDenying(true);
      let denied = false;
      try {
        denied = await denyFront();
      } finally {
        denyOut.current = false;
        if (alive()) setDenying(false);
      }
      // The deny is the Mac's now. The prompt goes only if it took it, the link is still up, and no
      // other approval is at the front by now (one behind it, or one that came meanwhile): the
      // words stay for the user to send once they have seen it.
      if (!denied || !alive() || down.current) return;
      if (waitingNow.current.some((id) => id !== pinned)) return;
      sendText("prompt", sent);
    },
    [denyFront, alive, sendText],
  );

  /** True if it sent a mode. */
  const cycle = useCallback((): boolean => {
    if (down.current || busy) return false;
    const next = nextMode(currentMode, modes);
    if (next === null) return false;
    setControlKind("mode");
    return runControl({ type: "set_mode", session, mode: next }, () => setAssumed(next));
  }, [busy, currentMode, modes, runControl, session]);
  const cycleKey = useCallback(() => void cycle(), [cycle]);

  // The slash menu's commands wait while an approval does (the keys are its) and while a command
  // is out; the words clear only when the command ran.
  const slashOff = offline || busy || front !== null || asking;
  // The desk's commands clear the words when the Mac acks, if they are still what was run: a
  // refusal keeps them, with its words, and words typed meanwhile are the user's.
  const shown = useRef(text);
  useLayoutEffect(() => {
    shown.current = text;
  });
  const runSlash = useCallback(
    (cmd: SlashCommand["cmd"]) => {
      if (down.current || slashOff) return;
      switch (cmd) {
        case "/model":
        case "/mode":
          if (cmd === "/model" ? requestMenu(session) : cycle()) setText("");
          return;
        case "/new":
          setAsking(true); // nothing goes until the user says Start fresh
          return;
        case "/session":
        case "/export":
        case "/help": {
          const ran = shown.current;
          runDesk(DESK_COMMAND[cmd], () => setText((current) => (current === ran ? "" : current)));
          return;
        }
      }
    },
    [slashOff, session, cycle, runDesk],
  );
  const cancelFresh = useCallback(() => setAsking(false), []);
  const startFresh = useCallback(() => {
    setAsking(false);
    if (down.current) return;
    const ran = shown.current;
    runDesk("new", () => setText((current) => (current === ran ? "" : current)));
  }, [runDesk]);

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
      // Say what to do instead: the front approval is denied first, through its own guards. Too
      // soon after it came, nothing goes, and the note says why.
      if (!canAnswer) {
        if (!armed) setEarly(permission.look);
        return;
      }
      lightTap();
      void denyThenPrompt(sent, front.id);
      return;
    }
    lightTap();
    sendText(steers ? "steer" : "prompt", sent);
  }, [busy, blank, text, runSlash, failPrompt, front, canAnswer, armed, permission.look, denyThenPrompt, sendText, steers]);

  const cancel = useCallback(() => {
    if (down.current || busy || !running) return;
    setControlKind("cancel");
    runControl({ type: "cancel", session });
  }, [busy, running, runControl, session]);

  // Try again: exactly the body that failed, never a deny, and not past an approval at the front.
  const retryOff = offline || busy || front !== null;
  const retry = useCallback(() => {
    const body = lastBody.current;
    if (down.current || retryOff || !body) return;
    lightTap();
    sendText(body.kind, body.text);
  }, [retryOff, sendText]);

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

  const values = useMemo(() => ({ "/model": model ?? "", "/mode": currentMode, "/session": shortId(session) }), [model, currentMode, session]);
  const sendLabel = target ? `Run ${target.cmd}` : steers ? "Steer" : running ? "Queue" : "Send";
  const failed = prompt.failed ?? control.failed ?? desk.failed;
  const looks = LINES.get(s) ?? lineLooks(s);
  return (
    <View>
      <SlashMenu matches={matches} values={values} disabled={slashOff} onRun={runSlash} />
      {deskNote || failed || tooEarly ? (
        <View style={s.notes}>
          {deskNote ? <Text style={s.note}>{SLASH_NOTE}</Text> : null}
          {tooEarly ? (
            <Text accessibilityLiveRegion="polite" style={s.note}>
              {JUST_APPEARED}
            </Text>
          ) : null}
          {failed ? (
            <View style={s.failure} accessibilityLiveRegion="polite">
              <Text style={s.failureText}>{failed.text}</Text>
              {prompt.failed?.retry ? (
                <Pressable
                  accessibilityRole="button"
                  accessibilityLabel="Try again"
                  accessibilityState={{ disabled: retryOff }}
                  disabled={retryOff}
                  onPress={retry}
                  style={[s.retry, retryOff && s.off]}
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
          autoFocus={focusOnMount}
          multiline
          scrollEnabled
          submitBehavior="submit"
          returnKeyType="send"
          style={s.input}
        />
      </View>
      {asking ? <FreshConfirm offline={offline} onStart={startFresh} onCancel={cancelFresh} /> : null}
      {desk.sheet ? <CommandSheet title={desk.sheet.title} text={desk.sheet.text} onClose={closeSheet} /> : null}
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
        cancelBusy={control.busy && controlKind === "cancel"}
        modeBusy={control.busy && controlKind === "mode"}
        sendBusy={prompt.busy || denying}
        answerBusy={answering}
        bottom={bottom}
        onCancel={cancel}
        onMode={cycleKey}
        onUp={up}
        onSlash={slash}
        onSend={submit}
        onAllow={allow}
        onDeny={deny}
      />
    </View>
  );
});
