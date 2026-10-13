// Draws `parse`'s blocks. Every string is a <Text> child: nothing is interpreted, nothing opens.
// A link is its text and, beside it in muted text, its URL; no part of it can be pressed.
import { memo, useMemo, type ReactNode } from "react";
import { StyleSheet, Text, View } from "react-native";

import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";
import { doneBlocks, streamBlocks } from "./blocks";
import type { Block, Span } from "./parse";

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
    rest: { ...type.caption, color: theme.muted },
  });
}

type Styles = ReturnType<typeof makeStyles>;
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

/** What a reply says in place of the blocks it does not draw. */
export const restOfReply = (n: number): string => `Rest of the reply is on the Mac (${n} more blocks)`;

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

/** The drawn blocks: the same array draws nothing again, so a chunk that only grows a reply's
 * hidden tail redraws only the count. */
const Blocks = memo(function Blocks({ blocks, s }: { blocks: Block[]; s: Styles }) {
  return blocks.map((block, i) => <BlockView key={i} block={block} s={s} />);
});

/**
 * A reply, drawn as the Markdown subset: at most `MAX_BLOCKS` blocks, then a line saying how
 * many more are on the Mac. A finished reply is parsed once per text; one `streaming` parses only
 * what came since its last whole block, so a chunk costs the same however long the reply is.
 */
export const Markdown = memo(function Markdown({ text, streaming = false }: { text: string; streaming?: boolean }) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const shown = useMemo(() => (streaming ? streamBlocks(text) : doneBlocks(text)), [text, streaming]);
  return (
    <View style={s.root}>
      <Blocks blocks={shown.blocks} s={s} />
      {shown.more > 0 ? <Text style={s.rest}>{restOfReply(shown.more)}</Text> : null}
    </View>
  );
});
