import type { KnownJournalEvent, ToolPhase } from "../protocol";
import { utf8Length } from "../client/frames";
import {
  BLOCK_BYTES,
  BLOCK_SLACK,
  EARLIER,
  EMPTY_TRANSCRIPT,
  MAX_ENTRIES,
  apply,
  applyAll,
  entrySize,
  type Entry,
  type Transcript,
} from "./transcript";

const text = (data: string): KnownJournalEvent => ({ type: "text", data });
const user = (data: string): KnownJournalEvent => ({ type: "user", data });
const tool = (id: string, name: string, target: string, phase: ToolPhase, detail = ""): KnownJournalEvent => ({
  type: "tool",
  data: { id, name, target, phase, detail },
});
const finished: KnownJournalEvent = { type: "finished", data: "completed" };
const started: KnownJournalEvent = { type: "started", data: null };
const stopped: KnownJournalEvent = { type: "stopped", data: null };

const run = (events: KnownJournalEvent[], from: Transcript = EMPTY_TRANSCRIPT): Transcript =>
  events.reduce(apply, from);
const kinds = (t: Transcript): string[] => t.entries.map((e) => e.kind);
const last = (t: Transcript): Entry => {
  const e = t.entries[t.entries.length - 1];
  if (!e) throw new Error("no entries");
  return e;
};
const tools = (e: Entry) => {
  if (e.kind !== "tools") throw new Error(`not a tools entry: ${e.kind}`);
  return e;
};
/** The bytes the entries hold, counted again from scratch. */
const measured = (t: Transcript): number => t.entries.reduce((sum, e) => sum + entrySize(e), 0);

describe("grouping tools", () => {
  it("joins consecutive tool events into one entry", () => {
    const t = run([tool("1", "Read", "a", "running"), tool("2", "Read", "b", "running"), tool("3", "Bash", "ls", "running")]);
    expect(kinds(t)).toEqual(["tools"]);
    expect(tools(last(t)).calls.map((c) => c.id)).toEqual(["1", "2", "3"]);
    expect(tools(last(t)).over).toBe(false);
  });

  it("starts a new group after text, and text after tools starts a new reply", () => {
    const t = run([tool("1", "Read", "a", "running"), text("hello"), tool("2", "Read", "b", "running"), text("bye")]);
    expect(kinds(t)).toEqual(["tools", "reply", "tools", "reply"]);
  });

  it("pairs an end with its call, keeping the start's target when the end has none", () => {
    const t = run([tool("1", "Read", "a.rs", "running"), tool("1", "Read", "", "done", "contents")]);
    expect(tools(last(t)).calls).toEqual([{ id: "1", name: "Read", target: "a.rs", phase: "done", detail: "contents" }]);
  });

  it("finds the end's call in an older tools entry of the same turn", () => {
    const t = run([
      started,
      tool("1", "Bash", "ls", "running"),
      text("thinking"),
      tool("2", "Read", "b", "running"),
      tool("1", "Bash", "ls", "done", "out"),
    ]);
    expect(kinds(t)).toEqual(["tools", "reply", "tools"]);
    expect(tools(t.entries[0] as Entry).calls[0]?.phase).toBe("done");
    expect(tools(last(t)).calls).toHaveLength(1);
  });

  it("pairs newest first when an ID is in two entries", () => {
    const t = run([
      tool("1", "Bash", "first", "running"),
      text("x"),
      tool("1", "Bash", "second", "running"),
      tool("1", "Bash", "", "done"),
    ]);
    expect(tools(t.entries[0] as Entry).calls[0]?.phase).toBe("running");
    expect(tools(last(t)).calls[0]).toMatchObject({ target: "second", phase: "done" });
  });

  it("never pairs an empty ID", () => {
    const t = run([tool("9", "Bash", "ls", "done"), tool("", "Bash", "a", "running"), tool("", "Bash", "a", "done")]);
    expect(tools(last(t)).calls).toHaveLength(3);
  });

  it("does not pair across a turn: an end after finished is its own call", () => {
    const t = run([started, tool("1", "Bash", "ls", "running"), finished, tool("1", "Bash", "ls", "done")]);
    expect(kinds(t)).toEqual(["tools", "tools"]);
    expect(tools(t.entries[0] as Entry).calls[0]?.phase).toBe("running");
  });

  it("closes a tools entry once its size passes 64 KiB", () => {
    const big = "x".repeat(40 * 1024);
    const t = run([
      tool("1", "Read", "a", "done", big),
      tool("2", "Read", "b", "done", big), // the entry was 40 KiB: it still takes this one
      tool("3", "Read", "c", "done", big), // now 80 KiB: closed
    ]);
    expect(kinds(t)).toEqual(["tools", "tools"]);
    expect(tools(t.entries[0] as Entry).calls.map((c) => c.id)).toEqual(["1", "2"]);
    expect(tools(last(t)).calls.map((c) => c.id)).toEqual(["3"]);
    expect(t.bytes).toBe(measured(t));
  });

  it("still pairs an end into a closed entry of this turn", () => {
    const big = "x".repeat(70 * 1024);
    const t = run([tool("1", "Bash", "a", "running"), tool("2", "Read", "b", "done", big), tool("3", "Read", "c", "running"), tool("1", "Bash", "", "done")]);
    expect(tools(t.entries[0] as Entry).calls.find((c) => c.id === "1")?.phase).toBe("done");
    expect(t.bytes).toBe(measured(t));
  });

  it("marks running calls over when the turn finishes or stops", () => {
    for (const end of [finished, stopped]) {
      const t = run([started, tool("1", "Bash", "sleep 9", "running"), end]);
      expect(tools(last(t)).over).toBe(true);
      expect(tools(last(t)).calls[0]?.phase).toBe("running");
    }
  });

  it("keeps the earlier turn's entries over and marks only this turn's", () => {
    const t = run([started, tool("1", "Bash", "a", "running"), finished, started, tool("2", "Bash", "b", "running")]);
    expect(t.entries.map((e) => (e.kind === "tools" ? e.over : null))).toEqual([true, false]);
  });
});

describe("streaming replies", () => {
  it("appends text events to the open reply", () => {
    const t = run([started, text("Hel"), text("lo "), text("world")]);
    expect(kinds(t)).toEqual(["reply"]);
    expect(last(t)).toMatchObject({ kind: "reply", text: "Hello world", done: false });
    expect(t.bytes).toBe(11);
  });

  it("closes the reply on finished; later text is a new reply", () => {
    const t = run([started, text("one"), finished, text("two")]);
    expect(t.entries).toMatchObject([
      { kind: "reply", text: "one", done: true },
      { kind: "reply", text: "two", done: false },
    ]);
  });

  it("closes the reply on stopped and on a new turn", () => {
    expect(last(run([text("a"), stopped]))).toMatchObject({ done: true });
    expect(last(run([text("a"), started]))).toMatchObject({ done: true });
  });

  it("puts a user message, a note and an error in the transcript", () => {
    const t = run([user("hi"), { type: "notice", data: "n" }, { type: "error", data: "e" }]);
    expect(t.entries).toMatchObject([
      { kind: "user", text: "hi" },
      { kind: "note", text: "n" },
      { kind: "error", text: "e" },
    ]);
    expect(new Set(t.entries.map((e) => e.key)).size).toBe(3);
  });

  it("trims a long streamed reply in steps, keeping the end", () => {
    const chunk = `${"streamed words ".repeat(68)}\n`;
    let t = run([started]);
    let longest = 0;
    for (let i = 0; i < 200; i++) {
      t = apply(t, text(chunk));
      longest = Math.max(longest, entrySize(last(t)));
    }
    const reply = last(t);
    expect(reply.kind).toBe("reply");
    if (reply.kind !== "reply") return;
    expect(reply.text.startsWith(EARLIER)).toBe(true);
    expect(reply.text.endsWith(chunk)).toBe(true);
    expect(longest).toBeLessThanOrEqual(BLOCK_BYTES + BLOCK_SLACK + utf8Length(EARLIER) + utf8Length(chunk));
    expect(entrySize(reply)).toBeLessThanOrEqual(BLOCK_BYTES + BLOCK_SLACK + utf8Length(EARLIER));
    expect(t.bytes).toBe(measured(t));
    // The prefix is never stacked.
    expect(reply.text.indexOf(EARLIER, 1)).toBe(-1);
  });
});

describe("hooks", () => {
  const hook = (outcome: string, detail = "", decision: string | null = null): KnownJournalEvent => ({
    type: "hook",
    data: { name: "PreToolUse:Bash", event: "PreToolUse", outcome, detail, decision },
  });

  it("shows only a failed, blocked or decided hook, in the TUI's words", () => {
    expect(run([hook("ran")]).entries).toEqual([]);
    expect(last(run([hook("failed")]))).toMatchObject({ kind: "note", text: "Hook PreToolUse:Bash failed" });
    expect(last(run([hook("failed", "exit 2")]))).toMatchObject({ text: "Hook PreToolUse:Bash failed: exit 2" });
    expect(last(run([hook("blocked")]))).toMatchObject({ text: "Hook PreToolUse:Bash blocked the action" });
    expect(last(run([hook("decided", "", "allow")]))).toMatchObject({
      text: "Hook PreToolUse:Bash allowed the tool (Octet was not asked)",
    });
    expect(last(run([hook("decided", "", "deny")]))).toMatchObject({
      text: "Hook PreToolUse:Bash answered the approval: deny",
    });
  });
});

describe("what the transcript does not show", () => {
  it("returns the same transcript for kinds it ignores", () => {
    const t = run([user("hi")]);
    const ignored: KnownJournalEvent[] = [
      { type: "ready", data: "id" },
      { type: "session", data: {} },
      { type: "models", data: [] },
      { type: "skills", data: [] },
      { type: "mode", data: "auto" },
      { type: "model_selected", data: "m" },
      { type: "usage", data: "1k" },
      { type: "approval", data: { id: 1, detail: "{}", summary: { action: "a", files: [], scope: "s" } } },
      { type: "approval_closed", data: 1 },
    ];
    for (const ev of ignored) expect(apply(t, ev)).toBe(t);
    expect(apply(t, started)).toBe(t);
  });

  it("ignores events whose data is not what the type promises", () => {
    const t = run([user("hi")]);
    const bad = [
      { type: "text", data: 5 },
      { type: "user", data: null },
      { type: "tool", data: "oops" },
      { type: "tool", data: { id: 1, name: "x", target: "", phase: "done", detail: "" } },
      { type: "tool", data: { id: "1", name: "x", target: "", phase: "weird", detail: "" } },
      { type: "hook", data: null },
      { type: "notice", data: { a: 1 } },
    ] as unknown as KnownJournalEvent[];
    for (const ev of bad) expect(apply(t, ev)).toBe(t);
  });
});

describe("sanitising", () => {
  it("drops escapes, control characters and bidi marks, and widens tabs", () => {
    const t = run([text("\u001b[31mred\u001b[0m a\tb ‮evil\u0007 \r\n")]);
    expect(last(t)).toMatchObject({ text: "red a    b evil \n" });
    expect(t.bytes).toBe(measured(t));
  });

  it("drops an escape string split across text events", () => {
    const t = run([text("a\u001b]0;ti"), text("tle\u0007b"), text("\u001b"), text("[1;3"), text("1mc")]);
    expect(last(t)).toMatchObject({ text: "abc" });
  });

  it("cleans tool names, targets and details", () => {
    const t = run([tool("1", "Ba\u001b[1msh", "ls\u0007", "done", "ok\u001b[0m\r\n")]);
    expect(tools(last(t)).calls[0]).toMatchObject({ name: "Bash", target: "ls", detail: "ok\n" });
  });

  it("drops a lone surrogate so that byte counts stay exact", () => {
    const t = run([text("a\ud800b"), text("\udc00c"), user("😀 ok")]);
    expect(t.entries[0]).toMatchObject({ text: "abc" });
    expect(t.bytes).toBe(measured(t));
    expect(t.bytes).toBe(3 + utf8Length("😀 ok"));
  });

  it("counts bytes, not characters", () => {
    const t = run([user("héllo €"), text("日本語 😀")]);
    expect(t.bytes).toBe(utf8Length("héllo €") + utf8Length("日本語 😀"));
    expect(t.bytes).toBe(measured(t));
  });
});

describe("structural sharing", () => {
  it("changes only the entry that changed", () => {
    const t1 = run([user("a"), text("b"), tool("1", "Read", "x", "running"), text("c")]);
    const t2 = apply(t1, text(" more"));
    expect(t2).not.toBe(t1);
    expect(t2.entries[0]).toBe(t1.entries[0]);
    expect(t2.entries[1]).toBe(t1.entries[1]);
    expect(t2.entries[2]).toBe(t1.entries[2]);
    expect(t2.entries[3]).not.toBe(t1.entries[3]);
    expect(last(t2)).toMatchObject({ text: "c more" });
    // The old transcript is untouched.
    expect(last(t1)).toMatchObject({ text: "c" });
  });

  it("an end leaves the other entries alone", () => {
    const t1 = run([user("a"), tool("1", "Read", "x", "running"), text("c")]);
    const t2 = apply(t1, tool("1", "Read", "x", "done"));
    expect(t2.entries[0]).toBe(t1.entries[0]);
    expect(t2.entries[1]).not.toBe(t1.entries[1]);
    expect(t2.entries[2]).toBe(t1.entries[2]);
  });

  it("finishing marks only entries of the turn", () => {
    const t1 = run([started, tool("1", "Bash", "a", "running"), finished, started, user("again"), tool("2", "Bash", "b", "running")]);
    const t2 = apply(t1, finished);
    expect(t2.entries[0]).toBe(t1.entries[0]);
    expect(t2.entries[1]).toBe(t1.entries[1]);
    expect(tools(t2.entries[2] as Entry).over).toBe(true);
  });

  it("applyAll gives the same result as applying one by one", () => {
    const events = [started, user("go"), tool("1", "Read", "a", "running"), tool("1", "Read", "", "done", "d"), text("x"), text("y"), finished];
    expect(applyAll(EMPTY_TRANSCRIPT, events)).toEqual(run(events));
    expect(applyAll(EMPTY_TRANSCRIPT, [])).toBe(EMPTY_TRANSCRIPT);
  });
});

describe("huge_records_are_clipped_and_batched", () => {
  const CAP = 200 * 1024;

  it("clips a 5 MB reply chunk to its end, with a note", () => {
    const body = `${"y".repeat(5 * 1024 * 1024)}THE END`;
    const t = run([text(body)]);
    const reply = last(t);
    expect(reply.kind).toBe("reply");
    if (reply.kind !== "reply") return;
    expect(reply.text.startsWith(EARLIER)).toBe(true);
    expect(reply.text.endsWith("THE END")).toBe(true);
    expect(entrySize(reply)).toBeLessThanOrEqual(CAP);
    expect(t.bytes).toBe(measured(t));
  });

  it("replaces an open reply with the end of a huge chunk, keeping the bytes exact", () => {
    const t = run([user("q"), text("some earlier words"), text(`${"y".repeat(2 * 1024 * 1024)}THE END`)]);
    expect(t.entries).toHaveLength(2);
    expect(last(t)).toMatchObject({ kind: "reply", done: false });
    expect(t.bytes).toBe(measured(t));
    expect(entrySize(last(t))).toBeLessThanOrEqual(CAP);
    const more = apply(t, text(" and on"));
    expect(more.bytes).toBe(measured(more));
  });

  it("clips a 5 MB tool detail and a 5 MB user message", () => {
    const t = run([tool("1", "Bash", "cat big", "done", `${"z".repeat(5 * 1024 * 1024)}tail`), user(`${"u".repeat(5 * 1024 * 1024)}tail`)]);
    for (const e of t.entries) expect(entrySize(e)).toBeLessThanOrEqual(CAP);
    const call = tools(t.entries[0] as Entry).calls[0];
    expect(call?.detail.startsWith(EARLIER)).toBe(true);
    expect(call?.detail.endsWith("tail")).toBe(true);
    expect(t.entries[1]).toMatchObject({ kind: "user" });
    expect(t.bytes).toBe(measured(t));
  });

  it("keeps an entry within 200,000 bytes when parallel calls end with big details", () => {
    const events: KnownJournalEvent[] = [user("before"), started];
    for (let i = 0; i < 10; i++) events.push(tool(String(i), "Read", `f${i}`, "running"));
    for (let i = 0; i < 10; i++) events.push(tool(String(i), "Read", "", "done", `${i}`.repeat(32 * 1024)));
    const t = applyAll(EMPTY_TRANSCRIPT, events);
    expect(kinds(t)).toEqual(["user", "tools"]);
    expect(entrySize(last(t))).toBeLessThanOrEqual(200_000);
    expect(t.bytes).toBe(measured(t));
    // The early ends fit whole; the late ones are cut to their end behind the note.
    const calls = tools(last(t)).calls;
    expect(calls[0]?.detail).toBe("0".repeat(32 * 1024));
    const late = calls[9]?.detail ?? "";
    expect(late.length).toBeLessThan(32 * 1024);
    if (late !== "") {
      expect(late.startsWith(EARLIER)).toBe(true);
      expect(late.endsWith("9")).toBe(true);
    }
    expect(calls.every((c) => c.phase === "done")).toBe(true);
    expect(t.entries[0]).toMatchObject({ kind: "user", text: "before" });
  });

  it("keeps the accounting exact when many parallel ends overflow the entry", () => {
    const events: KnownJournalEvent[] = [];
    for (let i = 0; i < 300; i++) events.push(tool(String(i), "Read", `f${i}`, "running"));
    for (let i = 0; i < 300; i++) events.push(tool(String(i), "Read", "", "done", "x".repeat(60 * 1024)));
    const t = applyAll(EMPTY_TRANSCRIPT, events);
    for (const e of t.entries) expect(entrySize(e)).toBeLessThanOrEqual(200_000);
    expect(t.bytes).toBe(measured(t));
    expect(tools(t.entries[0] as Entry).calls).toHaveLength(300);
  });

  it("clips a huge target and name too", () => {
    const t = run([tool("1", "N".repeat(1_000_000), "T".repeat(1_000_000), "running")]);
    expect(entrySize(last(t))).toBeLessThanOrEqual(CAP);
  });

  it("clips multi-byte text by bytes, not characters", () => {
    const t = run([text("日".repeat(300_000))]);
    expect(entrySize(last(t))).toBeLessThanOrEqual(CAP);
    expect(t.bytes).toBe(measured(t));
  });

  it("keeps at most 1,000 entries, dropping the oldest, with exact bytes", () => {
    const events: KnownJournalEvent[] = [];
    for (let i = 0; i < 3_000; i++) events.push(user(`message ${i} ${"é".repeat(i % 7)}`));
    const t = applyAll(EMPTY_TRANSCRIPT, events);
    expect(MAX_ENTRIES).toBe(1_000);
    expect(t.entries).toHaveLength(1_000);
    expect(t.entries[0]).toMatchObject({ text: expect.stringMatching(/^message 2000 /) as unknown as string });
    expect(last(t)).toMatchObject({ text: expect.stringMatching(/^message 2999 /) as unknown as string });
    expect(t.bytes).toBe(measured(t));
    expect(new Set(t.entries.map((e) => e.key)).size).toBe(1_000);
  });

  it("drops whole old entries to stay inside the byte limit, never the newest", () => {
    const events: KnownJournalEvent[] = [];
    for (let i = 0; i < 100; i++) events.push(user("w".repeat(60 * 1024)));
    const t = applyAll(EMPTY_TRANSCRIPT, events);
    expect(t.entries.length).toBeLessThan(100);
    expect(t.entries.length).toBeGreaterThan(1);
    expect(t.bytes).toBe(measured(t));
    expect(t.bytes).toBeLessThanOrEqual(4 * 1024 * 1024);
  });

  it("applies a whole batch with the array copied once: older entries keep identity", () => {
    const base = run([user("a"), user("b")]);
    const t = applyAll(base, Array.from({ length: 500 }, (_, i) => user(`m${i}`)));
    expect(t.entries).toHaveLength(502);
    expect(t.entries[0]).toBe(base.entries[0]);
    expect(t.entries[1]).toBe(base.entries[1]);
    expect(base.entries).toHaveLength(2);
  });

  it("is quick: 20,000 mixed events apply well inside a second", () => {
    const events: KnownJournalEvent[] = [started];
    for (let i = 0; i < 5_000; i++) {
      events.push(tool(String(i), "Read", `f${i}`, "running"), tool(String(i), "Read", "", "done", "x".repeat(100)), text("some words "), user("u"));
    }
    const began = Date.now();
    const t = applyAll(EMPTY_TRANSCRIPT, events);
    expect(Date.now() - began).toBeLessThan(2_000);
    expect(t.entries.length).toBeLessThanOrEqual(1_000);
    expect(t.bytes).toBe(measured(t));
  });
});
