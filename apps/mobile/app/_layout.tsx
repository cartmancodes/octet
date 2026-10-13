import { Stack } from "expo-router";
import { StatusBar } from "expo-status-bar";
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
        </Stack.Protected>
        <Stack.Protected guard={!paired}>
          <Stack.Screen name="pair" options={{ title: "Pair", headerShown: false }} />
        </Stack.Protected>
      </Stack>
    </>
  );
}
