import { render, screen } from "@testing-library/react-native";
import { AccessibilityInfo, Platform } from "react-native";

import { HubBanner } from "./HubBanner";

afterEach(() => jest.restoreAllMocks());

/** React Native's mock makes this one jest.fn for every test: start each spy cleared. */
function announcements() {
  const spy = jest.spyOn(AccessibilityInfo, "announceForAccessibility");
  spy.mockClear();
  return spy;
}

describe("HubBanner", () => {
  it("on iOS announces its words when they change, and not again for the same words", async () => {
    const announce = announcements();
    const view = await render(<HubBanner state="offline" />);
    await view.rerender(<HubBanner state="offline" />);
    await view.rerender(<HubBanner state="connecting" />);
    expect(announce.mock.calls).toEqual([["Can't reach your Mac"], ["Connecting to your Mac…"]]);
  });

  it("on Android leaves it to the polite live region", async () => {
    jest.replaceProperty(Platform, "OS", "android");
    const announce = announcements();
    await render(<HubBanner state="offline" />);
    expect(screen.getByLabelText("Can't reach your Mac").props.accessibilityLiveRegion).toBe("polite");
    expect(announce).not.toHaveBeenCalled();
  });

  it("says nothing on a screen not in front, and shows nothing when the Mac is reachable", async () => {
    const announce = announcements();
    const view = await render(<HubBanner state="offline" announce={false} />);
    expect(announce).not.toHaveBeenCalled();
    await view.rerender(<HubBanner state="ready" />);
    expect(screen.queryByLabelText(/Mac/)).toBeNull();
  });
});
