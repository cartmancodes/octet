import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { AccessibilityInfo, Platform, Pressable, ScrollView, StyleSheet, Text, View } from "react-native";

import { useApprovalAnswer, type ApprovalAnswer } from "../approvals";
import { foldCommand } from "../model/command";
import type { PendingApproval } from "../model/session";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import type { Send } from "./send";

export { ARM_MS, NO_FACE_ID } from "../approvals";
/** Files listed before "+N more". */
export const FILES_SHOWN = 5;
/** What VoiceOver says when a request comes to the front. */
export const ANNOUNCED = "Approval waiting";
/** Said when the command or the request is not whole on the phone: Allow runs the Mac's. */
export const SHORTENED = "Shortened — the full command is on the Mac. Check the desk before allowing.";
/** A command longer than this many lines says how many it has. */
const COUNT_PAST = 3;

/** A session's front approval and how to answer it: what the Permission card shows, and what the
 * key bar's y, n and esc and the prompt line's deny-then-prompt answer, through the same guards. */
export interface Permission {
  /** The approval at the front, or null when none waits. */
  front: PendingApproval | null;
  /** How many wait behind it. */
  more: number;
  /** The guards and the answer (`useApprovalAnswer`) for `front`. */
  answer: ApprovalAnswer;
  /** The front request as the user sees it: its id and what it asks. New for each request, and
   * for the same one once its content is filled in. Empty when none waits. */
  look: string;
}

interface PermissionOptions {
  /** Must be stable. */
  send: Send;
  /** The Mac can't be reached: the request stays on screen and nothing can be answered. */
  offline?: boolean;
  /** Something is over the request (the header's menu or Session info, or the drawer): nothing can
   * be answered, and the arm delay starts again once it is gone, so a tap that closed it answers
   * nothing. */
  covered?: boolean;
}

/**
 * A session's pending approvals, one at a time: the oldest comes first, the others wait behind it.
 * A request another device closed goes (the store drops it); one answered here goes at once, and
 * stays gone while the Mac still lists it. Mounted only while the session takes commands:
 * unmounting it is what stops a late Face ID result from sending anything.
 */
export function usePermission(session: string, approvals: PendingApproval[], { send, offline = false, covered = false }: PermissionOptions): Permission {
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
  const front = waiting[0] ?? null;
  const more = Math.max(0, waiting.length - 1);
  // A new request at the front is news. Android speaks the live region; iOS has none, so it is
  // announced there (both would speak twice). Its record filling in later is the same request.
  const frontId = front?.id;
  useEffect(() => {
    if (frontId !== undefined && Platform.OS === "ios") AccessibilityInfo.announceForAccessibility(ANNOUNCED);
  }, [frontId]);
  const answer = useApprovalAnswer(session, front, { send, offline, covered, isOpen, onAnswered });
  const look = front ? `${front.id}:${JSON.stringify(front.summary)}` : "";
  return useMemo(() => ({ front, more, answer, look }), [front, more, answer, look]);
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    card: {
      marginTop: spacing.sm + 2,
      marginHorizontal: 14,
      marginBottom: spacing.sm,
      paddingTop: spacing.lg,
      paddingHorizontal: spacing.md,
      paddingBottom: spacing.md,
      gap: 10,
      backgroundColor: theme.panel,
      borderColor: theme.amber,
      borderWidth: 1,
      borderRadius: radius.card,
    },
    notch: { ...type.body, position: "absolute", left: 10, top: -10, paddingHorizontal: 4, backgroundColor: theme.bg, color: theme.amber },
    bold: { fontWeight: "700" },
    body: { maxHeight: 220 },
    rows: { gap: 2 },
    row: { flexDirection: "row" },
    label: { ...type.body, width: 64, flexShrink: 0, color: theme.muted },
    value: { flex: 1, gap: 2 },
    text: { ...type.body, color: theme.fg },
    muted: { ...type.body, color: theme.muted },
    shortened: { ...type.label, color: theme.amber, fontWeight: "600" },
    toggle: { minHeight: 44, justifyContent: "center", alignSelf: "flex-start" },
    toggleText: { ...type.body, color: theme.fg, textDecorationLine: "underline" },
    status: { ...type.label, color: theme.muted },
    error: { ...type.label, color: theme.amber },
    buttons: { flexDirection: "row", gap: 6 },
    button: {
      flex: 1,
      height: 44,
      minHeight: 44,
      alignItems: "center",
      justifyContent: "center",
      borderRadius: radius.card,
      borderColor: theme.edge,
      borderWidth: 1,
    },
    allow: { borderColor: theme.accent, backgroundColor: theme.selected },
    off: { opacity: 0.4 },
    allowLabel: { ...type.body, color: theme.accent },
    denyLabel: { ...type.body, color: theme.fg },
  });
}
type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/**
 * The Permission card at the foot of the conversation: an amber border with "Permission required
 * #id" notched into it, then TOOL (the action), CMD (the command, folded so hidden padding cannot
 * push its tail out of sight) and FILE (the files), and REASON (what it allows, and where). Allow
 * (accent) and Deny answer it once, through `useApprovalAnswer`. Every string is the Mac's and
 * drawn as text. Nothing shows when no approval waits.
 */
export const PermissionCard = memo(function PermissionCard({ permission }: { permission: Permission }) {
  const { front } = permission;
  if (!front) return null;
  // Keyed by what it asks: each request, and the same one once its content is filled in (the
  // journal record after the state frame), starts with its full request closed.
  return <Card key={permission.look} approval={front} more={permission.more} answer={permission.answer} />;
});

interface CardProps {
  approval: PendingApproval;
  more: number;
  answer: ApprovalAnswer;
}

function Card({ approval, more, answer }: CardProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const [full, setFull] = useState(false);
  const { allow: allowNow, deny: denyNow, busy, canAnswer } = answer;
  const allow = useCallback(() => void allowNow(), [allowNow]);
  const deny = useCallback(() => void denyNow(), [denyNow]);
  const toggle = useCallback(() => setFull((open) => !open), []);

  const { summary, detail } = approval;
  const files = summary.files.slice(0, FILES_SHOWN);
  const rest = summary.files.length - files.length;
  const command = summary.command ? foldCommand(summary.command) : null;
  return (
    <View testID="permission-card" style={s.card} accessibilityLiveRegion="polite">
      <Text style={s.notch}>
        <Text style={s.bold}>Permission required</Text>
        {` #${approval.id}`}
      </Text>
      {approval.cut ? <Text style={s.shortened}>{SHORTENED}</Text> : null}
      <ScrollView style={s.body} contentContainerStyle={s.rows}>
        <Row label="TOOL" s={s}>
          <Text style={s.text}>{summary.action}</Text>
        </Row>
        {command ? (
          <Row label="CMD" s={s}>
            <Text style={s.text}>{command.text}</Text>
            {command.lines > COUNT_PAST ? <Text style={s.muted}>{`${command.lines} lines`}</Text> : null}
          </Row>
        ) : null}
        {files.length > 0 ? (
          <Row label="FILE" s={s}>
            {files.map((file, i) => (
              <Text key={`${i}:${file}`} style={s.text}>
                {file}
              </Text>
            ))}
            {rest > 0 ? <Text style={s.muted}>{`+${rest} more`}</Text> : null}
          </Row>
        ) : null}
        <Row label="REASON" s={s}>
          <Text style={s.text}>{summary.scope}</Text>
          {summary.cwd ? <Text style={s.muted}>{`in ${summary.cwd}`}</Text> : null}
        </Row>
        {full ? <Text style={s.muted}>{detail}</Text> : null}
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
      {more > 0 ? <Text style={s.status}>{`${more} more waiting`}</Text> : null}
      {answer.sending ? <Text style={s.status}>Sending…</Text> : null}
      {answer.failure ? <Text style={s.error}>{answer.failure}</Text> : null}
      <View style={s.buttons}>
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
      </View>
    </View>
  );
}

function Row({ label, s, children }: { label: string; s: Styles; children: ReactNode }) {
  return (
    <View testID={`permission-${label}`} style={s.row}>
      <Text style={s.label}>{label}</Text>
      <View style={s.value}>{children}</View>
    </View>
  );
}
