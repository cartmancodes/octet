import { memo, useState } from "react";
import { View, type LayoutChangeEvent } from "react-native";

import type { PendingApproval } from "../model/session";
import type { Engine, Mode, Phase } from "../protocol";
import { PermissionCard, usePermission } from "./PermissionCard";
import { PromptLine } from "./PromptLine";
import type { Send } from "./send";

interface ConvoControlsProps {
  session: string;
  phase: Phase;
  engine: Engine;
  mode: Mode;
  modes: readonly Mode[] | null;
  model: string | null;
  /** The session's pending approvals, oldest first. The store fills them in place. */
  approvals: PendingApproval[];
  /** Must be stable. */
  send: Send;
  /** The Mac can't be reached: everything stays on screen, and nothing is sent. */
  offline: boolean;
  /** The session's last turn ended on a question. */
  question?: boolean;
  /** The header's menu, Session info or the drawer is over the approval: it cannot be answered. */
  covered: boolean;
  /** The safe area under the key bar. */
  bottom: number;
  /** The prompt line and key bar were laid out: their top, within the screen's body. Must be stable. */
  onPromptLayout?: (event: LayoutChangeEvent) => void;
}

/**
 * The foot of a conversation that takes commands: the Permission card, then the prompt line with
 * its slash menu and key bar. They share one view of the front approval and its guards, so the
 * card's buttons, the y n esc keys and a deny-then-prompt answer it once between them. Mounted
 * only for a live session: unmounting it (the session ended, the screen closed) is what stops a
 * late Face ID result from sending anything. Its two parts are the screen's children, so the
 * prompt line's layout is measured where the keyboard moves it.
 */
export const ConvoControls = memo(function ConvoControls({
  session,
  phase,
  engine,
  mode,
  modes,
  model,
  approvals,
  send,
  offline,
  question = false,
  covered,
  bottom,
  onPromptLayout,
}: ConvoControlsProps) {
  // A sheet the prompt line opened (a command's text, the /new ask) covers the approval too.
  const [sheetUp, setSheetUp] = useState(false);
  const permission = usePermission(session, approvals, { send, offline, covered: covered || sheetUp });
  return (
    <>
      <PermissionCard permission={permission} />
      <View testID="prompt-area" onLayout={onPromptLayout}>
        <PromptLine
          session={session}
          phase={phase}
          engine={engine}
          mode={mode}
          modes={modes}
          model={model}
          permission={permission}
          send={send}
          offline={offline}
          question={question}
          bottom={bottom}
          onCovered={setSheetUp}
        />
      </View>
    </>
  );
});
