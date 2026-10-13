// The app's real HubClient against the real `octet hub` binary: a fake bridge (a Unix socket, a
// registration and a journal, written the way the terminal UI writes them) stands in for a session.
//
// Test-only fakes, all in temp directories that afterAll removes:
// - `tailscale` on the hub's PATH answers `serve status --json` with the hub's own port, so the
//   hub sees itself as published. Nothing here touches the real Tailscale.
// - the WebSocket factory sets `Tailscale-User-Login`, which `tailscale serve` adds in real life.
//   The app itself never sends that header.
import { type ChildProcess, spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, appendFileSync, rmSync, writeFileSync, chmodSync } from "node:fs";
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

  close(): void {
    for (const conn of this.conns) conn.destroy();
    this.server.close();
  }
}

run("the app's hub client against the real octet hub", () => {
  let root = "";
  let binDir = "";
  let hub: ChildProcess | null = null;
  let hubLog = "";
  let port = 0;
  let sessionId = "";
  let bridge: FakeBridge | null = null;
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

  beforeAll(async () => {
    root = mkdtempSync(path.join(tmpdir(), "omc"));
    binDir = mkdtempSync(path.join(tmpdir(), "omb"));
    const sock = path.join(root, "run", "hub.sock");
    if (sock.length > 100) throw new Error(`the temp path is too long for a Unix socket: ${sock}`);
    port = await freePort();

    // A stand-in `tailscale`: `serve status --json` shows the hub published; anything else fails.
    const serveStatus = JSON.stringify({
      TCP: { "443": { HTTPS: true } },
      Web: { "mac.test.ts.net:443": { Handlers: { "/": { Proxy: `http://127.0.0.1:${port}` } } } },
    });
    const script = path.join(binDir, "tailscale");
    writeFileSync(
      script,
      `#!/bin/sh\nif [ "$1 $2 $3" = "serve status --json" ]; then printf '%s' '${serveStatus}'; else exit 1; fi\n`,
    );
    chmodSync(script, 0o755);

    // The fake bridge: a journal with three records, a socket, and the registration the hub finds.
    const runDir = path.join(root, "run");
    mkdirSync(runDir, { recursive: true });
    const pid = process.pid;
    sessionId = `${pid}-1790000000`;
    for (let i = 0; i < 3; i++) journal("text", `hello ${i}`);
    bridge = new FakeBridge(path.join(runDir, `${pid}.sock`), (line) => ({
      v: 1,
      type: "ack",
      req: line.req,
      message: line.type === "prompt" ? "queued" : "answered",
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
        workspace: "/work",
        model: "opus",
        mode: "ask",
        vendor_session: "",
        journals: [path.join(root, JOURNAL)],
        from_phone: false,
      }),
    );

    hub = spawn(BINARY, ["hub", "--journal-dir", root, "--port", String(port), "--owner", OWNER], {
      env: { ...process.env, PATH: `${binDir}:${process.env.PATH ?? ""}` },
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

  it("is unpaired once the Mac revokes the phone", async () => {
    const revoke = spawn(BINARY, ["hub", "revoke", PHONE, "--journal-dir", root], { stdio: "ignore" });
    await new Promise<void>((resolve) => revoke.once("exit", () => resolve()));
    await until("the client to be unpaired", () => client?.state === "unpaired", 10_000);
    expect(client?.token).toBeNull();
  });
});
