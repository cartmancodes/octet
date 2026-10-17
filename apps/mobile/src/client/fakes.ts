// Test doubles for the hub client: a socket the test drives by hand and a clock that only moves
// when told to. Not imported by the app.
import type { Socket, SocketFactory } from "./HubClient";

export class FakeSocket implements Socket {
  sent: string[] = [];
  closed = false;
  onmessage?: (text: string) => void;
  onopen?: () => void;
  onclose?: (code: number) => void;

  constructor(readonly url: string) {}

  send(text: string): void {
    this.sent.push(text);
  }

  close(): void {
    this.closed = true;
  }

  /** The commands the client sent, parsed. */
  commands(): Record<string, unknown>[] {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }

  /** The commands of one type. */
  commandsOf(type: string): Record<string, unknown>[] {
    return this.commands().filter((c) => c.type === type);
  }

  open(): void {
    this.onopen?.();
  }

  /** The hub sends a frame (an object is stamped `v: 1`) or raw text. */
  receive(frame: object | string): void {
    const text = typeof frame === "string" ? frame : JSON.stringify({ v: 1, ...frame });
    this.onmessage?.(text);
  }

  /** The network or the hub ends the connection. */
  drop(code = 1006): void {
    this.onclose?.(code);
  }
}

export class Sockets {
  all: FakeSocket[] = [];

  readonly factory: SocketFactory = (url) => {
    const socket = new FakeSocket(url);
    this.all.push(socket);
    return socket;
  };

  get last(): FakeSocket {
    const socket = this.all[this.all.length - 1];
    if (!socket) throw new Error("no socket was opened");
    return socket;
  }
}

interface Timer {
  id: number;
  at: number;
  fn: () => void;
}

export class FakeClock {
  time = 1_000_000;
  private timers: Timer[] = [];
  private next = 1;

  readonly now = (): number => this.time;

  readonly schedule = (fn: () => void, ms: number): (() => void) => {
    const id = this.next++;
    this.timers.push({ id, at: this.time + ms, fn });
    return () => {
      this.timers = this.timers.filter((t) => t.id !== id);
    };
  };

  get pending(): number {
    return this.timers.length;
  }

  /** The delays of the timers waiting, soonest first. */
  get delays(): number[] {
    return this.timers.map((t) => t.at - this.time).sort((a, b) => a - b);
  }

  /** Moves time forward, running each timer that falls due in order. */
  advance(ms: number): void {
    const end = this.time + ms;
    for (;;) {
      const due = this.timers.filter((t) => t.at <= end).sort((a, b) => a.at - b.at || a.id - b.id)[0];
      if (!due) break;
      this.timers = this.timers.filter((t) => t.id !== due.id);
      this.time = Math.max(this.time, due.at);
      due.fn();
    }
    this.time = end;
  }
}
