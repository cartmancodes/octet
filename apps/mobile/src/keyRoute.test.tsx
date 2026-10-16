// A session key holds a `/` (`"<mac>/<session>"`): the Conversation's route must carry it whole,
// and give it back to the screen.
import { act, fireEvent, screen, waitFor } from "@testing-library/react-native";
import { useLocalSearchParams, useNavigationContainerRef, usePathname, useRouter } from "expo-router";
import { renderRouter } from "expo-router/testing-library";
import { Pressable, Text } from "react-native";

import Session from "../app/session/[id]";
import { isSessionPath, openSession } from "./navigation";
import { info, launch, pairedStorage } from "./screenRig";

const KEY = "m0/123-456";

function Opener() {
  const router = useRouter();
  const nav = useNavigationContainerRef();
  const pathname = usePathname();
  return (
    <Pressable accessibilityRole="button" accessibilityLabel="Open" onPress={() => openSession(router, nav.getRootState(), pathname, KEY)}>
      <Text>Open</Text>
    </Pressable>
  );
}

function Shown() {
  const id = useLocalSearchParams<{ id: string }>().id;
  const pathname = usePathname();
  return (
    <>
      <Text testID="key">{id}</Text>
      <Text testID="here">{String(isSessionPath(pathname, id))}</Text>
      <Opener />
    </>
  );
}

describe("a keyed session route", () => {
  it("openSession with a key opens the Conversation of that key, which reads it back whole", async () => {
    const view = renderRouter({ index: Opener, "session/[id]": Shown }, { initialUrl: "/" });
    await view;
    await act(async () => fireEvent.press(screen.getByLabelText("Open")));
    expect(screen.getByTestId("key").props.children).toBe(KEY);
    expect(screen.getByTestId("here").props.children).toBe("true");
    // One route segment, the key encoded in it: not /session/m0 with a stray 123-456.
    expect(view.getSegments()).toEqual(["session", "[id]"]);
    // Opening it again from its own screen is nothing: no second copy.
    await act(async () => fireEvent.press(screen.getByLabelText("Open")));
    expect(screen.getAllByTestId("key")).toHaveLength(1);
  });

  it("a deep link with the key encoded gives the key back", async () => {
    await renderRouter({ index: () => null, "session/[id]": Shown }, { initialUrl: `/session/${encodeURIComponent(KEY)}` });
    expect(screen.getByTestId("key").props.children).toBe(KEY);
    expect(screen.getByTestId("here").props.children).toBe("true");
  });

  it("isSessionPath matches the decoded and the encoded path of a key, and no other", () => {
    expect(isSessionPath(`/session/${KEY}`, KEY)).toBe(true);
    expect(isSessionPath(`/session/${encodeURIComponent(KEY)}`, KEY)).toBe(true);
    expect(isSessionPath("/session/123-456", KEY)).toBe(false);
    expect(isSessionPath(`/session/${KEY}`, "m1/123-456")).toBe(false);
  });
});

describe("the Conversation on a keyed route", () => {
  beforeEach(pairedStorage);

  it("streams the raw session from the key's Mac", async () => {
    const rig = await launch(`/session/${encodeURIComponent(KEY)}`, { "session/[id]": Session }, [info("123-456")]);
    await waitFor(() => expect(rig.sockets.last.commandsOf("subscribe").map((c) => c.session)).toEqual(["123-456"]));
  });
});
