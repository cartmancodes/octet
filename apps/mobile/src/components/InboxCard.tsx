import { memo, useCallback } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";

import { useApprovalAnswer, type AnswerLedger } from "../approvals";
import { cardLabel, sameItem, type InboxItem } from "../inbox";
import { Button } from "../ui";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import type { Send } from "./send";

interface InboxCardProps {
  item: InboxItem;
  /** The card's Mac can't be reached: the card is greyed and does nothing. */
  offline: boolean;
  /** The drawer is over the card: it answers nothing, and arms again once the drawer is gone. */
  covered: boolean;
  /** The rest must be stable, or every card re-renders with the list. */
  send: Send;
  ledger: AnswerLedger;
  /** Whether `id` is still the session's front approval, as the store has it now. */
  isFront: (session: string, id: number) => boolean;
  /** Opens the session. */
  onOpen: (id: string) => void;
  /** Opens the session with its prompt focused, to answer the question it ended on. */
  onReply: (id: string) => void;
}

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    card: { borderWidth: 1, borderRadius: radius.card, borderColor: theme.edge, paddingVertical: spacing.md, paddingHorizontal: 14, gap: 10 },
    plain: { backgroundColor: theme.bg },
    approval: { backgroundColor: theme.panel },
    grey: { opacity: 0.5 },
    top: { minHeight: 44 },
    head: { flexDirection: "row", justifyContent: "space-between", gap: spacing.sm },
    name: { ...type.body, color: theme.fg, fontWeight: "700", flexShrink: 1 },
    ago: { ...type.body, color: theme.muted },
    // The Mac's name gives way (to one line, then cut) before the session's name; "ago" never does.
    side: { flexDirection: "row", gap: spacing.sm, flexShrink: 1, minWidth: 0 },
    macName: { ...type.body, color: theme.muted, flexShrink: 1 },
    agoFixed: { ...type.body, color: theme.muted, flexShrink: 0 },
    amber: { ...type.body, color: theme.amber },
    accent: { ...type.body, color: theme.accent },
    muted: { ...type.body, color: theme.muted },
    file: { ...type.body, color: theme.fg, paddingLeft: spacing.md },
    buttons: { flexDirection: "row", gap: spacing.sm },
    button: { flex: 1 },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/**
 * One session on the Inbox: its name and how long ago, the status line, and the excerpt. A card
 * with an approval answers it here: Allow once (Face ID, then armed 400 ms after it shows) and
 * Deny, or Review for a file edit. A request the hub shortened offers Review in place of Allow
 * once. A session that ended on a question offers Reply, which opens it with the prompt ready.
 * Every string is the Mac's and drawn as text.
 */
export const InboxCard = memo(function InboxCard({ item, offline, covered, send, ledger, isFront, onOpen, onReply }: InboxCardProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const { id, approval } = item;
  const isOpen = useCallback((approvalId: number) => isFront(id, approvalId), [isFront, id]);
  const answer = useApprovalAnswer(id, approval, { send, offline, covered, isOpen, ledger });
  const { allow: allowNow, deny: denyNow } = answer;
  const open = useCallback(() => onOpen(id), [onOpen, id]);
  const reply = useCallback(() => onReply(id), [onReply, id]);
  const allow = useCallback(() => void allowNow(), [allowNow]);
  const deny = useCallback(() => void denyNow(), [denyNow]);
  const waiting = item.statusTone === "amber";
  return (
    <View testID={`inbox-card-${id}`} style={[s.card, waiting ? s.approval : s.plain, offline ? s.grey : null]}>
      <Pressable
        accessibilityRole="button"
        accessibilityLabel={cardLabel(item)}
        accessibilityState={offline ? DISABLED : ENABLED}
        disabled={offline}
        onPress={open}
        style={s.top}
      >
        <View style={s.head}>
          <Text numberOfLines={1} style={s.name}>
            {item.name}
          </Text>
          {/* With several Macs, the card names its own, muted, before how long ago. */}
          {item.macName ? (
            <View style={s.side}>
              <Text numberOfLines={1} style={s.macName}>
                {item.macName}
              </Text>
              <Text style={s.agoFixed}>{item.ago}</Text>
            </View>
          ) : (
            <Text style={s.ago}>{item.ago}</Text>
          )}
        </View>
        {/* An approval's command shows whole (its gist is at most 200 characters): what Allow runs
            is never cut off by the edge of the screen. */}
        <Text numberOfLines={approval ? undefined : 1} style={s[item.statusTone]}>
          {item.status}
        </Text>
        {/* Every file of the gist, each on its own line: all Allow touches is on the card. */}
        {item.files.map((file, i) => (
          <Text key={`${i}:${file}`} style={s.file}>
            {file}
          </Text>
        ))}
        {item.line !== "" ? (
          <Text numberOfLines={2} style={s.muted}>
            {item.line}
          </Text>
        ) : null}
      </Pressable>
      {approval ? (
        <View style={s.buttons}>
          {/* A shortened request is not all on the card: it is reviewed in the session, never
              allowed from here. One Review is enough; Deny still answers a command. */}
          {item.shortened ? (
            <Button label="Review" accessibilityLabel={`Review ${item.name}`} disabled={offline} onPress={open} style={s.button} />
          ) : (
            <Button
              label="Allow once"
              accessibilityLabel={`Allow once for ${item.name}`}
              variant="accent"
              disabled={!answer.canAnswer}
              busy={answer.busy}
              onPress={allow}
              style={s.button}
            />
          )}
          {item.editApproval ? (
            item.shortened ? null : (
              <Button label="Review" accessibilityLabel={`Review ${item.name}`} disabled={offline} onPress={open} style={s.button} />
            )
          ) : (
            <Button
              label="Deny"
              accessibilityLabel={`Deny for ${item.name}`}
              disabled={!answer.canAnswer}
              busy={answer.busy}
              onPress={deny}
              style={s.button}
            />
          )}
        </View>
      ) : item.question ? (
        <Button label="Reply" accessibilityLabel={`Reply to ${item.name}`} variant="accent" disabled={offline} onPress={reply} />
      ) : waiting ? (
        <Button label="Review" accessibilityLabel={`Review ${item.name}`} disabled={offline} onPress={open} />
      ) : null}
      {answer.sending ? <Text style={s.muted}>Sending…</Text> : null}
      {answer.failure ? <Text style={s.amber}>{answer.failure}</Text> : null}
    </View>
  );
}, sameProps);

/** A card draws again only for its own data: the list hands it a fresh item each time. */
function sameProps(a: InboxCardProps, b: InboxCardProps): boolean {
  return (
    sameItem(a.item, b.item) &&
    a.offline === b.offline &&
    a.covered === b.covered &&
    a.send === b.send &&
    a.ledger === b.ledger &&
    a.isFront === b.isFront &&
    a.onOpen === b.onOpen &&
    a.onReply === b.onReply
  );
}

const DISABLED = { disabled: true } as const;
const ENABLED = { disabled: false } as const;
