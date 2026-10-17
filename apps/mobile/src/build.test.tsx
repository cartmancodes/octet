import { render, screen } from "@testing-library/react-native";

import { DEV_BUILD_WARNING, DevBuildBanner, isDevBuild } from "./components/DevBuildBanner";
import { TOKEN_KEY, tokenKey } from "./hub";

const g = globalThis as { __DEV__?: boolean };
const dev = g.__DEV__;
afterEach(() => {
  g.__DEV__ = dev;
});

describe("a development build", () => {
  it("shows the amber warning, and a release build does not", async () => {
    g.__DEV__ = true;
    expect(isDevBuild()).toBe(true);
    await render(<DevBuildBanner />);
    expect(screen.getByText(DEV_BUILD_WARNING)).toBeTruthy();
    expect(DEV_BUILD_WARNING).toBe("Development build — don't pair with your everyday Mac");

    g.__DEV__ = false;
    expect(isDevBuild()).toBe(false);
    await render(<DevBuildBanner />);
    expect(screen.queryByText(DEV_BUILD_WARNING)).toBeNull();
  });

  it("keeps its token under its own keychain item, so it never inherits a release pairing", () => {
    expect(tokenKey(false)).toBe("octet.token");
    expect(tokenKey(true)).toBe("octet.token.dev");
    expect(TOKEN_KEY).toBe(tokenKey(dev === true));
  });
});
