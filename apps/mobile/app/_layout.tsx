import { Stack } from "expo-router";
import { StatusBar } from "expo-status-bar";

import { useTheme } from "../src/theme";

export default function RootLayout() {
  const theme = useTheme();
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
      />
    </>
  );
}
