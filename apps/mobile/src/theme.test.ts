import { dark, light, type Theme } from "./theme";

// WCAG 2.x relative luminance and contrast ratio, computed here so the test
// does not trust the theme's own numbers.
function channel(value: number): number {
  const c = value / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

function luminance(hex: string): number {
  const n = Number.parseInt(hex.slice(1), 16);
  return (
    0.2126 * channel((n >> 16) & 255) +
    0.7152 * channel((n >> 8) & 255) +
    0.0722 * channel(n & 255)
  );
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

describe("the TUI palette, as dark tokens", () => {
  it("equals the TUI's view.rs values exactly", () => {
    expect(dark).toEqual({
      bg: "#161113",
      panel: "#1f171a",
      fg: "#eae0da",
      muted: "#ae979a",
      edge: "#58333d",
      accent: "#d87c82",
      amber: "#e0b477",
      selected: "#40242c",
    });
  });
});

describe("the light tokens", () => {
  it("equal the design review values, with derived panel and selected", () => {
    expect(light).toEqual({
      bg: "#faf6f5",
      panel: "#f2eae8",
      fg: "#302329",
      muted: "#725960",
      edge: "#d6bcc4",
      accent: "#943f53",
      amber: "#825510",
      selected: "#ecdde1",
    });
  });
});

describe.each([
  ["dark", dark],
  ["light", light],
] as [string, Theme][])("%s contrast", (_name, theme) => {
  const text = ["fg", "muted", "accent", "amber"] as const;

  it.each(text.flatMap((fg) => (["bg", "panel"] as const).map((bg) => [fg, bg] as const)))(
    "%s on %s meets AA for body text (4.5:1), except muted on panel (3:1)",
    (fg, bg) => {
      const floor = fg === "muted" && bg === "panel" ? 3 : 4.5;
      expect(contrast(theme[fg], theme[bg])).toBeGreaterThanOrEqual(floor);
    },
  );

  it("keeps fg readable on the selected row", () => {
    expect(contrast(theme.fg, theme.selected)).toBeGreaterThanOrEqual(4.5);
    expect(contrast(theme.muted, theme.selected)).toBeGreaterThanOrEqual(3);
  });
});
