import { TEXT_SIZES } from "./persist";
import { sizedStyles, transcriptText } from "./sizedStyles";
import { MONO, dark, light, type Theme } from "./theme";

describe("sizedStyles: one style table per theme and transcript text size", () => {
  it("makes each table once, up front, and hands back the same object every time", () => {
    const make = jest.fn((theme: Theme, size: number) => ({ color: theme.fg, size }));
    const styles = sizedStyles(make);
    expect(make).toHaveBeenCalledTimes(2 * TEXT_SIZES.length);
    expect(styles(dark, 12)).toEqual({ color: dark.fg, size: 12 });
    expect(styles(light, 14)).toEqual({ color: light.fg, size: 14 });
    expect(styles(dark, 13)).toBe(styles(dark, 13));
    expect(styles(dark, 13)).not.toBe(styles(light, 13));
    expect(make).toHaveBeenCalledTimes(2 * TEXT_SIZES.length);
  });

  it("transcript text is monospace with 5 points of leading", () => {
    expect(transcriptText(12)).toEqual({ fontFamily: MONO, fontSize: 12, lineHeight: 17 });
  });
});
