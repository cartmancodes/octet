// Draws `parse`'s blocks. Every string is a <Text> child: nothing is interpreted, nothing opens.
// A link is its text and, beside it in muted text, its URL; no part of it can be pressed.
import { memo, useMemo, type ReactNode } from "react";
import { StyleSheet, Text, View } from "react-native";

import { DEFAULT_SETTINGS, type TextSize } from "../persist";
import { sizedStyles, transcriptText } from "../sizedStyles";
import { radius, spacing, type, useTheme, type Theme } from "../theme";
import { doneBlocks, streamBlocks } from "./blocks";
import type { Block, Span } from "./parse";

/** The transcript's text at `size` (`Aa`): monospace, 5 points of leading. */
function makeStyles(theme: Theme, size: TextSize) {
  const body = transcriptText(size);
  return StyleSheet.create({
    root: { gap: spacing.sm },
    text: { ...body, color: theme.fg },
    bold: { fontWeight: "700" },
    italic: { fontStyle: "italic" },
    code: { ...body, color: theme.fg, backgroundColor: theme.panel },
    url: { color: theme.muted },
    h1: { ...type.title, color: theme.accent },
    h2: { fontSize: 18, lineHeight: 24, fontWeight: "700", color: theme.accent },
    h3: { ...body, fontWeight: "700", color: theme.accent },
    block: {
      backgroundColor: theme.panel,
      borderColor: theme.edge,
      borderWidth: StyleSheet.hairlineWidth,
      borderRadius: radius.sm,
      padding: spacing.sm,
    },
    codeText: { ...body, color: theme.fg },
    item: { flexDirection: "row", gap: spacing.sm },
    marker: { ...body, color: theme.muted, minWidth: 16 },
    itemText: { ...body, color: theme.fg, flex: 1 },
    quote: { borderLeftWidth: 3, borderLeftColor: theme.edge, paddingLeft: spacing.md },
    quoteText: { ...body, color: theme.muted },
    rest: { ...type.caption, color: theme.muted },
  });
}

type Styles = ReturnType<typeof makeStyles>;
const stylesFor = sizedStyles(makeStyles);

/** What a reply says, at its top, in place of the earlier blocks it does not draw. */
export const earlierBlocks = (n: number): string => `${n} earlier blocks of this reply are on the Mac`;

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

/** The drawn blocks, keyed by their place in the whole reply (`first` is the first one's): as
 * the newest blocks move the window on, those still in it keep their key and their object, and
 * do not draw again. */
const Blocks = memo(function Blocks({ blocks, first, s }: { blocks: Block[]; first: number; s: Styles }) {
  return blocks.map((block, i) => <BlockView key={first + i} block={block} s={s} />);
});

/**
 * A reply, drawn as the Markdown subset: its newest `MAX_BLOCKS` blocks, under a line saying how
 * many earlier ones are on the Mac. A finished reply is parsed once per text; one `streaming` parses only
 * what came since its last whole block, so a chunk costs the same however long the reply is.
 */
export const Markdown = memo(function Markdown({
  text,
  streaming = false,
  size = DEFAULT_SETTINGS.textSize,
}: {
  text: string;
  streaming?: boolean;
  /** The transcript's text size. */
  size?: TextSize;
}) {
  const theme = useTheme();
  const s = stylesFor(theme, size);
  const shown = useMemo(() => (streaming ? streamBlocks(text) : doneBlocks(text)), [text, streaming]);
  return (
    <View style={s.root}>
      {shown.more > 0 ? <Text style={s.rest}>{earlierBlocks(shown.more)}</Text> : null}
      {shown.parts ? (
        <>
          {/* The start's part is the same array while the start does not move: it does not draw again. */}
          <Blocks blocks={shown.parts[0]} first={shown.more} s={s} />
          <Blocks blocks={shown.parts[1]} first={shown.more + shown.parts[0].length} s={s} />
        </>
      ) : (
        <Blocks blocks={shown.blocks} first={shown.more} s={s} />
      )}
    </View>
  );
});
