import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

import { POSE_NAMES, POSES } from "./poses";

const root = path.resolve(__dirname, "..", "..");

describe("scripts/mascot.mjs", () => {
  it("reproduces the checked-in poses.ts", () => {
    const generated = execFileSync(
      process.execPath,
      [path.join(root, "scripts", "mascot.mjs"), "--stdout"],
      { encoding: "utf8" },
    );
    const checkedIn = readFileSync(path.join(__dirname, "poses.ts"), "utf8");
    expect(generated).toBe(checkedIn);
  });
});

describe("the nine poses", () => {
  it("are idle, thinking, coding, searching, delegating, approval, success, error, sleeping", () => {
    expect([...POSE_NAMES].sort()).toEqual(
      [
        "approval",
        "coding",
        "delegating",
        "error",
        "idle",
        "searching",
        "sleeping",
        "success",
        "thinking",
      ].sort(),
    );
  });

  it.each(POSE_NAMES)("%s has a viewBox and one path per colour", (name) => {
    const pose = POSES[name];
    expect(pose.viewBox).toBe("0 0 59 43");
    expect(pose.paths.length).toBeGreaterThan(0);
    const fills = pose.paths.map((p) => p.fill);
    expect(new Set(fills).size).toBe(fills.length);
    for (const p of pose.paths) {
      expect(p.fill).toMatch(/^#[0-9a-f]{6}$/);
      expect(p.d).toMatch(/^M/);
    }
  });

  it("carry no metadata and no c2pa in the source", () => {
    const source = readFileSync(path.join(__dirname, "poses.ts"), "utf8");
    expect(source).not.toMatch(/metadata/i);
    expect(source).not.toMatch(/c2pa/i);
    expect(source.length).toBeLessThan(100_000);
  });
});
