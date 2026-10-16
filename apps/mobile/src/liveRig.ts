// For the tests: the Live Activity's native module as the controller sees it (src/activity.ts),
// every call kept, and its events sent by hand. Not imported by the app.
import type { ActivityState, DenyEvent, Subscription, TokenEvent, WidgetEntry } from "../modules/octet-live";
import type { LiveApi } from "./activity";

type Call = { what: "start" | "update" | "end" | "setEntries"; at: number; arg?: unknown };

export class FakeLive implements LiveApi {
  supported = true;
  /** `start` rejects, as it does with Live Activities turned off on the phone. */
  failStart = false;
  calls: Call[] = [];
  private readonly tokens = new Set<(e: TokenEvent) => void>();
  private readonly starts = new Set<(e: TokenEvent) => void>();
  private readonly denies = new Set<(e: DenyEvent) => void>();

  constructor(private readonly now: () => number = Date.now) {}

  isSupported = (): boolean => this.supported;
  start = async (state: ActivityState): Promise<void> => {
    this.calls.push({ what: "start", at: this.now(), arg: state });
    if (this.failStart) throw new Error("Live Activities are off");
  };
  update = async (state: ActivityState): Promise<void> => {
    this.calls.push({ what: "update", at: this.now(), arg: state });
  };
  end = async (): Promise<void> => {
    this.calls.push({ what: "end", at: this.now() });
  };
  setEntries = async (entries: Record<string, WidgetEntry>): Promise<void> => {
    this.calls.push({ what: "setEntries", at: this.now(), arg: entries });
  };
  addPushTokenListener = (fn: (e: TokenEvent) => void): Subscription => add(this.tokens, fn);
  addPushToStartTokenListener = (fn: (e: TokenEvent) => void): Subscription => add(this.starts, fn);
  addDenyListener = (fn: (e: DenyEvent) => void): Subscription => add(this.denies, fn);

  /** ActivityKit hands over a running activity's token, or says it ended (null); `activity` is
   * its id, which the native module names. */
  pushToken(token: string | null, activity?: string): void {
    for (const fn of [...this.tokens]) fn(activity === undefined ? { token } : { token, activity });
  }
  /** ActivityKit hands over the push-to-start token. */
  pushToStartToken(token: string): void {
    for (const fn of [...this.starts]) fn({ token });
  }
  /** Deny is tapped in the widget. */
  deny(event: DenyEvent): void {
    for (const fn of [...this.denies]) fn(event);
  }
  /** How many listeners are added. */
  listening(): number {
    return this.tokens.size + this.starts.size + this.denies.size;
  }
  of(what: Call["what"]): Call[] {
    return this.calls.filter((c) => c.what === what);
  }
  /** Every start and update, in order. */
  shown(): Call[] {
    return this.calls.filter((c) => c.what === "start" || c.what === "update");
  }
}

function add<T>(set: Set<T>, fn: T): Subscription {
  set.add(fn);
  return { remove: () => void set.delete(fn) };
}
