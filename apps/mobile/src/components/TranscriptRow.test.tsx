import { fireEvent, render, screen } from "@testing-library/react-native";

import type { Entry } from "../model/transcript";
import { TranscriptRow } from "./TranscriptRow";

const noop = () => undefined;

describe("a reply row", () => {
  const paragraphs = (n: number) => Array.from({ length: n }, (_, i) => `p${i}`).join("\n\n");

  it("draws a streaming reply of 16,000 paragraphs as its newest 400 blocks, under the earlier-blocks line", async () => {
    const entry = { kind: "reply" as const, key: "1", text: paragraphs(16_000), done: false };
    const view = await render(<TranscriptRow entry={entry} open={false} onToggle={noop} />);
    expect(screen.getByText("p15999")).toBeTruthy();
    expect(screen.queryByText("p15599")).toBeNull();
    expect(screen.getByText("15600 earlier blocks of this reply are on the Mac")).toBeTruthy();
    // The newest text, as it streams, is on screen.
    await view.rerender(<TranscriptRow entry={{ ...entry, text: `${entry.text}\n\nthe newest words` }} open={false} onToggle={noop} />);
    expect(screen.getByText("the newest words")).toBeTruthy();
    expect(screen.getByText("15601 earlier blocks of this reply are on the Mac")).toBeTruthy();
    expect(screen.container.queryAll((n) => n.type === "Text").length).toBeLessThan(1_000);
  });

  it("draws a finished reply of 16,000 code fences as at most 400 blocks", async () => {
    const text = "```\n".repeat(16_000);
    await render(<TranscriptRow entry={{ kind: "reply", key: "1", text, done: true }} open={false} onToggle={noop} />);
    expect(screen.getByText(/^\d+ earlier blocks of this reply are on the Mac$/)).toBeTruthy();
    expect(screen.container.queryAll((n) => n.type === "Text").length).toBeLessThan(1_000);
  });
});

describe("a tools row", () => {
  const entry: Entry = {
    kind: "tools",
    key: "1",
    calls: [{ id: "1", name: "Read", target: "a.rs", phase: "done", detail: "" }],
    over: false,
    more: 4_500,
  };

  it("says how many calls only the Mac holds, collapsed and open", async () => {
    const view = await render(<TranscriptRow entry={entry} open={false} onToggle={noop} />);
    expect(screen.getByText("4500 more calls on the Mac")).toBeTruthy();
    expect(screen.getByRole("button").props.accessibilityLabel).toMatch(/4500 more calls on the Mac/);
    await view.rerender(<TranscriptRow entry={entry} open onToggle={noop} />);
    expect(screen.getByText("4500 more calls on the Mac")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button"));
  });

  it("says nothing of the kind when it holds every call", async () => {
    await render(<TranscriptRow entry={{ ...entry, more: 0 }} open={false} onToggle={noop} />);
    expect(screen.queryByText(/more calls on the Mac/)).toBeNull();
  });
});
