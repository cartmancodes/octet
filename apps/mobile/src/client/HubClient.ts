import type {
  AckFrame,
  Cursor,
  DiffFrame,
  FromPhone,
  RecentFrame,
  RefusalCode,
  RefusedFrame,
  ToPhone,
} from "../protocol";
import { PROTOCOL_VERSION } from "../protocol";
import { SHORT_CHARS, field } from "../model/text";
import { cleanChanges, cleanDiff, type ChangesAnswer } from "./answers";
import { Backoff } from "./backoff";
import { Cursors } from "./cursors";
import { parseFrame } from "./frames";

export type HubState = "connecting" | "authenticating" | "ready" | "offline" | "unpaired";

/** What the client needs of a WebSocket; React Native wraps the global one, tests use a fake. */
export interface Socket {
  send(text: string): void;
  close(): void;
  onmessage?: (text: string) => void;
  onopen?: () => void;
  onclose?: (code: number) => void;
}
export type SocketFactory = (url: string) => Socket;

/** Not a wire frame: the client's note, in a batch, that a session's replay is over (the hub
 * acks a subscribe after the replay's last record, and the ack itself stays in the client). */
export interface ReplayedMark {
  type: "replayed";
  session: string;
}

/** What a batch carries: the hub's frames, in order, and the client's replayed marks among them. */
export type BatchItem = ToPhone | ReplayedMark;

/** What answers a request: `ack`, or the frame of the same name for `recent`, `changes` and
 * `diff` (the last two cleaned, answers.ts). */
export type Ack = AckFrame | RecentFrame | ChangesAnswer | DiffFrame;

/**
 * A command that was not done. `code` is the hub's, or one of two the phone makes up:
 * - `unsent`: the command never left the phone (no link yet, or the socket was already closed).
 *   It is safe to offer the user a resend.
 * - `lost`: it was sent and the outcome is unknown (the link dropped, or no answer in 45 s). Never
 *   resend automatically, and offer a resend only after the user has looked at the session.
 * The hub's own `timeout` (a session that did not answer) is a `lost` too. Any other hub code is
 * a definite no: `busy`, `rate_limited` and `unavailable` may be offered again later, the rest
 * (`not_allowed`, `invalid`, `turn_open`, ...) only after something changed.
 */
export class Refusal extends Error {
  readonly code: RefusalCode | "unsent" | "lost";

  constructor(code: RefusalCode | "unsent" | "lost", reason: string) {
    super(reason);
    Object.setPrototypeOf(this, new.target.prototype);
    this.name = "Refusal";
    this.code = code;
  }
}

/** The commands `request` sends, without the `v` and `req` it adds. Connection and
 * subscription commands have their own methods. */
export type RequestBody = FromPhone extends infer C
  ? C extends { type: "hello" | "pair" | "subscribe" | "unsubscribe" }
    ? never
    : C extends FromPhone
      ? Omit<C, "v" | "req">
      : never
  : never;

export interface HubClientOptions {
  url: string;
  /** The device token, or null before pairing. */
  token: string | null;
  factory: SocketFactory;
  now?: () => number;
  /** Runs `fn` after `ms`; returns the function that cancels it. */
  schedule?: (fn: () => void, ms: number) => () => void;
  /** [0, 1), for the backoff's jitter. */
  random?: () => number;
  /** Cursors saved by the store, to resume from. */
  cursors?: Record<string, Cursor>;
  /** A session's cursor moved (or, null, was cleared): the store saves it. Called after the
   * frames up to that cursor were handed to the batch listeners, at most once per session per batch. */
  onCursor?: (session: string, cursor: Cursor | null) => void;
}

/** A `rate_limited` or `timeout` refusal of a subscribe is retried this many times, this far apart. */
export const SUBSCRIBE_RETRIES = 3;
export const SUBSCRIBE_RETRY_MS = 5_000;

/** Past the hub's own deadline for an answer (30 s, swept every 1 s), and the trip back. */
export const REQUEST_TIMEOUT_MS = 45_000;
/** A request that times out takes the link down only if nothing came over it for this long. */
export const QUIET_MS = 10_000;
export const HANDSHAKE_TIMEOUT_MS = 15_000;
export const BATCH_INTERVAL_MS = 50;
/** A link ready this long counts as good: its drop starts the backoff over. */
export const STABLE_MS = 10_000;
/** While ready, a `ping` goes out this often; with no answer within `PING_TIMEOUT_MS` the link is dead. */
export const PING_INTERVAL_MS = 25_000;
export const PING_TIMEOUT_MS = 10_000;

interface Pending {
  resolve: (ack: Ack) => void;
  reject: (error: Refusal) => void;
  cancel: () => void;
  /** The most characters of a refusal's words kept. */
  words: number;
}

const defaultSchedule = (fn: () => void, ms: number): (() => void) => {
  const id = setTimeout(fn, ms);
  return () => clearTimeout(id);
};

/** The most UTF-16 units kept of a commit's refusal: git's own words, which the hub clips to the
 * last 2,000 characters (`GIT_WORDS`, crates/octet-hub/src/git.rs, counted in Unicode scalars),
 * where a hook puts its summary. A scalar is at most two units, so the hub's words come whole. */
export const COMMIT_WORDS_CHARS = 4_000;

/** A refusal's words, as the user will read them: the hub's, cleaned and clipped to `limit`. */
const refusalOf = (frame: RefusedFrame, limit = SHORT_CHARS): Refusal => new Refusal(frame.code ?? "invalid", field(frame.reason, limit));

const isUnpaired = (code: RefusalCode | null | undefined): boolean =>
  code === "unpaired" || code === "revoked";

/**
 * The phone's side of the hub protocol, with no React Native in it.
 *
 * `start()` connects, says `hello` with the device token, and on `welcome` is `ready` and
 * re-subscribes every wanted session after its cursor. A lost connection reconnects with a
 * jittered backoff (started over once a link has been ready for 10 s) until `stop()` (the app went to the background), which closes the socket
 * and leaves no timer behind. Only a `refused` with code `unpaired` or `revoked` forgets the
 * token and ends in the `unpaired` state; a closed socket never does.
 *
 * Frames reach `onBatch` listeners in order, at most once per 50 ms. A session's events are
 * deduplicated by `(journal, seq)`. `subscribed` frames are passed on as they come, so a
 * consumer sees `from: "start"` (a reset) in its place in the stream.
 *
 * While ready, a keepalive `ping` goes out every 25 s; no answer within 10 s takes the link for
 * dead, as a request timeout does. Pings are the client's own: they are never in a batch, and no
 * caller's request rides on them.
 */
export class HubClient {
  /** The hub's address, wss://... */
  readonly url: string;
  private tokenValue: string | null;
  private readonly factory: SocketFactory;
  private readonly now: () => number;
  private readonly schedule: (fn: () => void, ms: number) => () => void;
  private readonly onCursor?: (session: string, cursor: Cursor | null) => void;
  private readonly backoff: Backoff;
  private readonly cursors: Cursors;

  private stateValue: HubState;
  private running = false;
  /** How many times `start()` has started the client. */
  private started = 0;
  private socket: Socket | null = null;
  private nextReq = 1;
  private cancelReconnect: (() => void) | null = null;
  private cancelHandshake: (() => void) | null = null;
  /** When this connection said welcome; null before. */
  private readyAt: number | null = null;
  /** The next ping, or the deadline of the one sent; at most one is ever armed. */
  private cancelPing: (() => void) | null = null;
  /** The req of the ping awaiting its answer; null between pings. */
  private pingReq: number | null = null;
  /** When a frame last came in on the current socket. */
  private lastHeard = 0;

  private readonly wanted = new Set<string>();
  private readonly pending = new Map<number, Pending>();
  /** Subscribe requests in flight: req -> session. */
  private readonly subscribeReqs = new Map<number, string>();
  /** Sessions with a subscribe sent on this connection and not since unsubscribed or refused. */
  private readonly live = new Set<string>();
  /** Unsubscribes awaiting their ack: req -> session. The ack shares the hub's FIFO outbox with
   * live events, so no event of the old subscription can follow it; until then the session's
   * frames are dropped and a new subscribe waits. */
  private readonly settling = new Map<number, string>();
  private readonly retries = new Map<string, number>();
  private readonly retryTimers = new Map<string, () => void>();
  private readonly subscribeFailedListeners = new Set<(session: string, refusal: Refusal) => void>();
  private readonly pairings = new Set<() => void>();

  private buffer: BatchItem[] = [];
  private readonly dirty = new Set<string>();
  private cancelFlush: (() => void) | null = null;
  private lastFlush = Number.NEGATIVE_INFINITY;

  private readonly batchListeners = new Set<(frames: BatchItem[]) => void>();
  private readonly stateListeners = new Set<(state: HubState) => void>();

  private garbage = 0;
  private duplicates = 0;

  constructor(opts: HubClientOptions) {
    this.url = opts.url;
    this.tokenValue = opts.token;
    this.factory = opts.factory;
    this.now = opts.now ?? Date.now;
    this.schedule = opts.schedule ?? defaultSchedule;
    this.onCursor = opts.onCursor;
    this.backoff = new Backoff(opts.random);
    this.cursors = new Cursors(opts.cursors);
    this.stateValue = opts.token === null ? "unpaired" : "offline";
  }

  get state(): HubState {
    return this.stateValue;
  }

  /** Which run the client is in: a new number at each `start()` that connects, and null while it
   * is stopped (by `stop()`, or the hub refusing the token). Every attempt to connect is of a run. */
  get run(): number | null {
    return this.running ? this.started : null;
  }

  /** The device token; null before pairing and after the hub unpaired this phone. */
  get token(): string | null {
    return this.tokenValue;
  }

  /** Frames dropped for being too big, not JSON, of an unknown type or version, or malformed. */
  get garbageFrames(): number {
    return this.garbage;
  }

  /** Events dropped as duplicates of one already taken. */
  get duplicateEvents(): number {
    return this.duplicates;
  }

  /** Pairs with the Mac. Uses its own connection: the hub leaves a paired socket
   * unauthenticated, so `start()` then connects and says hello with the new token. */
  pair(secret: string, name: string, platform: string): Promise<{ token: string; device: string }> {
    return new Promise((resolve, reject) => {
      const socket = this.factory(this.url);
      const req = this.nextReq++;
      let done = false;
      const cancelTimer = this.schedule(
        () => finish(() => reject(new Refusal("lost", "The Mac did not answer in time"))),
        REQUEST_TIMEOUT_MS,
      );
      const finish = (settle: () => void): void => {
        if (done) return;
        done = true;
        cancelTimer();
        this.pairings.delete(abort);
        socket.onopen = socket.onmessage = socket.onclose = undefined;
        try {
          socket.close();
        } catch {
          // Already closed.
        }
        settle();
      };
      const abort = (): void => finish(() => reject(new Refusal("lost", "Can't reach your Mac")));
      this.pairings.add(abort);

      socket.onopen = () => {
        const command = { v: PROTOCOL_VERSION, type: "pair", req, secret, name, platform };
        try {
          socket.send(JSON.stringify(command));
        } catch {
          finish(() => reject(new Refusal("unsent", "Can't reach your Mac")));
        }
      };
      socket.onmessage = (text) => {
        const frame = parseFrame(text);
        if (!frame) {
          this.garbage += 1;
        } else if (frame.type === "paired") {
          this.tokenValue = frame.token;
          finish(() => resolve({ token: frame.token, device: frame.device }));
        } else if (frame.type === "refused") {
          finish(() => reject(refusalOf(frame)));
        }
      };
      socket.onclose = abort;
    });
  }

  /** Connects (foreground). Does nothing when running already, or with no token. */
  start(): void {
    if (this.running || this.tokenValue === null) return;
    this.running = true;
    this.started += 1;
    this.backoff.reset();
    this.connect();
  }

  /** Goes to the background: delivers what is buffered, closes the socket, cancels every timer. */
  stop(): void {
    this.running = false;
    this.cancelReconnect?.();
    this.cancelReconnect = null;
    this.cancelHandshake?.();
    this.cancelHandshake = null;
    for (const abort of [...this.pairings]) abort();
    this.flush();
    this.teardown();
    if (this.stateValue !== "unpaired") this.setState("offline");
  }

  /** Connects now instead of waiting out the backoff (the user pulled to refresh). A link that is
   * up or opening is left alone; a stopped client starts. */
  retry(): void {
    if (!this.running) {
      this.start();
      return;
    }
    if (this.cancelReconnect === null) return;
    this.cancelReconnect();
    this.cancelReconnect = null;
    this.connect();
  }

  /** Sends a command and resolves with its `ack` (or `recent`, `changes` or `diff`) frame.
   * Rejects with a `Refusal`: the hub's code and words, `lost` after 45 s, or `unsent` when not
   * ready. The hub answers a command it gave up on (30 s, swept every second) with its own
   * `timeout`, so the phone waits longer than that. A request with no answer in 45 s on a link
   * that has also been silent for 10 s takes the link for dead: the socket is closed and the
   * client reconnects with its backoff, resuming every session from its cursor. A link that is
   * still talking is kept. */
  request(cmd: RequestBody): Promise<Ack> {
    const socket = this.socket;
    if (this.stateValue !== "ready" || !socket) {
      return Promise.reject(new Refusal("unsent", "Can't reach your Mac"));
    }
    const req = this.nextReq++;
    return new Promise<Ack>((resolve, reject) => {
      const cancel = this.schedule(() => {
        this.pending.delete(req);
        reject(new Refusal("lost", "The Mac did not answer in time"));
        // A hub that is up answers in time; a half-open link (Wi-Fi gone, the Mac asleep) only
        // looks open, and is silent. Its late answers die with it. One that spoke lately is alive.
        if (this.socket === socket && this.now() - this.lastHeard >= QUIET_MS) this.lost();
      }, REQUEST_TIMEOUT_MS);
      const words = cmd.type === "commit" ? COMMIT_WORDS_CHARS : SHORT_CHARS;
      this.pending.set(req, { resolve, reject, cancel, words });
      try {
        socket.send(JSON.stringify({ ...cmd, v: PROTOCOL_VERSION, req }));
      } catch {
        this.pending.delete(req);
        cancel();
        reject(new Refusal("unsent", "Can't reach your Mac"));
      }
    });
  }

  /** Streams a session: replays after its cursor, then live, and again after every reconnect.
   * `fresh` forgets the cursor first, for a store that holds no transcript to resume. A session
   * already subscribed on this connection is unsubscribed first, and the new subscribe waits
   * for that ack so that no stale live event lands among the replay. */
  subscribe(session: string, fresh = false): void {
    if (fresh) {
      this.cursors.forget(session);
      this.dirty.add(session);
    }
    this.wanted.add(session);
    if (this.stateValue !== "ready") return;
    if (this.isSettling(session)) return; // the ack of the unsubscribe sends it
    this.cancelRetry(session);
    if (this.live.has(session)) {
      if (fresh) this.sendUnsubscribe(session);
      return;
    }
    this.sendSubscribe(session);
  }

  /** Stops streaming a session. Its cursor is kept for a later subscribe. */
  unsubscribe(session: string): void {
    if (!this.wanted.delete(session)) return;
    this.retries.delete(session);
    this.cancelRetry(session);
    if (this.stateValue === "ready" && this.live.has(session)) this.sendUnsubscribe(session);
  }

  /** Unsubscribes and drops the session's cursor, for a session that ended. */
  forget(session: string): void {
    this.unsubscribe(session);
    this.cursors.forget(session);
    this.dirty.add(session);
  }

  /** Called when a subscribe is refused for good: at once for most codes, and for
   * `rate_limited` or `timeout` after three retries 5 s apart. The session stays wanted; the
   * consumer decides whether to `forget` it. */
  onSubscribeFailed(fn: (session: string, refusal: Refusal) => void): () => void {
    this.subscribeFailedListeners.add(fn);
    return () => this.subscribeFailedListeners.delete(fn);
  }

  /** Called with each batch of frames, at most once per 50 ms. Returns the unsubscribe function. */
  onBatch(fn: (frames: BatchItem[]) => void): () => void {
    this.batchListeners.add(fn);
    return () => this.batchListeners.delete(fn);
  }

  onState(fn: (state: HubState) => void): () => void {
    this.stateListeners.add(fn);
    return () => this.stateListeners.delete(fn);
  }

  // Connection

  private connect(): void {
    const socket = this.factory(this.url);
    this.socket = socket;
    this.setState("connecting");
    this.cancelHandshake = this.schedule(() => {
      this.cancelHandshake = null;
      if (this.socket !== socket) return;
      socket.close();
      this.lost();
    }, HANDSHAKE_TIMEOUT_MS);

    // A socket that is no longer `this.socket` was replaced or stopped; what it still says is moot.
    socket.onopen = () => {
      if (this.socket !== socket || this.tokenValue === null) return;
      this.setState("authenticating");
      socket.send(
        JSON.stringify({ v: PROTOCOL_VERSION, type: "hello", req: this.nextReq++, token: this.tokenValue }),
      );
    };
    socket.onmessage = (text) => {
      if (this.socket === socket) this.handle(text);
    };
    socket.onclose = () => {
      if (this.socket === socket) this.lost();
    };
  }

  /** The socket is gone (or given up on): fail what waits, and come back if still wanted. */
  private lost(): void {
    // A link that was good for a while starts the climb over; one that drops just after its
    // welcome (a flapping network, a hub in a crash loop) keeps climbing.
    if (this.readyAt !== null && this.now() - this.readyAt >= STABLE_MS) this.backoff.reset();
    this.teardown();
    if (!this.running || this.stateValue === "unpaired") return;
    this.setState("offline");
    this.cancelReconnect = this.schedule(() => {
      this.cancelReconnect = null;
      if (this.running) this.connect();
    }, this.backoff.next());
  }

  /** Detaches and closes the socket, and fails every request that waits on it. */
  private teardown(): void {
    this.cancelHandshake?.();
    this.cancelHandshake = null;
    this.cancelPing?.();
    this.cancelPing = null;
    this.pingReq = null;
    this.readyAt = null;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = undefined;
      try {
        socket.close();
      } catch {
        // Already closed.
      }
    }
    for (const [, entry] of this.pending) {
      entry.cancel();
      entry.reject(new Refusal("lost", "Can't reach your Mac"));
    }
    this.pending.clear();
    // A new connection has no stale frames and no subscriptions.
    this.subscribeReqs.clear();
    this.settling.clear();
    this.live.clear();
    this.retries.clear();
    for (const cancel of this.retryTimers.values()) cancel();
    this.retryTimers.clear();
  }

  /** Arms the keepalive: a ping when the link has been silent for `delay` (the full interval after
   * a welcome or an answer). Any inbound frame is proof of life, so a link that spoke since is not
   * pinged yet, and one that spoke since a ping went out is not dead: a pong queued behind a heavy
   * replay is no reason to drop the link. */
  private armPing(delay = PING_INTERVAL_MS): void {
    this.cancelPing?.();
    const socket = this.socket;
    this.cancelPing = this.schedule(() => {
      this.cancelPing = null;
      if (this.socket !== socket) return;
      const quiet = this.now() - this.lastHeard;
      if (quiet < PING_INTERVAL_MS) {
        this.armPing(PING_INTERVAL_MS - quiet);
        return;
      }
      const sentAt = this.now();
      this.pingReq = this.send({ type: "ping" });
      if (this.pingReq === null) {
        this.lost(); // a socket that cannot send is as good as dead
        return;
      }
      this.cancelPing = this.schedule(() => {
        this.cancelPing = null;
        this.pingReq = null;
        if (this.socket !== socket) return;
        // It spoke since the ping: count the quiet from its last frame, not from now, so that a link
        // that went silent just after speaking is still found dead within ~35 s of that frame.
        if (this.lastHeard > sentAt) this.armPing(Math.max(0, PING_INTERVAL_MS - (this.now() - this.lastHeard)));
        else this.lost();
      }, PING_TIMEOUT_MS);
    }, delay);
  }

  /** True when `req` answers the ping in flight (and then the next one is armed). */
  private pinged(req: number): boolean {
    if (this.pingReq === null || req !== this.pingReq) return false;
    this.pingReq = null;
    this.armPing();
    return true;
  }

  private unpair(): void {
    this.tokenValue = null;
    this.running = false;
    this.cancelReconnect?.();
    this.cancelReconnect = null;
    this.flush();
    this.teardown();
    this.setState("unpaired");
  }

  private setState(state: HubState): void {
    if (state === this.stateValue) return;
    this.stateValue = state;
    for (const fn of [...this.stateListeners]) {
      try {
        fn(state);
      } catch {
        // One listener's failure is not the connection's.
      }
    }
  }

  // Frames

  private handle(text: string): void {
    this.lastHeard = this.now();
    const frame = parseFrame(text);
    if (!frame) {
      this.garbage += 1;
      return;
    }
    switch (frame.type) {
      case "welcome":
        this.cancelHandshake?.();
        this.cancelHandshake = null;
        this.readyAt = this.now();
        this.setState("ready");
        this.armPing();
        for (const session of this.wanted) this.sendSubscribe(session);
        return;
      case "paired":
        return; // only answers a pairing, which has its own connection
      case "ack":
      case "recent": {
        if (this.pinged(frame.req)) return;
        const entry = this.pending.get(frame.req);
        const subscribed = this.subscribeReqs.get(frame.req);
        if (entry) {
          this.pending.delete(frame.req);
          entry.cancel();
          entry.resolve(frame);
        } else if (subscribed !== undefined) {
          this.subscribeReqs.delete(frame.req);
          this.retries.delete(subscribed);
          if (this.wanted.has(subscribed) && !this.isSettling(subscribed)) this.queue({ type: "replayed", session: subscribed });
        } else {
          this.settled(frame.req); // else a late answer to a request that timed out: dropped
        }
        return;
      }
      case "changes":
      case "diff": {
        // Only ever an answer to a request: a late one (it timed out) is dropped.
        if (this.pinged(frame.req)) return;
        const entry = this.pending.get(frame.req);
        if (!entry) return;
        this.pending.delete(frame.req);
        entry.cancel();
        entry.resolve(frame.type === "changes" ? cleanChanges(frame) : cleanDiff(frame));
        return;
      }
      case "refused":
        this.refused(frame);
        return;
      case "event":
        if (!this.wanted.has(frame.session) || this.isSettling(frame.session)) return;
        if (this.cursors.accept(frame.session, frame.journal, frame.seq)) {
          this.dirty.add(frame.session);
          this.queue(frame);
        } else {
          this.duplicates += 1;
        }
        return;
      case "subscribed":
        if (!this.wanted.has(frame.session) || this.isSettling(frame.session)) return;
        this.retries.delete(frame.session);
        this.cursors.apply(frame.session, frame.from, frame.journals);
        this.dirty.add(frame.session);
        this.queue(frame);
        return;
      default:
        this.queue(frame);
    }
  }

  private refused(frame: RefusedFrame): void {
    if (this.pinged(frame.req)) return; // any answer shows the link is alive
    const entry = this.pending.get(frame.req);
    const session = this.subscribeReqs.get(frame.req);
    if (entry) {
      this.pending.delete(frame.req);
      entry.cancel();
      entry.reject(refusalOf(frame, entry.words));
    } else if (session !== undefined) {
      this.subscribeReqs.delete(frame.req);
      this.live.delete(session);
      if (!isUnpaired(frame.code)) this.subscribeRefused(session, frame);
    } else if (!this.settled(frame.req) && frame.req === 0) {
      this.queue(frame); // unsolicited (a revoked phone) or to a frame the hub could not parse
    }
    if (isUnpaired(frame.code)) this.unpair();
  }

  private subscribeRefused(session: string, frame: RefusedFrame): void {
    const tries = this.retries.get(session) ?? 0;
    const transient = frame.code === "rate_limited" || frame.code === "timeout";
    if (transient && tries < SUBSCRIBE_RETRIES && this.wanted.has(session)) {
      this.retries.set(session, tries + 1);
      this.cancelRetry(session);
      this.retryTimers.set(
        session,
        this.schedule(() => {
          this.retryTimers.delete(session);
          const idle = !this.live.has(session) && !this.isSettling(session);
          if (this.stateValue === "ready" && this.wanted.has(session) && idle) this.sendSubscribe(session);
        }, SUBSCRIBE_RETRY_MS),
      );
      return;
    }
    this.retries.delete(session);
    const refusal = refusalOf(frame);
    for (const fn of [...this.subscribeFailedListeners]) {
      try {
        fn(session, refusal);
      } catch {
        // One listener's failure is not the connection's.
      }
    }
  }

  private isSettling(session: string): boolean {
    for (const waiting of this.settling.values()) if (waiting === session) return true;
    return false;
  }

  private cancelRetry(session: string): void {
    this.retryTimers.get(session)?.();
    this.retryTimers.delete(session);
  }

  /** An unsubscribe was answered (ack or refusal): subscribe again if the session is wanted. */
  private settled(req: number): boolean {
    const session = this.settling.get(req);
    if (session === undefined) return false;
    this.settling.delete(req);
    if (this.stateValue === "ready" && this.wanted.has(session) && !this.isSettling(session)) {
      this.sendSubscribe(session);
    }
    return true;
  }

  private sendSubscribe(session: string): void {
    const after = this.cursors.cursor(session);
    const req = this.send(after ? { type: "subscribe", session, after } : { type: "subscribe", session });
    if (req === null) return;
    this.subscribeReqs.set(req, session);
    this.live.add(session);
  }

  private sendUnsubscribe(session: string): void {
    const req = this.send({ type: "unsubscribe", session });
    this.live.delete(session);
    if (req !== null) this.settling.set(req, session);
    else if (this.wanted.has(session)) this.sendSubscribe(session);
  }

  /** Sends a command whose answer the caller does not await; returns its req, or null when unsent. */
  private send(cmd: { type: string } & Record<string, unknown>): number | null {
    const socket = this.socket;
    if (!socket) return null;
    const req = this.nextReq++;
    try {
      socket.send(JSON.stringify({ ...cmd, v: PROTOCOL_VERSION, req }));
      return req;
    } catch {
      return null;
    }
  }

  // Batching

  private queue(frame: BatchItem): void {
    this.buffer.push(frame);
    if (this.cancelFlush) return;
    const wait = Math.max(0, this.lastFlush + BATCH_INTERVAL_MS - this.now());
    this.cancelFlush = this.schedule(() => {
      this.cancelFlush = null;
      this.flush();
    }, wait);
  }

  /** Hands the buffered frames to the listeners, then reports the cursors they advanced. */
  private flush(): void {
    this.cancelFlush?.();
    this.cancelFlush = null;
    if (this.buffer.length === 0 && this.dirty.size === 0) return;
    const frames = this.buffer;
    this.buffer = [];
    const sessions = [...this.dirty];
    this.dirty.clear();
    this.lastFlush = this.now();
    if (frames.length > 0) {
      for (const fn of [...this.batchListeners]) {
        try {
          fn(frames);
        } catch {
          // One listener's failure is not the connection's.
        }
      }
    }
    for (const session of sessions) {
      try {
        this.onCursor?.(session, this.cursors.cursor(session));
      } catch {
        // The store's failure to save is not the connection's.
      }
    }
  }
}
