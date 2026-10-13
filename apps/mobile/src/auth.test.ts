import * as LocalAuthentication from "expo-local-authentication";

import { confirmBiometric } from "./auth";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

beforeEach(() => {
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
});

describe("confirmBiometric", () => {
  it("is ok when the user passes, and shows the reason", async () => {
    expect(await confirmBiometric("Allow: Run tests")).toBe("ok");
    expect(auth.authenticateAsync).toHaveBeenCalledWith({ promptMessage: "Allow: Run tests" });
  });

  it("fails when the user cancels or does not match", async () => {
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
    expect(await confirmBiometric("x")).toBe("failed");
    auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "authentication_failed" });
    expect(await confirmBiometric("x")).toBe("failed");
  });

  it("is unavailable, without a prompt, with no hardware or nothing enrolled", async () => {
    auth.hasHardwareAsync.mockResolvedValueOnce(false);
    expect(await confirmBiometric("x")).toBe("unavailable");
    auth.isEnrolledAsync.mockResolvedValueOnce(false);
    expect(await confirmBiometric("x")).toBe("unavailable");
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
  });

  it("fails, never throws, when the module does", async () => {
    auth.authenticateAsync.mockRejectedValueOnce(new Error("native"));
    expect(await confirmBiometric("x")).toBe("failed");
  });
});
