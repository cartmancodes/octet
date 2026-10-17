import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { AccessibilityInfo, Platform, Pressable, ScrollView, StyleSheet, Text, View } from "react-native";

import { confirmBiometric } from "../auth";
import { successTap } from "../haptics";
import { Mascot } from "../mascot/Mascot";
import { poseFor } from "../mascot/state";
import { foldCommand } from "../model/command";
import type { PendingApproval } from "../model/session";
import { loadSettings } from "../persist";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { failure, useAlive, type Send } from "./send";

/** Allow and Deny stay dead this long after a request comes to the front: a tap meant for the one
 * before (or a thumb already on its way) must not answer the next. */
export const ARM_MS = 400;
/** Files listed before "+N more". */
export const FILES_SHOWN = 5;
export const NO_FACE_ID = "Face ID isn't set up on this phone. Turn it off in Settings to allow from here.";
/** What VoiceOver says when a request comes to the front. */
export const ANNOUNCED = "Approval waiting";
/** The system prompt's sentence is short; the action can be long. */
const REASON_CHARS = 80;
/** Said when the command or the request is not whole on the phone: Allow runs the Mac's. */
export const SHORTENED = "Shortened — the full command is on the Mac. Check the desk before allowing.";
/** A command longer than this many lines says how many it has. */
const COUNT_PAST = 3;

interface ApprovalSheetProps {
  session: string;
  /** The session's pending approvals, oldest first. The store fills them in place. */
  approvals: PendingApproval[];
  /** Must be stable. */
  send: Send;
  /** The Mac can't be reached: the request stays on screen and nothing can be answered. */
  offline?: boolean;
  /** Another sheet (Mode and model) is over this one: nothing can be answered, and the arm
   * delay starts again once it is gone, so a tap that closed it answers nothing. */
  covered?: boolean;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    sheet: {
      backgroundColor: theme.panel,
      borderTopColor: theme.amber,
      borderTopWidth: 2,
      paddingHorizontal: spacing.lg,
      paddingVertical: spacing.md,
      gap: spacing.sm,
    },
    head: { flexDirection: "row", alignItems: "center", gap: spacing.md },
    headText: { flex: 1, gap: 2 },
    kicker: { ...type.caption, color: theme.amber, fontWeight: "700" },
    action: { ...type.body, color: theme.amber, fontWeight: "600" },
    waiting: { ...type.caption, color: theme.muted },
    body: { maxHeight: 220 },
    bodyContent: { gap: spacing.xs },
    command: { ...type.mono, color: theme.fg },
    shortened: { ...type.caption, color: theme.amber, fontWeight: "600" },
    file: { ...type.mono, color: theme.fg },
    more: { ...type.caption, color: theme.muted },
    where: { ...type.caption, color: theme.muted },
    scope: { ...type.caption, color: theme.fg },
    detail: { ...type.mono, color: theme.muted },
    toggle: { minHeight: 44, justifyContent: "center", alignSelf: "flex-start" },
    toggleText: { ...type.body, color: theme.fg, textDecorationLine: "underline" },
    status: { ...type.caption, color: theme.muted },
    error: { ...type.caption, color: theme.amber },
    buttons: { flexDirection: "row", gap: spacing.md },
    button: {
      flex: 1,
      minHeight: 48,
      alignItems: "center",
      justifyContent: "center",
      borderRadius: radius.md,
      borderColor: theme.edge,
      borderWidth: 1,
    },
    allow: { backgroundColor: theme.accent, borderColor: theme.accent },
    off: { opacity: 0.4 },
    denyLabel: { ...type.body, color: theme.fg, fontWeight: "600" },
    allowLabel: { ...type.body, color: theme.bg, fontWeight: "600" },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

const POSE = poseFor({ kind: "approval" });

/**
 * Pending approvals, one at a time: the oldest comes first, the others wait behind it. A request
 * another device closed disappears (the store drops it); one answered here goes at once. The sheet
 * is the only amber thing on the screen. The screen does not mount it for a session that ended.
 */
export const ApprovalSheet = memo(function ApprovalSheet({
  session,
  approvals,
  send,
  offline = false,
  covered = false,
}: ApprovalSheetProps) {
  // Answered here and not closed by the Mac yet: hidden. Only ids still listed are kept, so an id
  // that left and came back is a new request (a vendor numbers its approvals from 1 again after a
  // provider switch or a reconnect), shown and answerable.
  const [answered, setAnswered] = useState<readonly number[]>([]);
  const listed = answered.filter((id) => approvals.some((a) => a.id === id));
  if (listed.length !== answered.length) setAnswered(listed);
  const onAnswered = useCallback((id: number) => setAnswered((prev) => (prev.includes(id) ? prev : [...prev, id])), []);
  // The ids still open, for a request to ask after Face ID: the store may have closed it meanwhile.
  // A layout effect, not a passive one: it runs in the commit itself, so a close is visible at
  // once to an Allow that is awaiting Face ID, with no frame in between.
  const open = useRef<readonly number[]>([]);
  useLayoutEffect(() => {
    open.current = approvals.map((a) => a.id);
  });
  const isOpen = useCallback((id: number) => open.current.includes(id), []);
  const waiting = approvals.filter((a) => !listed.includes(a.id));
  const front = waiting[0];
  // A new request at the front is news. Android speaks the live region; iOS has none, so it is
  // announced there (both would speak twice). Its record filling in later is the same request.
  const frontId = front?.id;
  useEffect(() => {
    if (frontId !== undefined && Platform.OS === "ios") AccessibilityInfo.announceForAccessibility(ANNOUNCED);
  }, [frontId]);
  if (!front) return null;
  // Keyed by the id and what it asks: each request, and the same one once its content is filled
  // in (the journal record after the state frame), starts unarmed, closed and with no old error.
  return (
    <Request
      key={`${front.id}:${JSON.stringify(front.summary)}`}
      isOpen={isOpen}
      session={session}
      approval={front}
      more={waiting.length - 1}
      send={send}
      offline={offline}
      covered={covered}
      onAnswered={onAnswered}
    />
  );
});

interface RequestProps {
  session: string;
  approval: PendingApproval;
  more: number;
  send: Send;
  offline: boolean;
  covered: boolean;
  /** Whether the approval is still open: it may have been closed elsewhere while Face ID was up. */
  isOpen: (id: number) => boolean;
  onAnswered: (id: number) => void;
}

function Request({ session, approval, more, send, offline, covered, isOpen, onAnswered }: RequestProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const alive = useAlive();
  const [armed, setArmed] = useState(false);
  const [full, setFull] = useState(false);
  // "checking": the biometric prompt is up. "sending": the answer is out, waiting for its ack.
  const [stage, setStage] = useState<"checking" | "sending" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const out = useRef(false);
  // For an answer that awaited Face ID: the link may have gone down meanwhile. A layout effect,
  // like `open` above, so the commit that takes the link away is seen at once.
  const down = useRef(offline);
  useLayoutEffect(() => {
    down.current = offline;
  });

  // The delay counts from when the request can be seen and answered: its arrival, the link
  // coming back, or the sheet over it closing. Hidden or offline, it is dead.
  const reachable = !offline && !covered;
  if (!reachable && armed) setArmed(false);
  useEffect(() => {
    if (!reachable) return;
    const timer = setTimeout(() => setArmed(true), ARM_MS);
    return () => clearTimeout(timer);
  }, [reachable]);

  const { summary, detail, id } = approval;
  const answer = useCallback(
    async (allow: boolean) => {
      if (out.current || !armed || down.current) return;
      out.current = true;
      setError(null);
      setStage("checking");
      try {
        if (allow) {
          // Read now, not at mount: a change in Settings counts at once.
          const { faceId } = await loadSettings();
          if (faceId) {
            const reason = `Allow: ${summary.action.slice(0, REASON_CHARS)}`;
            const checked = await confirmBiometric(reason);
            if (checked === "unavailable") {
              if (alive()) setError(NO_FACE_ID);
              return;
            }
            if (checked === "failed") return; // cancelled or no match: nothing is sent
          }
        }
        // Face ID takes a while: send only if this request is still on screen, still open, and so
        // still the one the user looked at (the screen takes the sheet away once the session ends),
        // and the Mac can still be reached.
        if (!alive() || !isOpen(id) || down.current) return;
        setStage("sending");
        await send({ type: "answer", session, approval: id, allow });
        if (allow) successTap();
        onAnswered(id);
      } catch (e) {
        if (alive()) setError(failure(e).text);
      } finally {
        out.current = false;
        if (alive()) setStage(null);
      }
    },
    [armed, summary.action, session, id, send, isOpen, onAnswered, alive],
  );
  const allow = useCallback(() => void answer(true), [answer]);
  const deny = useCallback(() => void answer(false), [answer]);
  const toggle = useCallback(() => setFull((open) => !open), []);

  const busy = stage !== null;
  const canAnswer = armed && !busy && reachable;
  const files = summary.files.slice(0, FILES_SHOWN);
  const rest = summary.files.length - files.length;
  const command = summary.command ? foldCommand(summary.command) : null;
  return (
    <View style={s.sheet} accessibilityLiveRegion="polite">
      <View style={s.head}>
        <Mascot pose={POSE} size={59} />
        <View style={s.headText}>
          <Text style={s.kicker}>Approval needed</Text>
          <Text style={s.action}>{summary.action}</Text>
          {more > 0 ? <Text style={s.waiting}>{`${more} more waiting`}</Text> : null}
        </View>
      </View>
      {approval.cut ? <Text style={s.shortened}>{SHORTENED}</Text> : null}
      {command && command.lines > COUNT_PAST ? <Text style={s.more}>{`${command.lines} lines`}</Text> : null}
      <ScrollView style={s.body} contentContainerStyle={s.bodyContent}>
        {command ? <Text style={s.command}>{`$ ${command.text}`}</Text> : null}
        {files.map((file, i) => (
          <Text key={`${i}:${file}`} style={s.file}>
            {file}
          </Text>
        ))}
        {rest > 0 ? <Text style={s.more}>{`+${rest} more`}</Text> : null}
        {summary.cwd ? <Text style={s.where}>{`in ${summary.cwd}`}</Text> : null}
        <Text style={s.scope}>{`Allows: ${summary.scope}`}</Text>
        {full ? <Text style={s.detail}>{detail}</Text> : null}
      </ScrollView>
      {detail !== "" ? (
        <Pressable
          accessibilityRole="button"
          accessibilityLabel={full ? "Hide full request" : "Show full request"}
          accessibilityState={{ expanded: full }}
          onPress={toggle}
          style={s.toggle}
        >
          <Text style={s.toggleText}>{full ? "Hide full request" : "Show full request"}</Text>
        </Pressable>
      ) : null}
      {stage === "sending" ? <Text style={s.status}>Sending…</Text> : null}
      {error ? <Text style={s.error}>{error}</Text> : null}
      <View style={s.buttons}>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Deny"
          accessibilityState={{ disabled: !canAnswer, busy }}
          disabled={!canAnswer}
          onPress={deny}
          style={[s.button, !canAnswer && s.off]}
        >
          <Text style={s.denyLabel}>Deny</Text>
        </Pressable>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Allow"
          accessibilityState={{ disabled: !canAnswer, busy }}
          disabled={!canAnswer}
          onPress={allow}
          style={[s.button, s.allow, !canAnswer && s.off]}
        >
          <Text style={s.allowLabel}>Allow</Text>
        </Pressable>
      </View>
    </View>
  );
}
