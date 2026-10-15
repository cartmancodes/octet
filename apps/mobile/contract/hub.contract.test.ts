// The app's real HubClient against the real `octet hub` binary: a fake bridge (a Unix socket, a
// registration and a journal, written the way the terminal UI writes them) stands in for a session.
//
// Test-only fakes, all in temp directories that afterAll removes:
// - `tailscale` on the hub's PATH answers `serve status --json` with the hub's own port, so the
//   hub sees itself as published. Nothing here touches the real Tailscale. The hub tries
//   `/Applications/Tailscale.app/...` if `tailscale` on PATH fails (crates/octet-hub/src/owner.rs), so
//   the stand-in logs every call it gets and the test asserts the lookup reached it and nothing else
//   was asked of it; a stand-in that fails would fall through to the real app, and so fails the test.
// - `OCTET_HUB_PUSH_URL` points the hub's pushes at a local HTTP server instead of exp.host (the
//   hub honours it only for `http://127.0.0.1:<port>`). The hub posts with `curl`, so the real
//   one stays on its PATH behind the stand-in directory. Nothing here reaches Expo.
// - the WebSocket factory sets `Tailscale-User-Login`, which `tailscale serve` adds in real life.
//   The app itself never sends that header.
import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import http from "node:http";
import { existsSync, mkdirSync, mkdtempSync, appendFileSync, readFileSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";

import WebSocket from "ws";

import { type BatchItem, HubClient } from "../src/client/HubClient";
import type { EventFrame, ToPhone } from "../src/protocol";
import { nodeSocketFactory } from "./nodeSocket";

const BINARY = path.resolve(__dirname, "../../../target/debug/octet");
const OWNER = "test@example";
const PHONE = "Contract phone";
const JOURNAL = "session-1-1.jsonl";
const PUSH_TOKEN = "ExponentPushToken[contract-test-token]";
const SESSION_TEXT = "Claude Code · opus · ask mode · /work";
const run = existsSync(BINARY) ? describe : describe.skip;

type Obj = Record<string, unknown>;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** Polls `read` until it returns something, or fails with `what` after `ms`. */
async function until<T>(what: string, read: () => T | undefined | null | false, ms = 8000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const got = read();
    if (got) return got;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(20);
  }
}

async function freePort(): Promise<number> {
  const probe = net.createServer();
  await new Promise<void>((resolve) => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address() as net.AddressInfo;
  await new Promise<void>((resolve) => probe.close(() => resolve()));
  return port;
}

/** One request on the hub's control socket and its one-line answer. */
function control(socket: string, request: Obj): Promise<Obj> {
  return new Promise((resolve, reject) => {
    const conn = net.createConnection(socket);
    let text = "";
    conn.setTimeout(5000, () => conn.destroy(new Error("the control socket did not answer")));
    conn.on("connect", () => conn.write(`${JSON.stringify(request)}\n`));
    conn.on("data", (chunk) => {
      text += chunk.toString();
      if (text.includes("\n")) {
        conn.end();
        try {
          resolve(JSON.parse(text) as Obj);
        } catch (e) {
          reject(e as Error);
        }
      }
    });
    conn.on("error", reject);
  });
}

/** A local stand-in for Expo's push endpoint: keeps every body posted and answers `ok`. */
class PushFake {
  readonly bodies: string[] = [];
  readonly server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (chunk: Buffer) => (body += chunk.toString()));
    req.on("end", () => {
      this.bodies.push(body);
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ data: [{ status: "ok", id: "ticket" }] }));
    });
  });

  /** Listens on a free port and returns it. */
  listen(): Promise<number> {
    return new Promise((resolve) =>
      this.server.listen(0, "127.0.0.1", () => resolve((this.server.address() as net.AddressInfo).port)),
    );
  }

  close(): void {
    this.server.closeAllConnections();
    this.server.close();
  }
}

/** The bridge's `state` frame: the phase and the approvals open now. The hub's pusher reads these,
 * not the journal, and the summary here is what a push must never carry. */
function bridgeState(approvals: number[]): Obj {
  return {
    v: 1,
    type: "state",
    state: {
      phase: "idle",
      queue: [],
      queued: 0,
      mode: "ask",
      approvals: approvals.map((id) => ({
        id,
        summary: { action: "Run rm -rf /secret-dir", files: ["/work/private.txt"], scope: "This request only, once" },
      })),
    },
  };
}

/** The fake terminal UI's end of the hub's link: records what the hub sends, answers it. */
class FakeBridge {
  readonly lines: Obj[] = [];
  readonly server: net.Server;
  private readonly conns = new Set<net.Socket>();

  constructor(
    readonly socketPath: string,
    private readonly reply: (line: Obj) => Obj,
  ) {
    this.server = net.createServer((conn) => {
      this.conns.add(conn);
      conn.on("close", () => this.conns.delete(conn));
      conn.on("error", () => undefined);
      let pending = "";
      conn.on("data", (chunk) => {
        pending += chunk.toString();
        for (let end = pending.indexOf("\n"); end >= 0; end = pending.indexOf("\n")) {
          const line = JSON.parse(pending.slice(0, end)) as Obj;
          pending = pending.slice(end + 1);
          this.lines.push(line);
          conn.write(`${JSON.stringify(this.reply(line))}\n`);
        }
      });
    });
  }

  listen(): Promise<void> {
    return new Promise((resolve) => this.server.listen(this.socketPath, resolve));
  }

  /** A frame the bridge sends on its own, as the terminal UI does when its state changes. */
  send(frame: Obj): void {
    for (const conn of this.conns) conn.write(`${JSON.stringify(frame)}\n`);
  }

  close(): void {
    for (const conn of this.conns) conn.destroy();
    this.server.close();
  }
}

run("the app's hub client against the real octet hub", () => {
  let root = "";
  let binDir = "";
  let standInLog = "";
  let hub: ChildProcess | null = null;
  let hubLog = "";
  let port = 0;
  let sessionId = "";
  /** The session's workspace: a git repository with one commit, made in the temp root. */
  let workspace = "";
  let bridge: FakeBridge | null = null;
  let pushFake: PushFake | null = null;
  let seq = 0;
  let client: HubClient | null = null;
  const sockets: WebSocket[] = [];
  const batches: BatchItem[] = [];

  const journal = (type: string, data: unknown): number => {
    const record = { format: "octet-preview-1", seq, type, data };
    appendFileSync(path.join(root, JOURNAL), `${JSON.stringify(record)}\n`);
    return seq++;
  };
  const events = (): EventFrame[] => batches.filter((b): b is EventFrame => b.type === "event");
  const eventSeqs = (): number[] => events().map((e) => e.seq);
  const frames = (type: string): ToPhone[] => batches.filter((b): b is ToPhone => b.type === type);
  /** git in the workspace, as the test (not the hub) runs it, with no GIT_* of the caller's (a
   * hook running this test would point git at its own repository). */
  const gitEnv: NodeJS.ProcessEnv = { ...process.env };
  for (const name of Object.keys(gitEnv)) if (name.startsWith("GIT_")) delete gitEnv[name];
  const git = (args: string[]): string => execFileSync("git", ["-C", workspace, ...args], { encoding: "utf8", env: gitEnv });

  beforeAll(async () => {
    root = mkdtempSync(path.join(tmpdir(), "omc"));
    binDir = mkdtempSync(path.join(tmpdir(), "omb"));
    const sock = path.join(root, "run", "hub.sock");
    if (sock.length > 100) throw new Error(`the temp path is too long for a Unix socket: ${sock}`);
    port = await freePort();

    // A stand-in `tailscale`: `serve status --json` shows the hub published; anything else fails.
    // Each call's arguments are appended to `tailscale.log`, for the test to check.
    const serveStatus = JSON.stringify({
      TCP: { "443": { HTTPS: true } },
      Web: { "mac.test.ts.net:443": { Handlers: { "/": { Proxy: `http://127.0.0.1:${port}` } } } },
    });
    const script = path.join(binDir, "tailscale");
    standInLog = path.join(binDir, "tailscale.log");
    writeFileSync(
      script,
      `#!/bin/sh\nprintf '%s\\n' "$*" >> '${standInLog}'\n` +
        `if [ "$1 $2 $3" = "serve status --json" ]; then printf '%s' '${serveStatus}'; else exit 1; fi\n`,
    );
    chmodSync(script, 0o755);

    // The fake bridge: a journal with three records, a socket, and the registration the hub finds.
    const runDir = path.join(root, "run");
    mkdirSync(runDir, { recursive: true });
    const pid = process.pid;
    sessionId = `${pid}-1790000000`;
    for (let i = 0; i < 3; i++) journal("text", `hello ${i}`);
    // The workspace: a git repository with one committed file. The hub takes every GIT_* out of
    // its git's environment, so who commits is set in the repository's own config, with no
    // signing (a key the machine has would prompt or fail).
    workspace = path.join(root, "ws");
    mkdirSync(workspace);
    git(["init", "-q"]);
    git(["config", "user.name", "Contract Test"]);
    git(["config", "user.email", "contract@example.test"]);
    git(["config", "commit.gpgsign", "false"]);
    writeFileSync(path.join(workspace, "README.md"), "# Work\n\nFirst line.\n");
    git(["add", "README.md"]);
    git(["commit", "-q", "-m", "First"]);
    bridge = new FakeBridge(path.join(runDir, `${pid}.sock`), (line) => ({
      v: 1,
      type: "ack",
      req: line.req,
      // A desk command answers with its text, as the terminal UI's `session` does.
      message: line.type === "prompt" ? "queued" : line.type === "command" ? SESSION_TEXT : "answered",
    }));
    await bridge.listen();
    writeFileSync(
      path.join(runDir, `${pid}.json`),
      JSON.stringify({
        id: sessionId,
        pid,
        started: 1_790_000_000,
        socket: bridge.socketPath,
        engine: "claude",
        workspace,
        model: "opus",
        mode: "ask",
        vendor_session: "",
        journals: [path.join(root, JOURNAL)],
        from_phone: false,
      }),
    );

    pushFake = new PushFake();
    const pushPort = await pushFake.listen();

    hub = spawn(BINARY, ["hub", "--journal-dir", root, "--port", String(port), "--owner", OWNER], {
      env: {
        ...process.env,
        PATH: `${binDir}:${process.env.PATH ?? ""}`,
        // curl must reach the local fake directly, whatever proxy the machine's environment names.
        NO_PROXY: "127.0.0.1",
        no_proxy: "127.0.0.1",
        OCTET_HUB_PUSH_URL: `http://127.0.0.1:${pushPort}/--/api/v2/push/send`,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    process.once("exit", () => hub?.kill("SIGKILL"));
    hub.stdout?.on("data", (d: Buffer) => (hubLog += d.toString()));
    hub.stderr?.on("data", (d: Buffer) => (hubLog += d.toString()));
    await untilAsync("the hub's control socket", async () => {
      try {
        return (await control(sock, { type: "status" })).published === true;
      } catch {
        return false;
      }
    });
  });

  afterAll(async () => {
    client?.stop();
    for (const ws of sockets) ws.terminate();
    bridge?.close();
    pushFake?.close();
    if (hub && hub.exitCode === null) {
      const exited = new Promise<void>((resolve) => hub?.once("exit", () => resolve()));
      hub.kill("SIGTERM");
      const giveUp = setTimeout(() => hub?.kill("SIGKILL"), 3000);
      await exited;
      clearTimeout(giveUp);
    }
    if (root) rmSync(root, { recursive: true, force: true });
    if (binDir) rmSync(binDir, { recursive: true, force: true });
  });

  async function untilAsync(what: string, read: () => Promise<boolean>, ms = 8000): Promise<void> {
    const end = Date.now() + ms;
    while (!(await read())) {
      if (Date.now() > end) throw new Error(`timed out waiting for ${what}\n${hubLog}`);
      await sleep(50);
    }
  }

  // The tests below run in order, on purpose, and share state: the paired client, the session's
  // journal and the events collected so far. Each builds on the one before; run the file whole.
  it("refuses a connection without the tailnet login header, as the app never sends it", async () => {
    const status = await new Promise<number>((resolve, reject) => {
      const ws = new WebSocket(`ws://127.0.0.1:${port}/`);
      ws.on("unexpected-response", (_req, res) => resolve(res.statusCode ?? 0));
      ws.on("open", () => reject(new Error("the hub let a login-less connection in")));
      ws.on("error", () => undefined);
    });
    expect(status).toBe(403);
  });

  it("pairs, says hello and lists the session", async () => {
    const secret = await control(path.join(root, "run", "hub.sock"), { type: "pair_secret" });
    expect(typeof secret.secret).toBe("string");
    expect(secret.url).toBe("https://mac.test.ts.net");
    // The hub's publish lookup went to the stand-in, and asked it only the one read-only question.
    const asked = readFileSync(standInLog, "utf8").split("\n").filter((line) => line !== "");
    expect(asked.length).toBeGreaterThan(0);
    expect(asked.every((line) => line === "serve status --json")).toBe(true);

    const factory = nodeSocketFactory(OWNER, sockets);
    const url = `ws://127.0.0.1:${port}/`;
    const pairing = new HubClient({ url, token: null, factory });
    const paired = await pairing.pair(String(secret.secret), PHONE, "ios");
    expect(paired.device).toBe(PHONE);
    expect(paired.token).toHaveLength(64);

    client = new HubClient({ url, token: paired.token, factory });
    client.onBatch((items) => batches.push(...items));
    client.start();
    await until("the client to be ready", () => client?.state === "ready");
    const sessions = await until("the sessions frame", () => frames("sessions")[0]);
    if (sessions.type !== "sessions") throw new Error("not sessions");
    expect(sessions.sessions.map((s) => [s.id, s.engine])).toEqual([[sessionId, "claude"]]);
  });

  it("replays a session, then streams a live event", async () => {
    client?.subscribe(sessionId);
    await until("the replay to end", () => batches.some((b) => b.type === "replayed"));
    const subscribed = frames("subscribed")[0];
    expect(subscribed).toMatchObject({ session: sessionId, from: "start", journals: [JOURNAL] });
    expect(eventSeqs()).toEqual([0, 1, 2]);
    expect(events().map((e) => e.event)).toEqual([0, 1, 2].map((i) => ({ type: "text", data: `hello ${i}` })));

    const live = journal("text", "live");
    await until("the live event", () => eventSeqs().includes(live));
    expect(eventSeqs()).toEqual([0, 1, 2, 3]);
  });

  it("sends a prompt to the bridge and gets the ack", async () => {
    const ack = await client?.request({ type: "prompt", session: sessionId, text: "run the tests" });
    expect(ack).toMatchObject({ type: "ack", message: "queued" });
    const line = bridge?.lines.find((l) => l.type === "prompt");
    expect(line).toMatchObject({ type: "prompt", by: PHONE, text: "run the tests" });
  });

  it("shows an approval and answers it", async () => {
    const id = journal("approval", {
      id: 7,
      detail: "Run cargo test",
      summary: { action: "Run a command", command: "cargo test", files: [], scope: "once" },
    });
    await until("the approval event", () => eventSeqs().includes(id));
    const ack = await client?.request({ type: "answer", session: sessionId, approval: 7, allow: true });
    expect(ack).toMatchObject({ type: "ack", message: "answered" });
    expect(bridge?.lines.find((l) => l.type === "answer")).toMatchObject({ by: PHONE, approval: 7, allow: true });
  });

  it("reconnects with its cursor and gets only what is new", async () => {
    const before = eventSeqs();
    expect(before).toEqual([0, 1, 2, 3, 4]);
    const connections = sockets.length;
    sockets[sockets.length - 1]?.terminate(); // the network drops
    await until("the client to see it", () => client?.state !== "ready");
    journal("text", "while away"); // 5
    journal("text", "still away"); // 6
    await until("a new connection that is ready", () => sockets.length > connections && client?.state === "ready");
    await until("the missed events", () => eventSeqs().includes(6));
    await sleep(300); // anything repeated would arrive by now
    expect(eventSeqs()).toEqual([0, 1, 2, 3, 4, 5, 6]);
    expect(client?.duplicateEvents).toBe(0);
    expect(frames("subscribed").map((f) => (f.type === "subscribed" ? f.from : ""))).toEqual(["start", "cursor"]);
  });

  it("answers a ping with pong", async () => {
    const ack = await client?.request({ type: "ping" });
    expect(ack).toMatchObject({ type: "ack", message: "pong" });
  });

  it("runs a desk command through the hub: the bridge gets the name and the device, and its text comes back", async () => {
    const ack = await client?.request({ type: "command", session: sessionId, name: "session" });
    expect(ack).toMatchObject({ type: "ack", message: SESSION_TEXT });
    expect(bridge?.lines.find((l) => l.type === "command")).toMatchObject({ type: "command", by: PHONE, name: "session" });
  });

  it("refuses a command that is not on the closed list, and the bridge never hears it", async () => {
    const before = bridge?.lines.length;
    await expect(
      client?.request({ type: "command", session: sessionId, name: "rm" as never }),
    ).rejects.toMatchObject({ code: "invalid" });
    expect(bridge?.lines.length).toBe(before);
  });

  it("lists the workspace's changes, diffs a listed file and refuses an unlisted one", async () => {
    writeFileSync(path.join(workspace, "README.md"), "# Work\n\nEdited from the contract test.\n");
    writeFileSync(path.join(workspace, "notes.txt"), "one\ntwo\n");
    const changes = await client?.request({ type: "changes", session: sessionId });
    if (changes?.type !== "changes") throw new Error(`not a changes answer: ${JSON.stringify(changes)}`);
    expect(changes.files.map((f) => [f.path, f.status]).sort()).toEqual([
      ["README.md", "M"],
      ["notes.txt", "?"],
    ]);
    expect(changes.files.find((f) => f.path === "notes.txt")).toMatchObject({ add: 2, del: 0, binary: false });
    expect(changes.more).toBe(0);

    const diff = await client?.request({ type: "diff", session: sessionId, path: "README.md" });
    if (diff?.type !== "diff") throw new Error(`not a diff answer: ${JSON.stringify(diff)}`);
    expect(diff.path).toBe("README.md");
    expect(diff.text).toContain("+Edited from the contract test.");
    expect(diff.text).toContain("-First line.");
    expect(diff).toMatchObject({ clipped: false, binary: false });

    await expect(client?.request({ type: "diff", session: sessionId, path: "hub-audit.jsonl" })).rejects.toMatchObject({
      code: "invalid",
      message: "Ask for the changes first",
    });
  });

  it("commits everything once the bridge says the session is idle, then has nothing to commit", async () => {
    // The hub lets a phone commit only once the bridge has said its phase, idle, with no approval.
    const states = frames("state").length;
    bridge?.send(bridgeState([]));
    await until("the idle state", () => frames("state").length > states);

    const ack = await client?.request({ type: "commit", session: sessionId, message: "Edit from the phone\n\nWith a body." });
    expect(ack).toMatchObject({ type: "ack" });
    const message = (ack as { message?: unknown } | undefined)?.message;
    expect(message).toMatch(/^Committed [0-9a-f]{4,40}$/);
    const hash = String(message).slice("Committed ".length);
    const head = git(["rev-parse", "--short", "HEAD"]).trim();
    expect(head).toBe(hash);
    expect(git(["log", "-1", "--format=%B"]).trim()).toBe("Edit from the phone\n\nWith a body.");
    expect(git(["status", "--porcelain"])).toBe("");

    await expect(client?.request({ type: "commit", session: sessionId, message: "Again" })).rejects.toMatchObject({
      code: "invalid",
      message: "Nothing to commit",
    });
    // The audit names the phone and the commit, never its message.
    const audit = readFileSync(path.join(root, "hub-audit.jsonl"), "utf8");
    const commits = audit
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => JSON.parse(line) as Obj)
      .filter((line) => line.what === "commit");
    expect(commits).toEqual([expect.objectContaining({ device: PHONE, session: sessionId, what: "commit" })]);
    expect(audit).not.toContain("Edit from the phone");
    expect(audit).not.toContain("With a body");
  });

  it("turns notifications on, and pushes a content-free message once the phone is in the background", async () => {
    const on = await client?.request({ type: "register_push", token: PUSH_TOKEN });
    expect(on).toMatchObject({ type: "ack", message: "Notifications on" });

    // The app closes its socket in the background; the hub pushes only to a phone it cannot reach.
    client?.stop();
    await sleep(500);
    // The first state seeds what the hub has seen; the approval after it is news.
    bridge?.send(bridgeState([]));
    bridge?.send(bridgeState([8]));
    const body = await until("the push at the local fake", () => pushFake?.bodies[0], 15_000);
    expect(JSON.parse(body)).toEqual([
      {
        to: PUSH_TOKEN,
        title: "Octet",
        body: "Approval waiting",
        data: { session: sessionId },
        sound: "default",
        priority: "high",
        channelId: "approvals",
      },
    ]);
    // Nothing of the approval, the workspace, the model or the engine is in it, and it came once.
    for (const secret of ["/work", workspace, "rm -rf", "secret-dir", "private", "opus", "claude"]) {
      expect(body).not.toContain(secret);
    }
    await sleep(3500);
    expect(pushFake?.bodies).toHaveLength(1);
    // The token is in neither the hub's output nor its audit file. The audit file is the one the
    // hub writes: it holds this run's prompt and the push registration, so the check is not
    // passed by an empty or other file.
    expect(hubLog).not.toContain(PUSH_TOKEN);
    const audit = readFileSync(path.join(root, "hub-audit.jsonl"), "utf8");
    const whats = audit
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => (JSON.parse(line) as { what?: unknown }).what);
    expect(whats).toEqual(expect.arrayContaining(["prompt", "push on"]));
    expect(audit).not.toContain(PUSH_TOKEN);
  });

  it("turns notifications off", async () => {
    client?.start();
    await until("the client to be ready", () => client?.state === "ready");
    const off = await client?.request({ type: "register_push", token: null });
    expect(off).toMatchObject({ type: "ack", message: "Notifications off" });
  });

  it("registers with kinds: only the kinds asked for are pushed, an error under its own channel", async () => {
    const kinds = { approvals: false, errors: true, turns: false };
    const on = await client?.request({ type: "register_push", token: PUSH_TOKEN, kinds });
    expect(on).toMatchObject({ type: "ack", message: "Notifications on" });
    // A second registration with no kinds keeps them, as an older app's would.
    const again = await client?.request({ type: "register_push", token: PUSH_TOKEN });
    expect(again).toMatchObject({ type: "ack", message: "Notifications on" });

    client?.stop();
    await sleep(500);
    const pushes = pushFake?.bodies.length ?? 0;
    // An approval, which this device did not ask for: nothing is pushed for it.
    bridge?.send(bridgeState([]));
    bridge?.send(bridgeState([9]));
    // An error record in the journal: this is pushed, with nothing of its text.
    journal("error", "boom: /work/private.txt");
    const body = await until("the error push at the local fake", () => pushFake?.bodies[pushes], 15_000);
    expect(JSON.parse(body)).toEqual([
      {
        to: PUSH_TOKEN,
        title: "Octet",
        body: "Session error",
        data: { session: sessionId },
        priority: "high",
        channelId: "errors",
      },
    ]);
    expect(body).not.toContain("boom");
    expect(body).not.toContain("private");
    await sleep(3500);
    expect(pushFake?.bodies).toHaveLength(pushes + 1);

    // Asking for approvals too: the approval still open now is news the next time it changes.
    client?.start();
    await until("the client to be ready", () => client?.state === "ready");
    const both = await client?.request({
      type: "register_push",
      token: PUSH_TOKEN,
      kinds: { approvals: true, errors: true, turns: false },
    });
    expect(both).toMatchObject({ type: "ack", message: "Notifications on" });
  });

  it("is unpaired once the Mac revokes the phone", async () => {
    const revoke = spawn(BINARY, ["hub", "revoke", PHONE, "--journal-dir", root], { stdio: "ignore" });
    const code = await new Promise<number | null>((resolve, reject) => {
      revoke.once("error", reject);
      revoke.once("exit", (exitCode) => resolve(exitCode));
    });
    expect(code).toBe(0);
    await until("the client to be unpaired", () => client?.state === "unpaired", 10_000);
    expect(client?.token).toBeNull();
  });
});
