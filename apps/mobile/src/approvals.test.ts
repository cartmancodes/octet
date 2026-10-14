import AsyncStorage from "@react-native-async-storage/async-storage";
import * as LocalAuthentication from "expo-local-authentication";

import { allowAll, approvalKey, createAnswerLedger } from "./approvals";
import { saveSettings } from "./persist";
import type { ApprovalInfo } from "./protocol";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;
const gist = (id: number): ApprovalInfo => ({ id, summary: { action: `Run ${id}`, command: "make", files: [], scope: "once" } });

beforeEach(async () => {
  await AsyncStorage.clear();
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
});

describe("allowAll", () => {
  it("asks Face ID once for the batch, then answers each in order", async () => {
    await saveSettings({ faceId: true });
    const order: number[] = [];
    const result = await allowAll([1, 2, 3], async (n) => void order.push(n));
    expect(result).toBe("ok");
    expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    expect(auth.authenticateAsync.mock.calls[0]?.[0]?.promptMessage).toBe("Allow 3 requests once");
    expect(order).toEqual([1, 2, 3]);
  });

  it("answers nothing when Face ID fails or is not set up", async () => {
    await saveSettings({ faceId: true });
    const answer = jest.fn(async () => undefined);
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
    expect(await allowAll([1, 2], answer)).toBe("failed");
    auth.isEnrolledAsync.mockResolvedValueOnce(false);
    expect(await allowAll([1, 2], answer)).toBe("unavailable");
    expect(answer).not.toHaveBeenCalled();
  });

  it("asks nothing with the setting off, and nothing for an empty batch", async () => {
    await saveSettings({ faceId: false });
    const answer = jest.fn(async () => undefined);
    expect(await allowAll([1], answer)).toBe("ok");
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    await saveSettings({ faceId: true });
    expect(await allowAll([], answer)).toBe("ok");
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    expect(answer).toHaveBeenCalledTimes(1);
  });

  it("one failed answer does not stop the rest", async () => {
    const order: number[] = [];
    await allowAll([1, 2], async (n) => {
      order.push(n);
      if (n === 1) throw new Error("refused");
    });
    expect(order).toEqual([1, 2]);
  });
});

describe("the answer ledger", () => {
  it("lets one answer out per request, and none once it was answered", () => {
    const ledger = createAnswerLedger();
    const key = approvalKey("s", gist(1));
    expect(ledger.claim(key)).toBe(true);
    expect(ledger.claim(key)).toBe(false);
    ledger.finish(key, false); // refused: it may be tried again
    expect(ledger.claim(key)).toBe(true);
    ledger.finish(key, true);
    expect(ledger.claim(key)).toBe(false);
  });

  it("publishes what it has given as a new set on each change, for a screen to count", () => {
    const ledger = createAnswerLedger();
    const key = approvalKey("s", gist(1));
    const heard = jest.fn();
    const off = ledger.subscribe(heard);
    const before = ledger.snapshot();
    ledger.claim(key);
    ledger.finish(key, false);
    expect(heard).not.toHaveBeenCalled();
    expect(ledger.snapshot()).toBe(before);
    ledger.claim(key);
    ledger.finish(key, true);
    expect(heard).toHaveBeenCalledTimes(1);
    expect(ledger.snapshot().has(key)).toBe(true);
    expect(ledger.given(key)).toBe(true);
    ledger.keep([]);
    expect(heard).toHaveBeenCalledTimes(2);
    expect(ledger.snapshot().has(key)).toBe(false);
    off();
  });

  it("forgets a request that is no longer listed, so one that comes back is new", () => {
    const ledger = createAnswerLedger();
    const key = approvalKey("s", gist(1));
    ledger.claim(key);
    ledger.finish(key, true);
    ledger.keep([approvalKey("s", gist(2))]);
    expect(ledger.claim(key)).toBe(true);
  });

  it("keys a request by its session, id and what it asks", () => {
    expect(approvalKey("s", gist(1))).not.toBe(approvalKey("t", gist(1)));
    expect(approvalKey("s", gist(1))).not.toBe(approvalKey("s", gist(2)));
    expect(approvalKey("s", gist(1))).not.toBe(approvalKey("s", { ...gist(1), summary: { ...gist(1).summary, command: "rm" } }));
  });
});
