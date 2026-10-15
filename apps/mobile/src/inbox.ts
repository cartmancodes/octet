// What the Inbox derives from the hub's sessions list: which tab each session is on, in what
// order, and the words on its card. Pure; the screen only draws it.
import { foldCommand } from "./model/command";
import type { ApprovalInfo, SessionInfo } from "./protocol";
import { clipLabel, phaseWords, sessionName, sessionPose } from "./sessions";

export type Tone = "amber" | "accent" | "muted";

/** One card. */
export interface InboxItem {
  /** The session's id. */
  id: string;
  /** Its workspace folder. */
  name: string;
  /** How long since it last did anything: "now", "2m", "3h", "2d", "1w"; empty when unknown. */
  ago: string;
  /** "● Bash · cargo test", "● THINKING · claude", "studio-mac · claude". */
  status: string;
  statusTone: Tone;
  /** The muted excerpt under the status. */
  line: string;
  /** An approval's files, as its gist has them (at most three, then "+N more"): each drawn on its
   * own line under the status, so everything Allow touches is on the card. Empty otherwise. */
  files: readonly string[];
  /** The approval a card answers: the session's front one, as its row has it. */
  approval: ApprovalInfo | null;
  /** The approval edits files and runs no command: the card offers Review, not Deny. */
  editApproval: boolean;
  /** The hub cut the approval's gist: its command (the first line, at most 200 characters) or
   * its files (three, then "+N more"); or the gist has neither command nor files, as when the
   * bridge reduced it to fit. What Allow would run is not all on the card, so the card offers
   * Review in place of Allow once, and Allow all leaves it out. */
  shortened: boolean;
  /** No approval waits, but the last turn ended on a question (the hub's heuristic): the card
   * offers Reply. */
  question: boolean;
}

export interface Inbox {
  /** An approval waits, or the last turn ended on a question. */
  needs: InboxItem[];
  /** A turn is running. */
  running: InboxItem[];
  /** Everything else. */
  done: InboxItem[];
}

/** The excerpt of a session waiting for an approval its row does not describe. */
export const NO_GIST_LINE = "Open the session to see what it asks.";

/** The excerpt of a session whose last turn ended on a question. */
export const QUESTION_LINE = "Ended on a question";

/** The name for the Mac when the hub's address says nothing. */
const SOME_MAC = "Mac";
/** How the hub marks a string it shortened. */
const CLIP_MARK = "…";
/** The hub's last entry of a file list it cut (`gist`, crates/octet-hub/src/sessions.rs). */
const MORE_FILES = /^\+\d+ more$/;
/** The files of a card that has none: one array, so a card's props stay the same. */
const NO_FILES: readonly string[] = [];
/** What a screen reader adds for a shortened request. */
export const SHORTENED_LABEL = "Shortened: review it in the session";
const LABEL_MAX = 63;

/** The Mac's name: the first label of the hub's host ("studio-mac" from
 * "wss://studio-mac.tail1234.ts.net/"). An IP address is kept whole. "Mac" without an address. */
export function macName(hubUrl: string | null | undefined): string {
  const match = /^[a-z][a-z0-9+.-]*:\/\/(\[[^\]/]*\]|[^/:?#[\]]+)/i.exec(hubUrl ?? "");
  const host = match?.[1];
  if (!host) return SOME_MAC;
  if (host.startsWith("[") || /^[0-9.]+$/.test(host)) return host;
  const first = host.split(".")[0] ?? "";
  return first === "" ? SOME_MAC : first.slice(0, LABEL_MAX);
}

const MINUTE = 60;
const HOUR = 3_600;
const DAY = 86_400;
const WEEK = 7 * DAY;

/** "now", "5m", "3h", "2d", "2w". `seconds` is Unix seconds and `nowMs` the clock in
 * milliseconds; a time in the future (clock skew) is now; zero or none is not known, so empty. */
export function shortAgo(seconds: number | null | undefined, nowMs: number): string {
  if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds <= 0) return "";
  const gone = Math.max(0, Math.floor(nowMs / 1000 - seconds));
  if (gone < MINUTE) return "now";
  if (gone < HOUR) return `${Math.floor(gone / MINUTE)}m`;
  if (gone < DAY) return `${Math.floor(gone / HOUR)}h`;
  if (gone < WEEK) return `${Math.floor(gone / DAY)}d`;
  return `${Math.floor(gone / WEEK)}w`;
}

/** Newest activity first; the id breaks a tie so the order never jumps. */
const newestFirst = (a: SessionInfo, b: SessionInfo): number =>
  (b.last_activity ?? 0) - (a.last_activity ?? 0) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);

function needsItem(s: SessionInfo, approval: ApprovalInfo | null, base: Pick<InboxItem, "id" | "name" | "ago">): InboxItem {
  if (!approval) {
    return {
      ...base,
      status: `● approval · ${s.engine}`,
      statusTone: "amber",
      line: NO_GIST_LINE,
      files: NO_FILES,
      approval: null,
      editApproval: false,
      shortened: false,
      question: false,
    };
  }
  const { action, command, files, scope, cwd } = approval.summary;
  // The hidden-padding fold, as the Permission card draws a command: nothing Allow runs is pushed out of sight.
  const what = command ? foldCommand(command).text : null;
  return {
    ...base,
    status: what ? `● ${action} · ${what}` : `● ${action}`,
    statusTone: "amber",
    line: scope !== "" ? scope : (cwd ?? ""),
    files: files.length > 0 ? files : NO_FILES,
    approval,
    editApproval: !command && files.length > 0,
    shortened:
      (!command && files.length === 0) ||
      (!!command && command.endsWith(CLIP_MARK)) ||
      files.some((f) => MORE_FILES.test(f) || f.endsWith(CLIP_MARK)),
    question: false,
  };
}

/**
 * The Inbox's three lists, each newest first. `fronts` is the approval each session's card
 * answers (the screen gives each row's own); a session waiting with none there can only be opened.
 * `mac` names the Mac on a quiet card; `now` is the clock in milliseconds. O(n log n) in the
 * sessions.
 */
export function classify(
  sessions: readonly SessionInfo[],
  fronts: ReadonlyMap<string, ApprovalInfo | null>,
  { mac, now }: { mac: string; now: number },
): Inbox {
  const out: Inbox = { needs: [], running: [], done: [] };
  for (const s of [...sessions].sort(newestFirst)) {
    const base = { id: s.id, name: sessionName(s.workspace), ago: shortAgo(s.last_activity, now) };
    if (s.approvals > 0) {
      out.needs.push(needsItem(s, fronts.get(s.id) ?? null, base));
    } else if (s.question === true && s.phase !== "running") {
      // A running turn has moved past the question, whatever the row still says.
      out.needs.push({
        ...base,
        status: "● question",
        statusTone: "amber",
        line: QUESTION_LINE,
        files: NO_FILES,
        approval: null,
        editApproval: false,
        shortened: false,
        question: true,
      });
    } else if (s.phase === "running") {
      out.running.push({
        ...base,
        status: `● ${sessionPose(s).toUpperCase()} · ${s.engine}`,
        statusTone: "accent",
        line: s.now ? s.now : phaseWords(s.phase),
        files: NO_FILES,
        approval: null,
        editApproval: false,
        shortened: false,
        question: false,
      });
    } else {
      out.done.push({
        ...base,
        status: `${mac} · ${s.engine}`,
        statusTone: "muted",
        line: s.goal ? s.goal : phaseWords(s.phase).toLowerCase(),
        files: NO_FILES,
        approval: null,
        editApproval: false,
        shortened: false,
        question: false,
      });
    }
  }
  return out;
}

/** What a screen reader says for a card: its name, how long ago, its status, its files and its
 * line, each part clipped (a screen reader reads every word), and whether the request is
 * shortened. */
export function cardLabel(item: InboxItem): string {
  return [
    item.name,
    item.ago,
    clipLabel(item.status),
    ...item.files.map((file) => clipLabel(file)),
    clipLabel(item.line),
    item.shortened ? SHORTENED_LABEL : "",
  ]
    .filter((x) => x !== "")
    .join(", ");
}

/** Two items that draw the same: a card re-renders only when this is false. */
export function sameItem(a: InboxItem, b: InboxItem): boolean {
  return (
    a.id === b.id &&
    a.name === b.name &&
    a.ago === b.ago &&
    a.status === b.status &&
    a.statusTone === b.statusTone &&
    a.line === b.line &&
    a.files === b.files &&
    a.approval === b.approval &&
    a.editApproval === b.editApproval &&
    a.shortened === b.shortened &&
    a.question === b.question
  );
}

/** One row of the drawer's session list. */
export interface DrawerRow {
  id: string;
  name: string;
  /** "approve", "question", a phase word ("thinking"), "done", or how long ago; empty when none is known. */
  status: string;
  tone: Tone;
}

/** The drawer's session list, with the counts under its title. */
export interface DrawerList {
  rows: DrawerRow[];
  /** Sessions that need you: an approval waiting, or a turn that ended on a question. */
  needs: number;
  /** Sessions with a turn running and nothing waiting. */
  running: number;
}

/**
 * The drawer's sessions, in the Inbox's order (needs you, running, then the rest, each newest
 * first). A session waiting on an approval says "approve"; one that ended on a question says
 * "question"; one running says what it is doing in a
 * word; one that finished well says "done"; any other says how long ago it last did anything.
 * `now` is the clock in milliseconds.
 */
export function drawerList(sessions: readonly SessionInfo[], now: number): DrawerList {
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const { needs, running, done } = classify(sessions, new Map(), { mac: "", now });
  const rows: DrawerRow[] = [];
  for (const item of [...needs, ...running, ...done]) {
    const s = byId.get(item.id);
    const base = { id: item.id, name: item.name };
    if (item.question) {
      rows.push({ ...base, status: "question", tone: "amber" });
    } else if (item.statusTone === "amber") {
      rows.push({ ...base, status: "approve", tone: "amber" });
    } else if (s && item.statusTone === "accent") {
      rows.push({ ...base, status: sessionPose(s), tone: "accent" });
    } else if (s && sessionPose(s) === "success") {
      rows.push({ ...base, status: "done", tone: "accent" });
    } else {
      rows.push({ ...base, status: item.ago, tone: "muted" });
    }
  }
  return { rows, needs: needs.length, running: running.length };
}

/** The line under "Octet" in the drawer: "2 need you · 3 running", or just "3 running". */
export const drawerSub = (needs: number, running: number): string =>
  needs > 0 ? `${needs} need you · ${running} running` : `${running} running`;
