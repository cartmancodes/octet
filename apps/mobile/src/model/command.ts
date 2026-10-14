// An approval's command as the Permission card shows it: nothing of what Allow runs may sit out of sight
// below a wall of blank lines. Plain TypeScript.

/** Blank lines in a row, from this many, are drawn as one line saying how many. */
export const FOLD_BLANKS = 3;

/** The line standing for `n` blank (or whitespace-only) lines. */
export const blankLines = (n: number): string => `⋯ ${n} blank lines ⋯`;

/** The characters that draw as a blank: whitespace, the braille blank, the Hangul fillers, the
 * soft hyphen and the joiners. */
const BLANKS = "\\s\u2800\u3164\u115f\uffa0\u00ad\u200c\u200d";
/** A line that shows nothing. */
const BLANK = new RegExp(`^[${BLANKS}]*$`, "u");
/** Blanks in a row inside a line, from this many, are drawn as one mark saying how many. */
export const FOLD_RUN = 16;
/** A run of blanks inside a line long enough to push the rest of it out of sight. */
const BLANK_RUN = new RegExp(`[${BLANKS}]{${FOLD_RUN},}`, "gu");

/** The text standing for `n` blank characters inside a line. */
export const blankChars = (n: number): string => ` ⋯ ${n} blank characters ⋯ `;

/** `command` with each run of `FOLD_BLANKS` or more blank lines folded into one line saying
 * how many, each long blank run inside a line shortened the same way, and how many lines the
 * command has. */
export function foldCommand(command: string): { text: string; lines: number } {
  const all = command.split("\n");
  const out: string[] = [];
  let blanks = 0;
  const flush = () => {
    if (blanks >= FOLD_BLANKS) out.push(blankLines(blanks));
    else for (let i = 0; i < blanks; i++) out.push("");
    blanks = 0;
  };
  for (const line of all) {
    if (BLANK.test(line)) {
      blanks += 1;
      continue;
    }
    flush();
    out.push(line.replace(BLANK_RUN, (run) => blankChars([...run].length)));
  }
  flush();
  return { text: out.join("\n"), lines: all.length };
}
