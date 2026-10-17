import { fireEvent, render, screen } from "@testing-library/react-native";
import { StyleSheet } from "react-native";

import type { Entry } from "../model/transcript";
import { dark, light, type Theme } from "../theme";
import { TranscriptRow } from "./TranscriptRow";

const noop = () => undefined;
const look = (node: { props: { style?: unknown } }) =>
  StyleSheet.flatten(node.props.style as never) as { color?: unknown; fontWeight?: unknown; fontSize?: unknown; fontFamily?: unknown };
const tones = (role: keyof Theme) => [dark[role], light[role]];

describe("the labels", () => {
  const tools: Entry = { kind: "tools", key: "1", calls: [{ id: "1", name: "Read", target: "a.rs", phase: "done", detail: "" }], over: false };
  it.each<[string, Entry, keyof Theme]>([
    ["YOU", { kind: "user", key: "1", text: "Fix the hub" }, "accent"],
    ["OCTET", { kind: "reply", key: "1", text: "On it.", done: true }, "fg"],
    ["TOOL", tools, "muted"],
    ["NOTE", { kind: "note", key: "1", text: "Model changed" }, "muted"],
    ["ERROR", { kind: "error", key: "1", text: "Rate limited" }, "amber"],
  ])("%s is a bold line over the entry, in its colour", async (label, entry, tone) => {
    await render(<TranscriptRow entry={entry} open={false} onToggle={noop} />);
    const node = screen.getByText(label);
    expect(tones(tone)).toContain(look(node).color);
    expect(look(node).fontWeight).toBe("700");
    expect(look(node).fontFamily).toBe("Menlo");
  });

  it("the tools row keeps its grouping under its label", async () => {
    await render(<TranscriptRow entry={tools} open={false} onToggle={noop} />);
    expect(screen.getByText("TOOL")).toBeTruthy();
    expect(screen.getByText("▸ Read 1 file")).toBeTruthy();
  });

  it("note bodies are muted and error bodies amber", async () => {
    const view = await render(<TranscriptRow entry={{ kind: "note", key: "1", text: "Model changed" }} open={false} onToggle={noop} />);
    expect(tones("muted")).toContain(look(screen.getByText("Model changed")).color);
    await view.rerender(<TranscriptRow entry={{ kind: "error", key: "1", text: "Rate limited" }} open={false} onToggle={noop} />);
    expect(tones("amber")).toContain(look(screen.getByText("Rate limited")).color);
  });

  it.each([12, 13, 14] as const)("draws the label and the body at the text size, %i px", async (size) => {
    const view = await render(<TranscriptRow entry={{ kind: "user", key: "1", text: "Fix the hub" }} open={false} onToggle={noop} size={size} />);
    expect(look(screen.getByText("YOU")).fontSize).toBe(size);
    expect(look(screen.getByText("Fix the hub")).fontSize).toBe(size);
    await view.rerender(<TranscriptRow entry={{ kind: "reply", key: "2", text: "Use **bold**", done: true }} open={false} onToggle={noop} size={size} />);
    expect(look(screen.getByText("OCTET")).fontSize).toBe(size);
    expect(look(screen.getByText(/^Use /)).fontSize).toBe(size);
    await view.rerender(<TranscriptRow entry={tools} open={false} onToggle={noop} size={size} />);
    expect(look(screen.getByText("▸ Read 1 file")).fontSize).toBe(size);
  });

  it("draws at 13 px when no size is given", async () => {
    await render(<TranscriptRow entry={{ kind: "user", key: "1", text: "Fix the hub" }} open={false} onToggle={noop} />);
    expect(look(screen.getByText("Fix the hub")).fontSize).toBe(13);
  });
});

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
