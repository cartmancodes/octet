// An approval's command as the sheet shows it: nothing of what Allow runs may sit out of sight
// below a wall of blank lines. Plain TypeScript.

/** Blank lines in a row, from this many, are drawn as one line saying how many. */
export const FOLD_BLANKS = 3;

/** The line standing for `n` blank (or whitespace-only) lines. */
export const blankLines = (n: number): string => `⋯ ${n} blank lines ⋯`;

/** A line that shows nothing: whitespace, and the characters that draw as a blank (the braille
 * blank, the Hangul fillers, the soft hyphen). */
const BLANK = /^[\s⠀ㅤᅟﾠ­]*$/;

/** `command` with each run of `FOLD_BLANKS` or more blank lines folded into one line saying
 * how many, and how many lines the command has. */
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
    out.push(line);
  }
  flush();
  return { text: out.join("\n"), lines: all.length };
}
