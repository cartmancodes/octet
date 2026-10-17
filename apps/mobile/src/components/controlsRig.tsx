// The conversation's controls on a fake `send`, for the tests of the Permission card, the prompt
// line, the slash menu and the key bar. Not imported by the app.
import { act, screen } from "@testing-library/react-native";

import type { RequestBody } from "../client/HubClient";
import type { PendingApproval } from "../model/session";
import type { Engine, Mode, Phase } from "../protocol";
import { ConvoControls } from "./ConvoControls";
import type { Send } from "./send";

/** A `send` the test settles by hand, in the order the commands went. */
export function rig() {
  const sent: RequestBody[] = [];
  const waiting: { resolve: (ack?: unknown) => void; reject: (e: unknown) => void }[] = [];
  const send = jest.fn((body: RequestBody) => {
    sent.push(body);
    return new Promise<unknown>((resolve, reject) => waiting.push({ resolve: (ack: unknown = {}) => resolve(ack), reject }));
  });
  return { sent, send, waiting };
}
export type Rig = ReturnType<typeof rig>;

export interface ControlsOpts {
  session?: string;
  phase?: Phase;
  engine?: Engine;
  mode?: Mode;
  modes?: readonly Mode[] | null;
  model?: string | null;
  approvals?: PendingApproval[];
  offline?: boolean;
  covered?: boolean;
  question?: boolean;
  bottom?: number;
}

const NONE: PendingApproval[] = [];

/** The controls of session "s" (by default), idle, with no approval. */
export const controls = (send: Send, o: ControlsOpts = {}) => (
  <ConvoControls
    session={o.session ?? "s"}
    phase={o.phase ?? "idle"}
    engine={o.engine ?? "claude"}
    mode={o.mode ?? "ask"}
    modes={o.modes === undefined ? null : o.modes}
    model={o.model ?? null}
    approvals={o.approvals ?? NONE}
    send={send}
    offline={o.offline ?? false}
    covered={o.covered ?? false}
    question={o.question ?? false}
    bottom={o.bottom ?? 0}
  />
);

export const approval = (id: number, extra: Partial<PendingApproval["summary"]> = {}): PendingApproval => ({
  id,
  detail: "",
  summary: { action: `Run command ${id}`, command: `make test-${id}`, files: [], cwd: null, scope: "this command once", ...extra },
});

export const flush = () => act(async () => {});
/** For tests on fake timers. */
export const settle = (ms: number) => act(async () => void jest.advanceTimersByTime(ms));

export const input = () => screen.getByLabelText("Message");
export const key = (name: string) => screen.getByRole("button", { name });
export const disabled = (node: { props: { accessibilityState?: { disabled?: boolean }; "aria-disabled"?: boolean } }) =>
  node.props.accessibilityState?.disabled === true || node.props["aria-disabled"] === true;
