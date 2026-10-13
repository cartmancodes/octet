import { Stack } from "expo-router";
import { StatusBar } from "expo-status-bar";
import * as SystemUI from "expo-system-ui";
import { useEffect } from "react";
import { AppState, View } from "react-native";

import { useHubController, useHubSnapshot } from "../src/hub";
import { useTheme } from "../src/theme";

/**
 * The shell. The app makes one hub connection (`hub` in src/hub.ts, the context's default).
 * Without a token the user can reach only Pair (`Stack.Protected` redirects), and a token the
 * Mac revoked sends them back there. Put every other screen inside the paired group below.
 */
export default function RootLayout() {
  const theme = useTheme();
  const hub = useHubController();
  const { status } = useHubSnapshot();

  useEffect(() => {
    // The socket and its timers live only while the app is in the foreground. `inactive` is a
    // pause (a Face ID prompt, the app switcher), not the background. A launch that reports no
    // state yet counts as foreground.
    hub.setForeground(AppState.currentState !== "background");
    const subscription = AppState.addEventListener("change", (state) => {
      if (state === "active") hub.setForeground(true);
      else if (state === "background") hub.setForeground(false);
    });
    void hub.init();
    return () => subscription.remove();
  }, [hub]);

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
      <Stack
        screenOptions={{
          headerStyle: { backgroundColor: theme.panel },
          headerTintColor: theme.fg,
          headerShadowVisible: false,
          contentStyle: { backgroundColor: theme.bg },
        }}
      >
        <Stack.Protected guard={paired}>
          <Stack.Screen name="index" options={{ title: "Sessions" }} />
          <Stack.Screen name="session/[id]" options={{ title: "Session" }} />
          <Stack.Screen name="recent" options={{ title: "Recent and new" }} />
          <Stack.Screen name="settings" options={{ title: "Settings" }} />
        </Stack.Protected>
        <Stack.Protected guard={!paired}>
          <Stack.Screen name="pair" options={{ title: "Pair", headerShown: false }} />
        </Stack.Protected>
      </Stack>
    </>
  );
}
