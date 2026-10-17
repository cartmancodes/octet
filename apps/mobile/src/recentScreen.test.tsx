import { measureParentSize } from "@shopify/flash-list/dist/recyclerview/utils/measureLayout";
import { act, fireEvent, screen } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { router } from "expo-router";
import { StyleSheet } from "react-native";

import Recent from "../app/recent";
import Settings from "../app/settings";
import { hear, info, launch, pairedStorage, type Rig } from "./screenRig";
import type { CatalogInfo, RecentInfo } from "./protocol";
import { octetStore } from "./store";
import { dark, light } from "./theme";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

beforeEach(async () => {
  await pairedStorage();
  // A tall list, so FlashList draws every row (it draws only what is near the view).
  (measureParentSize as jest.Mock).mockReturnValue({ x: 0, y: 0, width: 400, height: 5000 });
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
});
afterEach(() => jest.restoreAllMocks());

const nowSeconds = () => Math.floor(Date.now() / 1000);
const item = (session: string, extra: Partial<RecentInfo> = {}): RecentInfo => ({
  engine: "claude",
  workspace: "/Users/me/Projects/octet",
  session,
  title: `Title of ${session}`,
  started: nowSeconds() - 3 * 3600,
  ...extra,
});

/** Sessions first, then Recent on top of it, as the user gets there. */
async function open(list = [info("live")]) {
  const rig = await launch("/", { recent: Recent, settings: Settings }, list);
  await act(async () => router.push("/recent"));
  await flush();
  return rig;
}
const requests = (rig: Rig, type: string) => rig.sockets.last.commandsOf(type);
const lastReq = (rig: Rig, type: string) => Number(requests(rig, type).at(-1)?.req);
const flush = () => act(async () => void jest.advanceTimersByTime(60));

/** The Mac answers the newest `recent` request. */
async function answer(rig: Rig, items: RecentInfo[]) {
  await act(async () => {
    rig.sockets.last.receive({ type: "recent", req: lastReq(rig, "recent"), items });
    rig.clock.advance(50);
  });
  await flush();
}
async function refuse(rig: Rig, type: string, reason: string, code: string, req = lastReq(rig, type)) {
  await act(async () => {
    rig.sockets.last.receive({ type: "refused", req, reason, code });
  });
}
async function ack(rig: Rig, type: string, message: string) {
  await act(async () => {
    rig.sockets.last.receive({ type: "ack", req: lastReq(rig, type), message });
  });
}
const button = (name: string | RegExp) => screen.queryByRole("button", { name });
const press = async (name: string | RegExp) => {
  await fireEvent.press(screen.getByRole("button", { name }));
  await flush();
};
const radio = (name: string | RegExp) => screen.queryByRole("radio", { name });
const choose = async (name: string | RegExp) => {
  await fireEvent.press(screen.getByRole("radio", { name }));
  await flush();
};
const tab = async (name: "Recent" | "New") => {
  await fireEvent.press(screen.getByRole("tab", { name }));
  await flush();
};

describe("Recent", () => {
  it("asks the Mac once the connection is ready and lists what it answers", async () => {
    const rig = await open();
    expect(requests(rig, "recent")).toHaveLength(1);
    expect(requests(rig, "recent")[0]).not.toHaveProperty("workspace");
    await answer(rig, [
      item("one", { title: "Fix the hub", engine: "codex", workspace: "/Users/me/Projects/notes" }),
      item("two", { title: "", started: nowSeconds() - 5 * 86_400 }),
    ]);
    expect(screen.getByRole("button", { name: /^Resume Fix the hub, notes/ })).toBeTruthy();
    expect(screen.getByText("Codex")).toBeTruthy();
    expect(screen.getByText("Fix the hub")).toBeTruthy();
    expect(screen.getByText("notes")).toBeTruthy(); // the folder, not the path
    expect(screen.queryByText("/Users/me/Projects/notes")).toBeNull();
    expect(screen.getByText("3 h ago")).toBeTruthy();
    expect(screen.getByText("Untitled")).toBeTruthy();
    expect(screen.getByText("5 d ago")).toBeTruthy();
    expect(screen.getByText("Claude")).toBeTruthy();
  });

  it("matches the answer to its own request", async () => {
    const rig = await open();
    const req = lastReq(rig, "recent");
    await act(async () => {
      rig.sockets.last.receive({ type: "recent", req: req + 100, items: [item("stranger")] });
    });
    expect(screen.queryByText("Title of stranger")).toBeNull();
    await answer(rig, [item("mine")]);
    expect(screen.getByText("Title of mine")).toBeTruthy();
  });

  it("shows nothing but a note while loading, and says when there are none", async () => {
    const rig = await open();
    expect(screen.getByText("Loading recent sessions…")).toBeTruthy();
    await answer(rig, []);
    expect(screen.queryByText("Loading recent sessions…")).toBeNull();
    expect(screen.getByText("No recent sessions")).toBeTruthy();
  });

  it("reloads on pull to refresh, and the spinner follows the request", async () => {
    const rig = await open();
    await answer(rig, [item("one")]);
    const list = () => screen.getByTestId("recent-list");
    expect(list().props.refreshControl.props.refreshing).toBe(false);
    await act(async () => {
      list().props.refreshControl.props.onRefresh();
    });
    expect(requests(rig, "recent")).toHaveLength(2);
    expect(list().props.refreshControl.props.refreshing).toBe(true);
    await answer(rig, [item("one"), item("two")]);
    expect(list().props.refreshControl.props.refreshing).toBe(false);
    expect(screen.getByText("Title of two")).toBeTruthy();
  });

  it("drops an older answer that comes after a newer request", async () => {
    const rig = await open();
    await answer(rig, [item("one")]);
    await act(async () => {
      screen.getByTestId("recent-list").props.refreshControl.props.onRefresh();
    });
    const second = lastReq(rig, "recent");
    await act(async () => {
      screen.getByTestId("recent-list").props.refreshControl.props.onRefresh();
    });
    await answer(rig, [item("third")]);
    await act(async () => {
      rig.sockets.last.receive({ type: "recent", req: second, items: [item("second")] });
    });
    expect(screen.getByText("Title of third")).toBeTruthy();
    expect(screen.queryByText("Title of second")).toBeNull();
  });

  it("shows a refusal and a Try again that asks again", async () => {
    const rig = await open();
    await refuse(rig, "recent", "Too many requests, wait a moment", "rate_limited");
    expect(screen.getByText("Too many requests, wait a moment")).toBeTruthy();
    await press("Try again");
    expect(requests(rig, "recent")).toHaveLength(2);
    expect(screen.queryByText("Too many requests, wait a moment")).toBeNull();
    await answer(rig, [item("one")]);
    expect(screen.getByText("Title of one")).toBeTruthy();
    expect(button("Try again")).toBeNull();
  });

  it("a reload after a reconnect clears the old failure at once", async () => {
    const rig = await open();
    await refuse(rig, "recent", "Too many requests, wait a moment", "rate_limited");
    expect(screen.getByText("Too many requests, wait a moment")).toBeTruthy();
    await act(async () => {
      rig.sockets.last.drop();
    });
    await act(async () => {
      rig.clock.advance(60_000);
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    expect(requests(rig, "recent")).toHaveLength(1); // the new socket's own
    expect(screen.queryByText("Too many requests, wait a moment")).toBeNull();
    expect(button("Try again")).toBeNull();
  });

  it("offers Try again after a lost request too: recent only reads", async () => {
    const rig = await open();
    // A silent link: its unanswered ping takes it down, the request with it, and it reconnects.
    await act(async () => rig.clock.advance(46_000));
    expect(screen.getByText(/Not sure this reached the Mac/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
    await act(async () => {
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    expect(requests(rig, "recent")).toHaveLength(1); // asked again on the new link
    // The reload cleared the failure.
    expect(screen.queryByText(/Not sure this reached the Mac/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("ignores a reply that comes after the screen left", async () => {
    const error = jest.spyOn(console, "error").mockImplementation(() => {});
    const rig = await open();
    await act(async () => router.back());
    await answer(rig, [item("late")]);
    expect(screen.queryByText("Title of late")).toBeNull();
    expect(error).not.toHaveBeenCalled();
  });

  it("draws a hub string as plain text", async () => {
    const rig = await open();
    await answer(rig, [item("x", { title: "<b>bold</b> [x](https://evil.example)", workspace: "/w/$(rm -rf)" })]);
    expect(screen.getByText("<b>bold</b> [x](https://evil.example)")).toBeTruthy();
    expect(screen.getByText("$(rm -rf)")).toBeTruthy();
  });

  it("sizes and themes the Resume button", async () => {
    const rig = await open();
    await answer(rig, [item("one")]);
    const resume = screen.getByRole("button", { name: /^Resume/ });
    const style = StyleSheet.flatten(resume.props.style);
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
    expect(style.minWidth).toBeGreaterThanOrEqual(44);
    expect([dark.fg, light.fg]).toContain(StyleSheet.flatten(screen.getByText("Title of one").props.style).color);
  });

  describe("Resume", () => {
    it("sends start with the engine, workspace and session, and no model or mode", async () => {
      const rig = await open();
      await answer(rig, [item("sess-1", { engine: "codex", workspace: "/Users/me/Projects/notes", model: "gpt-6" })]);
      await press(/^Resume/);
      expect(requests(rig, "start")).toEqual([
        expect.objectContaining({ type: "start", engine: "codex", workspace: "/Users/me/Projects/notes", resume: "sess-1" }),
      ]);
      const sent = requests(rig, "start")[0] as Record<string, unknown>;
      expect(sent).not.toHaveProperty("model");
      expect(sent).not.toHaveProperty("mode");
      expect(sent).not.toHaveProperty("confirm");
    });

    it("shows the refusal in the hub's words", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await refuse(rig, "start", "That workspace is not allowed", "not_allowed");
      expect(screen.getByText("That workspace is not allowed")).toBeTruthy();
      expect(button(/^Resume/)?.props.accessibilityState.disabled).toBeFalsy();
    });

    it("says a lost start may have reached the Mac, and does not offer to resend", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await act(async () => rig.clock.advance(46_000)); // past the request's 45 s
      expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
      expect(button("Try again")).toBeNull();
    });

    it("shows Starting, then opens the session when one with that tmux name appears", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await ack(rig, "start", "Starting octet-a1b2c3 in /Users/me/Projects/octet");
      expect(screen.getByText("Starting octet-a1b2c3 — it will appear in Sessions")).toBeTruthy();
      // A session that is not the one waited for changes nothing.
      await hear(rig, [info("live"), info("other", { tmux: "octet-ffffff" })]);
      expect(rig.view.getPathname()).toBe("/recent");
      await hear(rig, [info("live"), info("new-id", { tmux: "octet-a1b2c3", from_phone: true })]);
      await flush();
      expect(rig.view.getPathname()).toBe("/session/new-id");
      // replaced, not pushed: back does not land on Recent
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
    });

    it("opens the session at once if it is already listed when the ack comes", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await hear(rig, [info("live"), info("early", { tmux: "octet-a1b2c3" })]);
      await ack(rig, "start", "Starting octet-a1b2c3 in /w");
      await flush();
      expect(rig.view.getPathname()).toBe("/session/early");
    });

    it("says it has not appeared after 30 seconds, and still opens it if it comes later", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await ack(rig, "start", "Starting octet-a1b2c3 in /w");
      await act(async () => void jest.advanceTimersByTime(29_000));
      expect(screen.getByText("Starting octet-a1b2c3 — it will appear in Sessions")).toBeTruthy();
      await act(async () => void jest.advanceTimersByTime(2_000));
      expect(screen.getByText("octet-a1b2c3 hasn't appeared yet. Check tmux on the Mac.")).toBeTruthy();
      expect(screen.queryByText(/it will appear in Sessions/)).toBeNull();
      await hear(rig, [info("late", { tmux: "octet-a1b2c3" })]);
      await flush();
      expect(rig.view.getPathname()).toBe("/session/late");
    });

    it("leaving the screen cancels the watch", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await ack(rig, "start", "Starting octet-a1b2c3 in /w");
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
      await hear(rig, [info("later", { tmux: "octet-a1b2c3" })]);
      await act(async () => void jest.advanceTimersByTime(60_000));
      expect(rig.view.getPathname()).toBe("/");
    });

    it("allows one start at a time: disabled while it is out and while a launch is awaited", async () => {
      const rig = await open();
      await answer(rig, [item("one"), item("two")]);
      await press(/^Resume Title of one/);
      for (const b of screen.getAllByRole("button", { name: /^Resume/ })) expect(b.props.accessibilityState.disabled).toBe(true);
      await fireEvent.press(screen.getByRole("button", { name: /^Resume Title of two/ }));
      expect(requests(rig, "start")).toHaveLength(1);
      await ack(rig, "start", "Starting octet-a1b2c3 in /w");
      for (const b of screen.getAllByRole("button", { name: /^Resume/ })) expect(b.props.accessibilityState.disabled).toBe(true);
      // After 30 seconds the wait is over: the user may start another.
      await act(async () => void jest.advanceTimersByTime(31_000));
      for (const b of screen.getAllByRole("button", { name: /^Resume/ })) expect(b.props.accessibilityState.disabled).toBe(false);
    });

    it("a second start does not navigate to the first one's session", async () => {
      const rig = await open();
      await answer(rig, [item("one"), item("two")]);
      await press(/^Resume Title of one/);
      await ack(rig, "start", "Starting octet-aaaaaa in /w");
      await act(async () => void jest.advanceTimersByTime(31_000)); // late; buttons are free again
      await press(/^Resume Title of two/);
      expect(screen.queryByText(/octet-aaaaaa/)).toBeNull(); // the old notice is gone
      await hear(rig, [info("first", { tmux: "octet-aaaaaa" })]); // the first one turns up now
      await flush();
      expect(rig.view.getPathname()).toBe("/recent");
      await ack(rig, "start", "Starting octet-bbbbbb in /w");
      await hear(rig, [info("first", { tmux: "octet-aaaaaa" }), info("second", { tmux: "octet-bbbbbb" })]);
      await flush();
      expect(rig.view.getPathname()).toBe("/session/second");
    });

    it("tells the user it started even if the hub's words do not name the session", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      await ack(rig, "start", "ok");
      expect(screen.getByText("Started — it will appear in Sessions")).toBeTruthy();
      expect(button(/^Resume/)?.props.accessibilityState.disabled).toBe(false);
    });
  });
});

describe("New", () => {
  const catalog = (models: CatalogInfo["models"]): CatalogInfo => ({
    modes: ["ask", "accept-edits", "auto", "full-access"],
    current_mode: "ask",
    models,
    current: null,
  });
  const MODELS: CatalogInfo["models"] = [
    { engine: "claude", selection: "sonnet", name: "Sonnet", id: "claude-sonnet-5" },
    { engine: "claude", selection: "opus", name: "Opus", id: "claude-opus-5" },
    { engine: "codex", selection: "gpt", name: "GPT", id: "gpt-6" },
  ];
  const hearCatalog = async (rig: Rig, models = MODELS) => {
    await act(async () => {
      octetStore.getState().applyBatch([{ v: 1, type: "catalog", session: "live", catalog: catalog(models) }]);
      rig.clock.advance(50);
    });
  };
  const openNew = async (recent: RecentInfo[] = [], list = [info("live")]) => {
    const rig = await open(list);
    await answer(rig, recent);
    await tab("New");
    return rig;
  };
  const startsSent = (rig: Rig) => requests(rig, "start") as Record<string, unknown>[];

  it("has an engine picker of Claude, Codex and Demo, Claude chosen", async () => {
    await openNew();
    expect(["Claude", "Codex", "Demo"].map((n) => radio(n)?.props.accessibilityState.checked)).toEqual([true, false, false]);
    await choose("Codex");
    expect(radio("Codex")?.props.accessibilityState.checked).toBe(true);
    expect(radio("Claude")?.props.accessibilityState.checked).toBe(false);
  });

  it("offers the live sessions' and the recent items' workspaces, once each, by folder name", async () => {
    await openNew(
      [item("a", { workspace: "/Users/me/Projects/zeta" }), item("b", { workspace: "/Users/me/Projects/live" })],
      [info("live"), info("beta")],
    );
    const names = screen.getAllByTestId(/^workspace:/);
    expect(names.map((r) => r.props.accessibilityLabel)).toEqual(["beta", "live", "zeta"]);
  });

  it("chooses the first workspace until another is tapped", async () => {
    const rig = await openNew([], [info("live"), info("beta")]);
    expect(radio("beta")?.props.accessibilityState.checked).toBe(true);
    await choose("live");
    await press("Start session");
    expect(startsSent(rig)[0]).toMatchObject({ workspace: "/Users/me/Projects/live" });
  });

  it("with no workspace known says so and shows no form", async () => {
    await openNew([], []);
    expect(screen.getByText("Start a session on the Mac first, or add a workspace to hub.json.")).toBeTruthy();
    expect(button("Start session")).toBeNull();
    expect(radio("Claude")).toBeNull();
  });

  it("sends start with the engine and the workspace, and no model or mode, by default", async () => {
    const rig = await openNew();
    await press("Start session");
    expect(startsSent(rig)).toHaveLength(1);
    const sent = startsSent(rig)[0] as Record<string, unknown>;
    expect(sent).toMatchObject({ type: "start", engine: "claude", workspace: "/Users/me/Projects/live" });
    for (const key of ["model", "mode", "confirm", "resume"]) expect(sent).not.toHaveProperty(key);
  });

  it("offers only Default for a model when no live session has sent its models", async () => {
    await openNew();
    expect(screen.getAllByRole("radio", { name: "Default" })).toHaveLength(2); // model and mode
    expect(radio("Sonnet")).toBeNull();
  });

  it("offers the chosen engine's models from the live sessions' catalogs", async () => {
    const rig = await openNew();
    await hearCatalog(rig);
    await flush();
    expect(radio("Sonnet")).toBeTruthy();
    expect(radio("Opus")).toBeTruthy();
    expect(radio("GPT")).toBeNull();
    await choose("Codex");
    expect(radio("GPT")).toBeTruthy();
    expect(radio("Sonnet")).toBeNull();
    await choose("Demo");
    expect(radio("GPT")).toBeNull();
  });

  it("sends the chosen model and mode", async () => {
    const rig = await openNew();
    await hearCatalog(rig);
    await flush();
    await choose("Opus");
    await choose("Accept edits");
    await press("Start session");
    expect(startsSent(rig)[0]).toMatchObject({ engine: "claude", model: "opus", mode: "accept-edits" });
    expect(startsSent(rig)[0]).not.toHaveProperty("confirm");
  });

  it("forgets the model when the engine changes", async () => {
    const rig = await openNew();
    await hearCatalog(rig);
    await flush();
    await choose("Opus");
    await choose("Codex");
    await press("Start session");
    expect(startsSent(rig)[0]).not.toHaveProperty("model");
  });

  it("shows the refusal and allows another try", async () => {
    const rig = await openNew();
    await press("Start session");
    await refuse(rig, "start", "Only 3 sessions at a time", "busy");
    expect(screen.getByText("Only 3 sessions at a time")).toBeTruthy();
    await press("Start session");
    expect(startsSent(rig)).toHaveLength(2);
  });

  it("shows Starting after the ack, and opens the session when it appears", async () => {
    const rig = await openNew();
    await press("Start session");
    expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
    await ack(rig, "start", "Starting octet-0a1b2c in /Users/me/Projects/live");
    expect(screen.getByText("Starting octet-0a1b2c — it will appear in Sessions")).toBeTruthy();
    expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
    await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c" })]);
    await flush();
    expect(rig.view.getPathname()).toBe("/session/fresh");
  });

  it("takes no action while the Mac can't be reached: Resume and Start are off, and nothing asks for Face ID", async () => {
    const rig = await openNew([item("one")]);
    await choose(/^Full access/);
    await act(async () => {
      rig.sockets.last.drop();
    });
    const off = (name: string | RegExp) => button(name)?.props.accessibilityState?.disabled === true;
    expect(off("Start session")).toBe(true);
    await press("Start session");
    expect(auth.authenticateAsync).not.toHaveBeenCalled();
    await tab("Recent");
    expect(off(/^Resume/)).toBe(true);
    await press(/^Resume/);
    expect(rig.sockets.all.flatMap((sock) => sock.commandsOf("start"))).toEqual([]);
    // Back online, they are live again.
    await act(async () => {
      rig.clock.advance(1_000);
      rig.sockets.last.open();
      rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
      rig.clock.advance(50);
    });
    await answer(rig, [item("one")]);
    expect(off(/^Resume/)).toBe(false);
  });

  describe("full access", () => {
    const FULL = /^Full access/;
    const typed = () => screen.queryByLabelText("Type full access to confirm");
    const armed = async (rig: Rig) => {
      await choose(FULL);
      await press("Start session");
      return rig;
    };

    it("asks for Face ID, whatever the setting says, then the exact words, then sends with confirm", async () => {
      const rig = await openNew();
      await choose(FULL);
      expect(auth.authenticateAsync).not.toHaveBeenCalled(); // choosing alone asks nothing
      await press("Start session");
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(startsSent(rig)).toHaveLength(0);
      await fireEvent.changeText(typed() as never, "full acces");
      expect(button("Turn on full access")?.props.accessibilityState.disabled).toBe(true);
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      expect(startsSent(rig)).toEqual([
        expect.objectContaining({ type: "start", engine: "claude", mode: "full-access", confirm: "full access" }),
      ]);
    });

    it("sends nothing and shows no field when Face ID fails", async () => {
      auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" } as never);
      const rig = await armed(await openNew());
      expect(typed()).toBeNull();
      expect(startsSent(rig)).toHaveLength(0);
    });

    it("says it needs Face ID when the phone has none", async () => {
      auth.isEnrolledAsync.mockResolvedValueOnce(false);
      const rig = await armed(await openNew());
      expect(screen.getByText("Full access needs Face ID set up on this phone.")).toBeTruthy();
      expect(typed()).toBeNull();
      expect(startsSent(rig)).toHaveLength(0);
    });

    it("can be cancelled, and changing the mode disarms it", async () => {
      const rig = await armed(await openNew());
      await press("Cancel full access");
      expect(typed()).toBeNull();
      await press("Start session");
      expect(typed()).toBeTruthy();
      await choose("Auto");
      expect(typed()).toBeNull();
      await press("Start session");
      expect(startsSent(rig)[0]).toMatchObject({ mode: "auto" });
      expect(startsSent(rig)[0]).not.toHaveProperty("confirm");
    });

    it("a refusal disarms it: the next try asks for Face ID again", async () => {
      const rig = await armed(await openNew());
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      await refuse(rig, "start", "Full access is off on this Mac", "not_allowed");
      expect(screen.getByText("Full access is off on this Mac")).toBeTruthy();
      expect(typed()).toBeNull();
      await press("Start session");
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
      expect((typed() as never as { props: { value: string } }).props.value).toBe("");
    });

    it("a successful start disarms the panel: after the ack and the late notice the next Start asks for Face ID again", async () => {
      const rig = await armed(await openNew());
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      expect(typed()).toBeNull(); // sent: the words are spent
      await ack(rig, "start", "Starting octet-a1b2c3 in /Users/me/Projects/live");
      await act(async () => void jest.advanceTimersByTime(31_000));
      expect(screen.getByText("octet-a1b2c3 hasn't appeared yet. Check tmux on the Mac.")).toBeTruthy();
      expect(typed()).toBeNull();
      expect(button("Turn on full access")).toBeNull();
      await press("Start session");
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
      expect(startsSent(rig)).toHaveLength(1); // nothing went without a fresh Face ID
    });

    it("a start whose ack names no session also disarms it", async () => {
      const rig = await armed(await openNew());
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      await ack(rig, "start", "ok");
      expect(button("Turn on full access")).toBeNull();
      expect(typed()).toBeNull();
    });

    it("disarms when the armed workspace leaves the list: it never falls back to another one", async () => {
      const rig = await openNew([], [info("live"), info("beta")]);
      await choose("live");
      await choose(FULL);
      await press("Start session");
      expect(typed()).toBeTruthy();
      await hear(rig, [info("beta")]); // "live" is gone; the form would now fall back to beta
      await flush();
      expect(typed()).toBeNull();
      expect(button("Turn on full access")).toBeNull();
      expect(startsSent(rig)).toHaveLength(0);
    });

    it("disarms when the model leaves the catalog, and when another is chosen", async () => {
      const rig = await openNew();
      await hearCatalog(rig);
      await flush();
      await choose("Opus");
      await choose(FULL);
      await press("Start session");
      expect(typed()).toBeTruthy();
      await hearCatalog(rig, MODELS.filter((m) => m.selection !== "opus"));
      await flush();
      expect(typed()).toBeNull();
      await press("Start session");
      expect(typed()).toBeTruthy();
      await choose("Sonnet");
      expect(typed()).toBeNull();
      expect(startsSent(rig)).toHaveLength(0);
    });

    it("arms nothing when Face ID answers after the form changed", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      const rig = await openNew([], [info("live"), info("beta")]);
      await choose("live");
      await choose(FULL);
      await fireEvent.press(screen.getByRole("button", { name: "Start session" }));
      await flush();
      await hear(rig, [info("beta")]); // the approved workspace goes while the prompt is up
      await flush();
      await act(async () => finish({ success: true }));
      expect(typed()).toBeNull();
      expect(startsSent(rig)).toHaveLength(0);
      // and the next Start asks again
      await press("Start session");
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
    });

    it("a Resume never carries full access", async () => {
      const rig = await open();
      await answer(rig, [item("one")]);
      await press(/^Resume/);
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(startsSent(rig)[0]).not.toHaveProperty("mode");
    });
  });
});
