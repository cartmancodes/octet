import { render, screen } from "@testing-library/react-native";
import { StyleSheet } from "react-native";

import { dark, light } from "../theme";
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
    // A family React Native can load (the test platform is iOS), not a CSS list.
    expect(style(screen.getByText("make gate && echo **done**")).fontFamily).toBe("Menlo");
    expect(style(screen.getByText("cargo test")).fontFamily).toBe("Menlo");
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

  it("draws the last 400 blocks of a reply, under a line saying the earlier ones are on the Mac", async () => {
    const text = Array.from({ length: 1_000 }, (_, i) => `para ${i}`).join("\n\n");
    for (const streaming of [false, true]) {
      await render(<Markdown text={text} streaming={streaming} />);
      expect(screen.getByText("para 999")).toBeTruthy();
      expect(screen.getByText("para 600")).toBeTruthy();
      expect(screen.queryByText("para 599")).toBeNull();
      const line = screen.getByText("600 earlier blocks of this reply are on the Mac");
      expect([dark.muted, light.muted]).toContain(style(line).color);
      // At the top: before the first block drawn.
      const texts = screen.container.queryAll((n) => n.type === "Text").map((n) => n.props.children as unknown);
      expect(texts.indexOf("600 earlier blocks of this reply are on the Mac")).toBe(0);
    }
    await render(<Markdown text="short" />);
    expect(screen.queryByText(/earlier blocks/)).toBeNull();
  });

  it("draws a reply still streaming as Markdown too", async () => {
    await render(<Markdown text={"Some **strong** words\n\n```sh\nmake"} streaming />);
    expect(style(screen.getByText("strong")).fontWeight).toBe("700");
    expect(style(screen.getByText("make")).fontFamily).toBe("Menlo");
  });

  it("parses once per text, not once per render", async () => {
    const parse = jest.spyOn(parser, "parse");
    const view = await render(<Markdown text="**a**" />);
    await view.rerender(<Markdown text="**a**" />);
    expect(parse).toHaveBeenCalledTimes(1);
    parse.mockRestore();
  });
});
