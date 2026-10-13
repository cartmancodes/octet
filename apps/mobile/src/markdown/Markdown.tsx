// Draws `parse`'s blocks. Every string is a <Text> child: nothing is interpreted, nothing opens.
// A link is its text and, beside it in muted text, its URL; no part of it can be pressed.
import { memo, useMemo, type ReactNode } from "react";
import { StyleSheet, Text, View } from "react-native";

import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { parse, type Block, type Span } from "./parse";

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    root: { gap: spacing.sm },
    text: { ...type.body, color: theme.fg },
    bold: { fontWeight: "700" },
    italic: { fontStyle: "italic" },
    code: { ...type.mono, color: theme.fg, backgroundColor: theme.panel },
    url: { color: theme.muted },
    h1: { ...type.title, color: theme.accent },
    h2: { fontSize: 18, lineHeight: 24, fontWeight: "700", color: theme.accent },
    h3: { ...type.body, fontWeight: "700", color: theme.accent },
    block: {
      backgroundColor: theme.panel,
      borderColor: theme.edge,
      borderWidth: StyleSheet.hairlineWidth,
      borderRadius: radius.sm,
      padding: spacing.sm,
    },
    codeText: { ...type.mono, color: theme.fg },
    item: { flexDirection: "row", gap: spacing.sm },
    marker: { ...type.body, color: theme.muted, minWidth: 16 },
    itemText: { ...type.body, color: theme.fg, flex: 1 },
    quote: { borderLeftWidth: 3, borderLeftColor: theme.edge, paddingLeft: spacing.md },
    quoteText: { ...type.body, color: theme.muted },
  });
}

type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/** The spans of a block as nested <Text>; a link's URL follows its text, muted. */
function spans(list: readonly Span[], s: Styles): ReactNode[] {
  const out: ReactNode[] = [];
  list.forEach((span, i) => {
    const look = [span.bold ? s.bold : null, span.italic ? s.italic : null, span.code ? s.code : null];
    out.push(
      <Text key={i} style={look}>
        {span.text}
      </Text>,
    );
    if (span.url !== undefined) {
      out.push(
        <Text key={`${i}url`} style={s.url}>
          {` (${span.url})`}
        </Text>,
      );
    }
  });
  return out;
}

const BlockView = memo(function BlockView({ block, s }: { block: Block; s: Styles }) {
  switch (block.type) {
    case "paragraph":
      return <Text style={s.text}>{spans(block.spans, s)}</Text>;
    case "heading":
      return <Text style={block.level === 1 ? s.h1 : block.level === 2 ? s.h2 : s.h3}>{spans(block.spans, s)}</Text>;
    case "code":
      return (
        <View style={s.block}>
          <Text selectable style={s.codeText}>
            {block.text}
          </Text>
        </View>
      );
    case "list":
      return (
        <View>
          {block.items.map((item, i) => (
            <View key={i} style={[s.item, { paddingLeft: item.depth * spacing.lg }]}>
              <Text style={s.marker}>{item.marker}</Text>
              <Text style={s.itemText}>{spans(item.spans, s)}</Text>
            </View>
          ))}
        </View>
      );
    case "quote":
      return (
        <View style={s.quote}>
          <Text style={s.quoteText}>{spans(block.spans, s)}</Text>
        </View>
      );
  }
});

/** Recent texts' blocks: FlashList recycles a row's component for another reply as the list
 * scrolls, which would otherwise parse each reply again as it comes back into view. */
const PARSED = new Map<string, Block[]>();
const PARSED_KEPT = 64;

function blocksOf(text: string): Block[] {
  const hit = PARSED.get(text);
  if (hit) {
    PARSED.delete(text); // most recent last
    PARSED.set(text, hit);
    return hit;
  }
  const blocks = parse(text);
  PARSED.set(text, blocks);
  if (PARSED.size > PARSED_KEPT) {
    const oldest = PARSED.keys().next().value;
    if (oldest !== undefined) PARSED.delete(oldest);
  }
  return blocks;
}

/** A finished reply, drawn as the Markdown subset. Parsed once per text. */
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const blocks = useMemo(() => blocksOf(text), [text]);
  return (
    <View style={s.root}>
      {blocks.map((block, i) => (
        <BlockView key={i} block={block} s={s} />
      ))}
    </View>
  );
});
