import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { usePathname, useRouter } from "expo-router";
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useState, type ReactNode } from "react";
import { AccessibilityInfo, Animated, BackHandler, PanResponder, Pressable, StyleSheet, Text, useWindowDimensions, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import { closeDrawer, openDrawer, useDrawerEnabled, useDrawerOpen } from "../drawer";
import { useHubClient } from "../hub";
import { drawerList, drawerSub, macName, type DrawerRow } from "../inbox";
import { useHub, useSessions } from "../store";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../theme";

/** The drawer's width, and how long it takes to open or close. */
export const DRAWER_WIDTH = 304;
export const DRAWER_MS = 180;
/** The strip at the screen's left edge that a swipe starts from. */
export const EDGE_WIDTH = 18;
/** How far a drag opens or closes the drawer. */
export const OPEN_DRAG = 30;
export const CLOSE_DRAG = 40;
/** The scrim's opacity once open. */
const SCRIM = 0.6;
/** A leftward drag this long, and mostly horizontal, is the drawer's to take from its children. */
const CLAIM_DRAG = 8;
/** How often the open drawer's "how long ago" moves on. */
const AGO_TICK_MS = 30_000;
/** The edge strip leaves the header and the composer alone. */
const EDGE_TOP = 56;
const EDGE_BOTTOM = 120;
/** The scrim is dark ink in both themes. */
const INK = dark.bg;

/** Whether a drag of `dx` across and `dy` down is horizontal: at least twice as wide as tall. */
const horizontal = (dx: number, dy: number): boolean => Math.abs(dx) >= 2 * Math.abs(dy);

function makeStyles(theme: Theme) {
  return StyleSheet.create({
    panel: {
      position: "absolute",
      left: 0,
      top: 0,
      bottom: 0,
      width: DRAWER_WIDTH,
      backgroundColor: theme.panel,
      borderRightWidth: 1,
      borderRightColor: theme.edge,
    },
    head: { flexDirection: "row", alignItems: "center", paddingLeft: 20, paddingRight: spacing.md, paddingBottom: spacing.md },
    headText: { flex: 1 },
    title: { ...type.body, color: theme.accent, fontWeight: "700" },
    sub: { ...type.label, color: theme.muted },
    plus: { width: 44, height: 44, alignItems: "center", justifyContent: "center", borderWidth: 1, borderColor: theme.edge, borderRadius: 22 },
    plusText: { fontSize: 20, lineHeight: 24, fontFamily: type.body.fontFamily, color: theme.fg },
    row: { minHeight: 44, flexDirection: "row", alignItems: "center", gap: 10, marginHorizontal: spacing.sm, paddingHorizontal: spacing.md, borderRadius: radius.md },
    on: { backgroundColor: theme.selected },
    name: { ...type.body, flex: 1 },
    badge: { minWidth: 24, height: 24, paddingHorizontal: 6, alignItems: "center", justifyContent: "center", borderRadius: 12, backgroundColor: theme.amber },
    badgeText: { ...type.label, color: INK, fontWeight: "700" },
    group: { ...type.label, color: theme.muted, paddingHorizontal: 20, paddingBottom: 4 },
    list: { flex: 1, minHeight: 0 },
    listContent: { paddingTop: 20, paddingBottom: 20 },
    foot: { borderTopWidth: 1, borderTopColor: theme.edge, paddingTop: spacing.sm },
    fg: { color: theme.fg },
    muted: { color: theme.muted },
    accent: { color: theme.accent },
    amber: { color: theme.amber },
  });
}
const DARK = makeStyles(dark);
const LIGHT = makeStyles(light);

interface RowProps {
  row: DrawerRow;
  /** The screen showing is this session's. */
  selected: boolean;
  /** The Mac can be reached: its sessions are in the foreground colour. */
  online: boolean;
  /** Must be stable. */
  onOpen: (id: string) => void;
}

const SELECTED = { selected: true } as const;
const NOT_SELECTED = { selected: false } as const;

/** One session in the drawer: its name, and a word for where it stands. Both are the Mac's and
 * drawn as text. */
const SessionRow = memo(function SessionRow({ row, selected, online, onOpen }: RowProps) {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const open = useCallback(() => onOpen(row.id), [onOpen, row.id]);
  const nameColor = selected ? s.accent : online ? s.fg : s.muted;
  return (
    <Pressable
      testID={`drawer-row-${row.id}`}
      accessibilityRole="button"
      accessibilityLabel={row.status === "" ? row.name : `${row.name}, ${row.status}`}
      accessibilityState={selected ? SELECTED : NOT_SELECTED}
      onPress={open}
      style={rowStyle(theme, selected)}
    >
      <Text numberOfLines={1} style={[s.name, nameColor]}>
        {row.name}
      </Text>
      {row.status !== "" ? <Text style={[type.label, s[row.tone]]}>{row.status}</Text> : null}
    </Pressable>
  );
}, sameRow);

const ROWS = {
  dark: { on: [DARK.row, DARK.on], off: [DARK.row] },
  light: { on: [LIGHT.row, LIGHT.on], off: [LIGHT.row] },
};
const rowStyle = (theme: Theme, selected: boolean) => ROWS[theme === light ? "light" : "dark"][selected ? "on" : "off"];

function sameRow(a: RowProps, b: RowProps): boolean {
  return (
    a.row.id === b.row.id &&
    a.row.name === b.row.name &&
    a.row.status === b.row.status &&
    a.row.tone === b.row.tone &&
    a.selected === b.selected &&
    a.online === b.online &&
    a.onOpen === b.onOpen
  );
}

/** Whether `pathname` is the Conversation of session `id` (expo-router may percent-encode the id). */
const isSessionPath = (pathname: string, id: string): boolean => pathname === `/session/${id}` || pathname === `/session/${encodeURIComponent(id)}`;

const keyOf = (row: DrawerRow): string => row.id;
const readClock = (): number => Date.now();

/**
 * The side drawer: the Inbox, the Mac's sessions and Settings. Mounted once, over the stack, in
 * app/_layout.tsx. It opens from the ☰ buttons or a swipe from the edge (`DrawerHost`), and closes
 * on a tap on the scrim, a drag left, or Android back. Its open state is `src/drawer.ts`, which the
 * screens under it read: nothing there answers an approval while it is up.
 */
export function Drawer() {
  const theme = useTheme();
  const s = theme === light ? LIGHT : DARK;
  const insets = useSafeAreaInsets();
  const router = useRouter();
  const pathname = usePathname();
  const client = useHubClient();
  const sessions = useSessions();
  const hub = useHub();
  const open = useDrawerOpen();
  const online = hub === "ready";
  const mac = macName(client?.url);

  // Stays mounted while it slides out.
  const [mounted, setMounted] = useState(open);
  if (open && !mounted) setMounted(true);
  const [progress] = useState(() => new Animated.Value(0));
  const [reduce, setReduce] = useState(false);
  useEffect(() => {
    let live = true;
    void Promise.resolve(AccessibilityInfo.isReduceMotionEnabled()).then((on) => {
      if (live && on === true) setReduce(true);
    });
    const subscription = AccessibilityInfo.addEventListener("reduceMotionChanged", setReduce);
    return () => {
      live = false;
      subscription?.remove();
    };
  }, []);
  useEffect(() => {
    if (!mounted) return;
    const slide = Animated.timing(progress, { toValue: open ? 1 : 0, duration: reduce ? 0 : DRAWER_MS, useNativeDriver: true });
    slide.start(({ finished }) => {
      if (finished && !open) setMounted(false);
    });
    return () => slide.stop();
  }, [open, mounted, reduce, progress]);

  // Android back closes it before it goes anywhere else.
  useEffect(() => {
    if (!open) return;
    const subscription = BackHandler.addEventListener("hardwareBackPress", () => {
      closeDrawer();
      return true;
    });
    return () => subscription.remove();
  }, [open]);

  // A drag left on the drawer, once it is clearly horizontal, is the drawer's, not its list's.
  // `lead` is what the drag had covered when the drawer took it; the responder counts from there.
  const [pan] = useState(() => {
    const lead = { dx: 0, dy: 0 };
    return PanResponder.create({
      onMoveShouldSetPanResponderCapture: (_event, g) => {
        if (g.dx > -CLAIM_DRAG || !horizontal(g.dx, g.dy)) return false;
        // A fast drag can pass the distance in its first move: it closes at once.
        if (g.dx < -CLOSE_DRAG) {
          closeDrawer();
          return false;
        }
        lead.dx = g.dx;
        lead.dy = g.dy;
        return true;
      },
      onPanResponderMove: (_event, g) => {
        const dx = lead.dx + g.dx;
        const dy = lead.dy + g.dy;
        if (dx < -CLOSE_DRAG && horizontal(dx, dy)) closeDrawer();
      },
    });
  });

  // The clock the "ago" texts count from: read afresh each time it opens, then every 30 s while
  // it is open (nothing is drawn while it is shut).
  const [clock, setClock] = useState(readClock);
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (open) setClock(readClock());
  }
  useEffect(() => {
    if (!open) return;
    const timer = setInterval(() => setClock(readClock()), AGO_TICK_MS);
    return () => clearInterval(timer);
  }, [open]);
  const list = useMemo(() => (mounted ? drawerList(sessions, clock) : null), [mounted, sessions, clock]);
  const rows = list?.rows ?? NONE;

  const goInbox = useCallback(() => {
    closeDrawer();
    if (pathname !== "/") router.navigate("/");
  }, [pathname, router]);
  const goSettings = useCallback(() => {
    closeDrawer();
    if (pathname !== "/settings") router.navigate("/settings");
  }, [pathname, router]);
  // New session is Task 4's screen; until then, Recent and new starts one.
  const goNew = useCallback(() => {
    closeDrawer();
    router.push("/recent");
  }, [router]);
  const goSession = useCallback(
    (id: string) => {
      closeDrawer();
      const target = { pathname: "/session/[id]", params: { id } } as const;
      if (isSessionPath(pathname, id)) return;
      // From one conversation to another: replace it, so back still returns to where it came from.
      if (pathname.startsWith("/session/")) router.replace(target);
      else router.push(target);
    },
    [pathname, router],
  );

  const renderItem = useCallback<ListRenderItem<DrawerRow>>(
    ({ item }) => (
      <SessionRow
        row={item}
        selected={isSessionPath(pathname, item.id)}
        online={online}
        onOpen={goSession}
      />
    ),
    [pathname, online, goSession],
  );

  const place = useMemo(
    () => ({ paddingTop: insets.top + spacing.md, paddingBottom: Math.max(insets.bottom, spacing.lg) }),
    [insets.top, insets.bottom],
  );
  const slide = useMemo(() => ({ transform: [{ translateX: progress.interpolate({ inputRange: [0, 1], outputRange: [-DRAWER_WIDTH, 0] }) }] }), [progress]);
  const fade = useMemo(() => ({ opacity: progress.interpolate({ inputRange: [0, 1], outputRange: [0, SCRIM] }) }), [progress]);

  const header = useMemo(
    () => (
      <Text style={s.group}>
        <Text style={online ? s.accent : s.muted}>{online ? "●" : "○"}</Text>
        {` ${mac} · ${online ? "online" : "offline"}`}
      </Text>
    ),
    [s, online, mac],
  );

  if (!mounted || !list) return null;
  const selectedInbox = pathname === "/";
  const selectedSettings = pathname === "/settings";
  return (
    <View pointerEvents="box-none" accessibilityViewIsModal style={StyleSheet.absoluteFill}>
      <Animated.View testID="drawer-scrim" style={[StyleSheet.absoluteFill, SCRIM_BOX, fade]}>
        <Pressable accessibilityRole="button" accessibilityLabel="Close menu" onPress={closeDrawer} style={StyleSheet.absoluteFill} />
      </Animated.View>
      <Animated.View testID="drawer" onAccessibilityEscape={closeDrawer} {...pan.panHandlers} style={[s.panel, place, slide]}>
        <View style={s.head}>
          <View style={s.headText}>
            <Text style={s.title}>Octet</Text>
            <Text style={s.sub}>{drawerSub(list.needs, list.running)}</Text>
          </View>
          <Pressable accessibilityRole="button" accessibilityLabel="New session" onPress={goNew} style={s.plus}>
            <Text style={s.plusText}>+</Text>
          </Pressable>
        </View>
        <Pressable
          accessibilityRole="button"
          accessibilityLabel={list.needs > 0 ? `Inbox, ${list.needs} need you` : "Inbox"}
          accessibilityState={selectedInbox ? SELECTED : NOT_SELECTED}
          onPress={goInbox}
          style={rowStyle(theme, selectedInbox)}
        >
          <Text style={[s.name, selectedInbox ? s.accent : s.fg]}>Inbox</Text>
          {list.needs > 0 ? (
            <View testID="drawer-badge" style={s.badge}>
              <Text style={s.badgeText}>{String(list.needs)}</Text>
            </View>
          ) : null}
        </Pressable>
        <View style={s.list}>
          <FlashList
            data={rows}
            extraData={pathname}
            renderItem={renderItem}
            keyExtractor={keyOf}
            ListHeaderComponent={header}
            contentContainerStyle={s.listContent}
          />
        </View>
        <View style={s.foot}>
          <Pressable
            accessibilityRole="button"
            accessibilityLabel="Settings"
            accessibilityState={selectedSettings ? SELECTED : NOT_SELECTED}
            onPress={goSettings}
            style={rowStyle(theme, selectedSettings)}
          >
            <Text style={[s.name, selectedSettings ? s.accent : s.fg]}>Settings</Text>
          </Pressable>
        </View>
      </Animated.View>
    </View>
  );
}

const NONE: DrawerRow[] = [];
const SCRIM_BOX = { backgroundColor: INK } as const;

/** What the edge swipe's responder reads: where the gutter is, and where the touch came down. A
 * plain object the render never reads (only the responder does), moved by `place`. */
function makeZone() {
  const zone = {
    enabled: false,
    top: 0,
    bottom: 0,
    x: 0,
    y: 0,
    dx: 0,
    dy: 0,
    place(enabled: boolean, top: number, bottom: number) {
      zone.enabled = enabled;
      zone.top = top;
      zone.bottom = bottom;
    },
  };
  return zone;
}

/**
 * Wraps the screens, so the drawer can take a swipe from the left edge. It claims nothing on touch
 * start, so a tap or a scroll that begins in the 18 pt gutter reaches the screen as usual; it takes a
 * touch only once it is a rightward drag, at least twice as wide as tall, that began in the gutter
 * (below the header, above the composer). Past 30 pt that opens the drawer; a vertical drag never does.
 * Where the drawer is off (Pair, New session) or already open it takes nothing.
 * While the drawer is open the screens under it are hidden from a screen reader on Android too (the
 * drawer is modal on iOS).
 */
export function DrawerHost({ children }: { children: ReactNode }) {
  const insets = useSafeAreaInsets();
  const { height } = useWindowDimensions();
  const open = useDrawerOpen();
  // Read by the responder, which is made once.
  const [zone] = useState(makeZone);
  const enabled = useDrawerEnabled() && !open;
  const top = insets.top + EDGE_TOP;
  const bottom = height - EDGE_BOTTOM;
  useLayoutEffect(() => {
    zone.place(enabled, top, bottom);
  }, [zone, enabled, top, bottom]);
  const [pan] = useState(() =>
    PanResponder.create({
      onStartShouldSetPanResponderCapture: (event) => {
        zone.x = event.nativeEvent.pageX;
        zone.y = event.nativeEvent.pageY;
        return false;
      },
      onMoveShouldSetPanResponder: (_event, g) => {
        if (!zone.enabled || zone.x > EDGE_WIDTH || zone.y < zone.top || zone.y > zone.bottom) return false;
        if (g.dx < CLAIM_DRAG || !horizontal(g.dx, g.dy)) return false;
        // A fast drag can pass the distance in its first move: it opens at once.
        if (g.dx > OPEN_DRAG) {
          openDrawer();
          return false;
        }
        zone.dx = g.dx;
        zone.dy = g.dy;
        return true;
      },
      onPanResponderMove: (_event, g) => {
        const dx = zone.dx + g.dx;
        const dy = zone.dy + g.dy;
        if (dx > OPEN_DRAG && horizontal(dx, dy)) openDrawer();
      },
    }),
  );
  return (
    <View testID="drawer-host" importantForAccessibility={open ? "no-hide-descendants" : "auto"} {...pan.panHandlers} style={HOST}>
      {children}
    </View>
  );
}

const HOST = { flex: 1 } as const;
