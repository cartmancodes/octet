// The transcript's styles at each text size (`Aa`): one table per theme and size, made once when
// the module loads, so a row never builds a style sheet while it draws.
import { TEXT_SIZES, type TextSize } from "./persist";
import { MONO, dark, light, type Theme } from "./theme";

/** The transcript's text at `size`: monospace, 5 points of leading. */
export const transcriptText = (size: TextSize) => ({ fontFamily: MONO, fontSize: size, lineHeight: size + 5 });

/** `make`'s styles for each theme and text size, made now; the lookup hands back the same object
 * each time. */
export function sizedStyles<S>(make: (theme: Theme, size: TextSize) => S): (theme: Theme, size: TextSize) => S {
  const table = (theme: Theme) => new Map(TEXT_SIZES.map((size) => [size, make(theme, size)] as const));
  const forDark = table(dark);
  const forLight = table(light);
  return (theme, size) => {
    const styles = (theme === light ? forLight : forDark).get(size);
    // TEXT_SIZES lists every TextSize, so a size is always there.
    if (styles === undefined) throw new Error(`No styles for text size ${size}`);
    return styles;
  };
}
