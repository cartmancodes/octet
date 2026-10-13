import { poseFor, type Activity } from "./state";

const tool = (name: string, target = ""): Activity => ({ kind: "tool", name, target });

describe("poseFor mirrors the TUI's mascot::State", () => {
  it.each([
    [{ kind: "approval" }, "approval"],
    [{ kind: "replying" }, "thinking"],
    [{ kind: "finished" }, "success"],
    [{ kind: "failed" }, "error"],
    [{ kind: "idle" }, "idle"],
    [{ kind: "stopped" }, "sleeping"],
  ] as const)("%j -> %s", (activity, pose) => {
    expect(poseFor(activity)).toBe(pose);
  });

  it.each(["Read", "Grep", "Glob", "WebSearch"])("%s searches", (name) => {
    expect(poseFor(tool(name))).toBe("searching");
  });

  it.each(["rg foo", "grep -r x .", "find . -name a", "cat a.txt", "ls -la"])(
    "a Bash command `%s` searches",
    (cmd) => {
      expect(poseFor(tool("Bash", cmd))).toBe("searching");
    },
  );

  it.each(["Task", "dispatch_agent", "spawn_agent", "CollabAgent", "delegate"])(
    "%s delegates, before searching",
    (name) => {
      expect(poseFor(tool(name, "grep x"))).toBe("delegating");
    },
  );

  it.each([tool("Edit"), tool("Write"), tool("Bash", "cargo test")])("%j codes", (a) => {
    expect(poseFor(a)).toBe("coding");
  });

  it("a Bash call stays coding, whatever it runs that is not a search", () => {
    expect(poseFor(tool("Bash"))).toBe("coding");
    expect(poseFor(tool("Bash", ""))).toBe("coding");
    expect(poseFor(tool("Bash", "  npm run build"))).toBe("coding");
    expect(poseFor(tool("Bash", "git commit -m x"))).toBe("coding");
    expect(poseFor(tool("bash", "cargo test"))).toBe("coding");
  });
});
