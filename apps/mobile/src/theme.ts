import { Platform, useColorScheme } from "react-native";

/** The colour roles, named after the terminal UI's (crates/octet-tui/src/view.rs). */
export interface Theme {
  bg: string;
  panel: string;
  fg: string;
  muted: string;
  edge: string;
  accent: string;
  /** Warnings and permission decisions only. */
  amber: string;
  selected: string;
}

/** The terminal UI's palette, exactly. */
export const dark: Theme = {
  bg: "#161113",
  panel: "#1f171a",
  fg: "#eae0da",
  muted: "#ae979a",
  edge: "#58333d",
  accent: "#d87c82",
  amber: "#e0b477",
  selected: "#40242c",
};

/**
 * The design review's light palette. `panel` and `selected` are derived: each
 * steps a little darker than `bg`, with the same crimson tint, and every text
 * colour still clears AA on both (checked in theme.test.ts).
 */
export const light: Theme = {
  bg: "#faf6f5",
  panel: "#f2eae8",
  fg: "#302329",
  muted: "#725960",
  edge: "#d6bcc4",
  accent: "#943f53",
  amber: "#825510",
  selected: "#ecdde1",
};

export const spacing = { xs: 4, sm: 8, md: 12, lg: 16, xl: 24, xxl: 32 } as const;

export const radius = { sm: 6, md: 10, lg: 16 } as const;

/** The type scale; the system font for prose, monospace for code and tool rows. */
export const type = {
  caption: { fontSize: 12, lineHeight: 16 },
  body: { fontSize: 16, lineHeight: 22 },
  title: { fontSize: 20, lineHeight: 26, fontWeight: "600" },
  display: { fontSize: 28, lineHeight: 34, fontWeight: "700" },
  // One family name: React Native takes no CSS fallback list.
  mono: { fontSize: 13, lineHeight: 18, fontFamily: Platform.select({ ios: "Menlo", android: "monospace", default: "monospace" }) },
} as const;

/** The theme for the system's colour scheme (dark when it is unset). */
export function useTheme(): Theme {
  return useColorScheme() === "light" ? light : dark;
}
