// What the Inbox derives from the hub's sessions list: which tab each session is on, in what
// order, and the words on its card. Pure; the screen only draws it.
import { macNameOf } from "./macs";
import { foldCommand } from "./model/command";
import type { ApprovalInfo, SessionInfo } from "./protocol";
import { clipLabel, phaseWords, sessionName, sessionPose } from "./sessions";

export type Tone = "amber" | "accent" | "muted";

/** One card. */
export interface InboxItem {
  /** The session's id. */
  id: string;
  /** The Mac it runs on (the phone's id for it), when its row says. */
  mac?: string;
  /** The Mac's name, drawn muted on the card: only with several Macs paired. */
  macName?: string;
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

/** How the hub marks a string it shortened. */
const CLIP_MARK = "…";
/** The hub's last entry of a file list it cut (`gist`, crates/octet-hub/src/sessions.rs). */
const MORE_FILES = /^\+\d+ more$/;
/** The files of a card that has none: one array, so a card's props stay the same. */
const NO_FILES: readonly string[] = [];
/** What a screen reader adds for a shortened request. */
export const SHORTENED_LABEL = "Shortened: review it in the session";

/** The Mac's name: the first label of the hub's host ("studio-mac" from
 * "wss://studio-mac.tail1234.ts.net/"). An IP address is kept whole. "Mac" without an address. */
export const macName = macNameOf;

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

/** A listed row, and the Mac it came from when the store says. */
export type MacSession = SessionInfo & { mac?: string };

const byText = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

/** Newest activity first; a tie goes by the Mac's name (`nameOf`), then by id, so the order never
 * jumps. */
const newestFirst =
  (nameOf: (s: MacSession) => string) =>
  (a: MacSession, b: MacSession): number =>
    (b.last_activity ?? 0) - (a.last_activity ?? 0) || byText(nameOf(a), nameOf(b)) || byText(a.id, b.id);

type Base = Pick<InboxItem, "id" | "mac" | "macName" | "name" | "ago">;

function needsItem(s: SessionInfo, approval: ApprovalInfo | null, base: Base): InboxItem {
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
 * `mac` names the Mac on a quiet card; `now` is the clock in milliseconds. `names`, the paired
 * Macs' names by id, is given only when there are several: each card then carries its own Mac's
 * name (and a quiet card's status is the engine alone), and a tie in time goes by that name. O(n log n)
 * in the sessions.
 */
export function classify(
  sessions: readonly MacSession[],
  fronts: ReadonlyMap<string, ApprovalInfo | null>,
  { mac, now, names }: { mac: string; now: number; names?: ReadonlyMap<string, string> },
): Inbox {
  const out: Inbox = { needs: [], running: [], done: [] };
  const nameOf = (s: MacSession): string => (names && s.mac !== undefined ? (names.get(s.mac) ?? "") : "");
  for (const s of [...sessions].sort(newestFirst(nameOf))) {
    const macName = names ? nameOf(s) : undefined;
    const base: Base = { id: s.id, mac: s.mac, macName, name: sessionName(s.workspace), ago: shortAgo(s.last_activity, now) };
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
        // With several Macs the card names its own Mac already: the engine alone.
        status: macName !== undefined ? s.engine : `${mac} · ${s.engine}`,
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

/** The lists `inboxOf` made last, and what from. */
let made: { sessions: readonly MacSession[]; mac: string; now: number; names: ReadonlyMap<string, string> | undefined; inbox: Inbox } | null = null;

/** The same rows, in the same order. */
const sameRows = (a: readonly MacSession[], b: readonly MacSession[]): boolean =>
  a === b || (a.length === b.length && a.every((s, i) => s === b[i]));

/** The same names by id, in the same order (each screen's hook makes its own map). */
function sameNames(a: ReadonlyMap<string, string> | undefined, b: ReadonlyMap<string, string> | undefined): boolean {
  if (a === b) return true;
  if (!a || !b || a.size !== b.size) return false;
  const theirs = [...b];
  return [...a].every(([id, name], i) => theirs[i]?.[0] === id && theirs[i]?.[1] === name);
}

/**
 * The Inbox's lists of `sessions` (see `classify`), each card answering its own row's front
 * approval: made once for the same rows, clock, Mac name and names, and the same object until one
 * of them changes. The Inbox and the drawer read the same clock (`useAgoClock`), so whichever
 * draws second after a change reuses what the first made: the sessions are sorted once per
 * change, not once per list. With `names`, `mac` says nothing (each card names its own Mac).
 */
export function inboxOf(
  sessions: readonly MacSession[],
  { mac, now, names }: { mac: string; now: number; names?: ReadonlyMap<string, string> },
): Inbox {
  const name = names ? "" : mac;
  if (made && made.now === now && made.mac === name && sameNames(made.names, names) && sameRows(made.sessions, sessions)) {
    return made.inbox;
  }
  const fronts = new Map(sessions.map((s) => [s.id, s.approval ?? null]));
  const inbox = classify(sessions, fronts, names ? { mac: name, now, names } : { mac: name, now });
  made = { sessions, mac: name, now, names, inbox };
  return inbox;
}

/** What a screen reader says for a card: its name, how long ago, its status, its files and its
 * line, each part clipped (a screen reader reads every word), and whether the request is
 * shortened. */
export function cardLabel(item: InboxItem): string {
  return [
    item.name,
    item.macName ?? "",
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
    a.mac === b.mac &&
    a.macName === b.macName &&
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
  kind: "row";
  id: string;
  /** The Mac it runs on, when its row says. */
  mac?: string;
  name: string;
  /** "approve", "question", a phase word ("thinking"), "done", or how long ago; empty when none is known. */
  status: string;
  tone: Tone;
}

/** A Mac's header in the drawer, over its sessions: with several Macs only. */
export interface DrawerGroup {
  kind: "mac";
  /** `mac:<id>`: a list key no session's has. */
  id: string;
  mac: string;
}

export type DrawerItem = DrawerRow | DrawerGroup;

/** The drawer's session list, with the counts under its title. */
export interface DrawerList {
  rows: DrawerRow[];
  /** What the list draws: the rows, or, with several Macs, each Mac's header (every paired Mac,
   * in pairing order) with its rows under it. */
  items: DrawerItem[];
  /** Sessions that need you: an approval waiting, or a turn that ended on a question. */
  needs: number;
  /** Sessions with a turn running and nothing waiting. */
  running: number;
}

/**
 * The drawer's sessions, in the Inbox's order (needs you, running, then the rest, each newest
 * first): the Inbox's own lists (`inboxOf`), so the two sort once between them. A session waiting
 * on an approval says "approve"; one that ended on a question says "question"; one running says
 * what it is doing in a word; one that finished well says "done"; any other says how long ago it
 * last did anything. `now` is the clock in milliseconds. `names`, the paired Macs' names by id, is
 * given only when there are several: the rows are then grouped by Mac, in the Inbox's order within
 * each. `mac` is the one Mac's name, as the Inbox has it.
 */
export function drawerList(sessions: readonly MacSession[], now: number, names?: ReadonlyMap<string, string>, mac = "Mac"): DrawerList {
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const { needs, running, done } = inboxOf(sessions, names ? { mac, now, names } : { mac, now });
  const rows: DrawerRow[] = [];
  for (const item of [...needs, ...running, ...done]) {
    const s = byId.get(item.id);
    const base = { kind: "row" as const, id: item.id, mac: item.mac, name: item.name };
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
  return { rows, items: names ? grouped(rows, names) : rows, needs: needs.length, running: running.length };
}

/** Each Mac of `names` (in its order) as a header with its rows under it, in their order. */
function grouped(rows: readonly DrawerRow[], names: ReadonlyMap<string, string>): DrawerItem[] {
  const byMac = new Map<string, DrawerRow[]>();
  for (const row of rows) {
    const mac = row.mac ?? "";
    const list = byMac.get(mac);
    if (list) list.push(row);
    else byMac.set(mac, [row]);
  }
  const items: DrawerItem[] = [];
  for (const mac of names.keys()) {
    items.push({ kind: "mac", id: `mac:${mac}`, mac });
    for (const row of byMac.get(mac) ?? []) items.push(row);
  }
  return items;
}

/** What a screen reader says for a drawer row: its name, and its status when it has one. */
export const drawerRowLabel = (row: DrawerRow): string => (row.status === "" ? row.name : `${row.name}, ${row.status}`);

/** The line under "Octet" in the drawer: "2 need you · 3 running", or just "3 running". */
export const drawerSub = (needs: number, running: number): string =>
  needs > 0 ? `${needs} need you · ${running} running` : `${running} running`;
