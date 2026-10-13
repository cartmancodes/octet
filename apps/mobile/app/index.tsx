import { StyleSheet, Text, View } from "react-native";

import { Mascot } from "../src/mascot/Mascot";
import { spacing, type, useTheme } from "../src/theme";

/** A placeholder home screen until the sessions list lands. */
export default function Home() {
  const theme = useTheme();
  return (
    <View style={[styles.screen, { backgroundColor: theme.bg }]}>
      <Mascot pose="idle" size={192} />
      <Text style={[type.display, { color: theme.fg }]}>Octet</Text>
      <Text style={[type.body, { color: theme.muted }]}>Paired with your Mac.</Text>
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1, alignItems: "center", justifyContent: "center", gap: spacing.md },
});
