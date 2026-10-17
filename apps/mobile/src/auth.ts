// Face ID, the fingerprint or the device passcode, before an action a phone must not take by
// accident: allowing an agent's request, turning on full access. `expo-local-authentication`
// stays behind this one function.
import * as LocalAuthentication from "expo-local-authentication";

/** `ok`: the user proved it is them. `failed`: they did not, or cancelled. `unavailable`: this
 * phone has no biometrics, or none enrolled, so the prompt cannot be shown at all. */
export type BiometricResult = "ok" | "failed" | "unavailable";

/** Asks the user to authenticate; `reason` is the sentence the system prompt shows. */
export async function confirmBiometric(reason: string): Promise<BiometricResult> {
  try {
    const [hardware, enrolled] = await Promise.all([
      LocalAuthentication.hasHardwareAsync(),
      LocalAuthentication.isEnrolledAsync(),
    ]);
    if (!hardware || !enrolled) return "unavailable";
    // Android: Class 3 only. Its default, weak, takes a camera face unlock that a photo can fool;
    // a phone with only that falls back to its PIN.
    const result = await LocalAuthentication.authenticateAsync({ promptMessage: reason, biometricsSecurityLevel: "strong" });
    return result.success ? "ok" : "failed";
  } catch {
    return "failed";
  }
}
