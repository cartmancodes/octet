import { Stack, useNavigationContainerRef, usePathname, useRouter } from "expo-router";
import * as SplashScreen from "expo-splash-screen";
import { StatusBar } from "expo-status-bar";
import * as SystemUI from "expo-system-ui";
import { useEffect, useLayoutEffect, useRef } from "react";
import { AppState, View } from "react-native";

import { ApprovalToasts } from "../src/components/ApprovalToasts";
import { Drawer, DrawerHost } from "../src/components/Drawer";
import { closeDrawer, drawerScreen, setDrawerEnabled } from "../src/drawer";
import { useHubController, useHubSnapshot } from "../src/hub";
import { openSession } from "../src/navigation";
import { clearDelivered, configureNotifications, watchTaps } from "../src/notifications";
import { useTheme } from "../src/theme";
import { ToastHost } from "../src/ui";

// The native splash stays up until the app knows whether it is paired; app.json paints it in the
// theme's colours, so there is no flash between it and the first screen.
SplashScreen.preventAutoHideAsync().catch(() => undefined);

/** The drawer owns the left edge on the Inbox, a Conversation and Settings, so those screens give
 * up the system's back-swipe (a swipe from the edge, or from anywhere). Their header buttons and
 * Android back still go back. */
const OWN_EDGE = { gestureEnabled: false, fullScreenGestureEnabled: false } as const;

/**
 * The shell. The app makes one hub connection (`hub` in src/hub.ts, the context's default).
 * Without a token the user can reach only Pair (`Stack.Protected` redirects), and a token the
 * Mac revoked sends them back there. Put every other screen inside the paired group below.
 */
export default function RootLayout() {
  const theme = useTheme();
  const hub = useHubController();
  const router = useRouter();
  const nav = useNavigationContainerRef();
  const { status, offer } = useHubSnapshot();
  const pathname = usePathname();

  // The drawer belongs to the Inbox, a Conversation and Settings of a paired phone: anywhere else
  // (Pair, New session) it is closed and disabled. A layout effect, so no frame shows it open there.
  const drawerHere = status === "paired" && !offer && drawerScreen(pathname);
  useLayoutEffect(() => {
    setDrawerEnabled(drawerHere);
    return () => setDrawerEnabled(false);
  }, [drawerHere]);
  // Any navigation (a notification tap, a link, a tap on a drawer row) leaves the drawer shut.
  useEffect(() => closeDrawer(), [pathname]);

  useEffect(() => {
    // The socket and its timers live only while the app is in the foreground. `inactive` is a
    // pause (a Face ID prompt, the app switcher), not the background. A launch that reports no
    // state yet counts as foreground. Back in the app, the notifications in the tray are old
    // news: the screens show the live state.
    hub.setForeground(AppState.currentState !== "background");
    const subscription = AppState.addEventListener("change", (state) => {
      if (state === "active") {
        hub.setForeground(true);
        void clearDelivered();
      } else if (state === "background") hub.setForeground(false);
    });
    void hub.init();
    return () => subscription.remove();
  }, [hub]);

  // A tap opens the session it names, as the drawer does (back to it when it is in the stack);
  // unpaired, the phone can only pair; a tap that names no valid session lands on the Inbox. (The
  // Session screen already handles a session that is gone.)
  const open = (session: string | null) => {
    if (status === "unpaired") router.replace("/pair");
    else if (session === null) router.replace("/");
    else openSession(router, nav.getRootState(), pathname, session);
  };
  // The latest of these, for the listener below, which is set up once.
  const route = useRef({ status, offer, open });
  const waiting = useRef<{ session: string | null } | null>(null);
  useEffect(() => {
    route.current = { status, offer, open };
    if (status === "loading" || offer || waiting.current === null) return;
    // A tap that arrived before the saved pairing was read, or during the notifications step.
    const { session } = waiting.current;
    waiting.current = null;
    open(session);
  });

  // Nothing shows while the app is open; Android gets its channels. Then taps on notifications,
  // the one that launched the app included.
  useEffect(() => {
    void configureNotifications();
    return watchTaps((session) => {
      if (route.current.status === "loading" || route.current.offer) waiting.current = { session };
      else route.current.open(session);
    });
  }, []);

  // The splash lifts once the saved pairing is known.
  useEffect(() => {
    if (status !== "loading") SplashScreen.hideAsync().catch(() => undefined);
  }, [status]);

  // The native root view, behind every screen (Android's window, iOS's under a sheet as it
  // slides): app.json paints it dark before JavaScript runs; from here it follows the theme.
  useEffect(() => {
    SystemUI.setBackgroundColorAsync(theme.bg).catch(() => undefined);
  }, [theme.bg]);

  if (status === "loading") return <View style={{ flex: 1, backgroundColor: theme.bg }} />;

  const paired = status === "paired";
  return (
    <>
      <StatusBar style="auto" />
      <DrawerHost>
        <Stack
          screenOptions={{
            headerStyle: { backgroundColor: theme.panel },
            headerTintColor: theme.fg,
            headerShadowVisible: false,
            contentStyle: { backgroundColor: theme.bg },
          }}
        >
          <Stack.Protected guard={paired && !offer}>
            <Stack.Screen name="index" options={{ title: "Inbox", headerShown: false, ...OWN_EDGE }} />
            <Stack.Screen name="session/[id]" options={{ title: "Session", headerShown: false, ...OWN_EDGE }} />
            <Stack.Screen name="new" options={{ title: "New session", headerShown: false }} />
            <Stack.Screen name="recent" options={{ title: "Resume a session" }} />
            <Stack.Screen name="settings" options={{ title: "Settings", headerShown: false, ...OWN_EDGE }} />
          </Stack.Protected>
          <Stack.Protected guard={!paired || offer}>
            <Stack.Screen name="pair" options={{ title: "Pair", headerShown: false }} />
          </Stack.Protected>
        </Stack>
      </DrawerHost>
      {paired && !offer ? <ApprovalToasts /> : null}
      {paired && !offer ? <Drawer /> : null}
      {/* Last, so a toast draws over the drawer and its scrim: its action still works there. */}
      <ToastHost />
    </>
  );
}
