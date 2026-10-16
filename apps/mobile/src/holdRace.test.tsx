// The hold race: a screen's stream effect runs with the client its hook last read. If that is no
// longer its Mac's link (the Mac was paired again in between), `watch` would take a hold on the new
// link, and the cleanup, which lets go only for its own client, would never give it back. So the
// effect does nothing unless the client it holds is the Mac's link now. Here the hook is made to
// lag on purpose: every screen's `useClient` returns a client that is not the link.
import { act } from "@testing-library/react-native";
import { router } from "expo-router";

import Changes from "../app/changes";
import Session from "../app/session/[id]";
import Usage from "../app/usage";
import { info, launch, pairedStorage } from "./screenRig";
import { k, sessionPath } from "./testKeys";

const mockStale = { client: { state: "ready" } };
jest.mock("./hub", () => {
  const actual = jest.requireActual<typeof import("./hub")>("./hub");
  return { ...actual, useClient: (mac: string | null) => (mac === null ? null : mockStale.client) };
});

const S = "4821-1790000000";

beforeEach(async () => {
  await pairedStorage();
});
afterEach(() => jest.restoreAllMocks());

describe("a screen whose client is not its Mac's link takes no hold", () => {
  it.each([
    ["the Conversation", sessionPath(S)],
    ["Usage", `/usage?id=${encodeURIComponent(k(S))}`],
    ["Changes", `/changes?id=${encodeURIComponent(k(S))}`],
  ])("%s subscribes to nothing", async (_screen, url) => {
    const rig = await launch("/", { "session/[id]": Session, usage: Usage, changes: Changes }, [info(S, { phase: "running" })]);
    await act(async () => router.push(url));
    await act(async () => void jest.advanceTimersByTime(200));
    expect(rig.sockets.last.commandsOf("subscribe")).toEqual([]);
  });
});
