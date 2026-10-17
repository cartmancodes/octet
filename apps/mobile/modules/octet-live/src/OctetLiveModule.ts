import { NativeModule, requireOptionalNativeModule } from "expo";

/** The events exactly as the Swift module sends them; index.ts checks each one before passing it on. */
export type OctetLiveEvents = {
  pushToken: (event: unknown) => void;
  pushToStartToken: (event: unknown) => void;
  deny: (event: unknown) => void;
};

/** The content state as the Swift module takes it (a Record of the same five fields). */
export interface NativeActivityState {
  kind: string;
  session: string;
  mac: string;
  since: number;
  approvals: number;
}

declare class OctetLiveNative extends NativeModule<OctetLiveEvents> {
  isSupported(): boolean;
  start(state: NativeActivityState): Promise<void>;
  update(state: NativeActivityState): Promise<void>;
  end(): Promise<void>;
  /** Replaces the App Group's entries with this JSON object. */
  setEntries(json: string): Promise<void>;
  /** The app's send for Deny request `id` settled: ends its background task and acks it. */
  denySettled(id: string): void;
  /** The token hand-over in the background settled: ends its background task. */
  handOverSettled(): void;
}

/**
 * The native module, or null where there is none: Android (the module is iOS-only), Expo Go, jest
 * and the web. index.ts turns null into no-ops.
 */
export default requireOptionalNativeModule<OctetLiveNative>("OctetLive");
