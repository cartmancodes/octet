import { render, screen } from "@testing-library/react-native";
import { StyleSheet } from "react-native";

import { dark, light, type } from "../theme";
import { Markdown } from "./Markdown";
import * as parser from "./parse";

type Node = { props: Record<string, unknown>; parent: Node | null };

/** True when `node` or anything above it would act on a press. */
function pressable(node: Node | null): boolean {
  for (let n = node; n; n = n.parent) {
    const p = n.props;
    if (p.onPress || p.onLongPress || p.onPressIn || p.accessibilityRole === "link" || p.role === "link" || p.href) return true;
  }
  return false;
}

const style = (node: { props: Record<string, unknown> }) => StyleSheet.flatten(node.props.style as never) as Record<string, unknown>;

/** The colour a nested <Text> draws in: its own, else the nearest one above it. */
function colorOf(node: Node | null): unknown {
  for (let n = node; n; n = n.parent) {
    const color = style(n).color;
    if (color !== undefined) return color;
  }
  return undefined;
}

describe("markdown_is_inert (drawn)", () => {
  it("draws a link as its text and the URL in muted text, and nothing about it presses", async () => {
    await render(<Markdown text="Read [the guide](https://evil.example/x?y=1) first." />);
    const label = screen.getByText("the guide");
    const url = screen.getByText(/https:\/\/evil\.example\/x\?y=1/);
    expect([dark.muted, light.muted]).toContain(colorOf(url as unknown as Node));
    expect(pressable(label as unknown as Node)).toBe(false);
    expect(pressable(url as unknown as Node)).toBe(false);
    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });

  it("draws HTML as the characters it is", async () => {
    const html = '<b>bold?</b><script>alert(1)</script><a href="https://x">x</a>';
    await render(<Markdown text={html} />);
    expect(screen.getByText(html)).toBeTruthy();
    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });

  it("draws code blocks and inline code in monospace", async () => {
    await render(<Markdown text={"Run `cargo test` now.\n\n```sh\nmake gate && echo **done**\n```"} />);
    expect(style(screen.getByText("make gate && echo **done**")).fontFamily).toBe(type.mono.fontFamily);
    expect(style(screen.getByText("cargo test")).fontFamily).toBe(type.mono.fontFamily);
  });

  it("draws bold, italic, headings, lists and quotes", async () => {
    await render(<Markdown text={"# Plan\n\nSome **strong** and *soft* words.\n\n- one\n2. two\n\n> said"} />);
    expect(style(screen.getByText("strong")).fontWeight).toBe("700");
    expect(style(screen.getByText("soft")).fontStyle).toBe("italic");
    expect([dark.accent, light.accent]).toContain(colorOf(screen.getByText("Plan") as unknown as Node));
    expect(screen.getByText("•")).toBeTruthy();
    expect(screen.getByText("one")).toBeTruthy();
    expect(screen.getByText("2.")).toBeTruthy();
    expect(screen.getByText("said")).toBeTruthy();
    expect(screen.queryByText(/\*\*/)).toBeNull();
  });

  it("parses once per text, not once per render", async () => {
    const parse = jest.spyOn(parser, "parse");
    const view = await render(<Markdown text="**a**" />);
    await view.rerender(<Markdown text="**a**" />);
    expect(parse).toHaveBeenCalledTimes(1);
    parse.mockRestore();
  });
});
