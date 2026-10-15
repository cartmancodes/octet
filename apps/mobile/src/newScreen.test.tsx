import { act, fireEvent, screen } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { router } from "expo-router";
import { StyleSheet } from "react-native";

import NewSession from "../app/new";
import Recent from "../app/recent";
import Session from "../app/session/[id]";
import { hear, info, launch, pairedStorage, type Rig } from "./screenRig";
import type { CatalogInfo, RecentInfo } from "./protocol";
import { octetStore } from "./store";
import { dark, light } from "./theme";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

beforeEach(async () => {
  await pairedStorage();
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
});
afterEach(() => jest.restoreAllMocks());

const flush = () => act(async () => void jest.advanceTimersByTime(60));
const requests = (rig: Rig, type: string) => rig.sockets.last.commandsOf(type);
const lastReq = (rig: Rig, type: string) => Number(requests(rig, type).at(-1)?.req);
const starts = (rig: Rig) => requests(rig, "start") as Record<string, unknown>[];
const prompts = (rig: Rig) => requests(rig, "prompt") as Record<string, unknown>[];

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
  await flush();
};

/** Sessions first, then New session on top of it, as the user gets there. */
async function open(list = [info("live")]) {
  const rig = await launch("/", { new: NewSession, recent: Recent, "session/[id]": Session }, list);
  await act(async () => router.push("/new"));
  await flush();
  return rig;
}
async function answerRecent(rig: Rig, items: RecentInfo[]) {
  await act(async () => {
    rig.sockets.last.receive({ type: "recent", req: lastReq(rig, "recent"), items });
    rig.clock.advance(50);
  });
  await flush();
}
async function ack(rig: Rig, type: string, message: string) {
  await act(async () => {
    rig.sockets.last.receive({ type: "ack", req: lastReq(rig, type), message });
  });
}
async function refuse(rig: Rig, type: string, reason: string, code: string) {
  await act(async () => {
    rig.sockets.last.receive({ type: "refused", req: lastReq(rig, type), reason, code });
  });
}
const button = (name: string | RegExp) => screen.queryByRole("button", { name });
const press = async (name: string | RegExp) => {
  await fireEvent.press(screen.getByRole("button", { name }));
  await flush();
};
const field = () => screen.getByLabelText("What should the agent do?");
const write = async (text: string) => {
  await fireEvent.changeText(field(), text);
  await flush();
};
const chip = (name: RegExp) => screen.getByRole("button", { name });
const start = () => press("Start session");
const typed = () => screen.queryByLabelText("Type full access to confirm");
const FULL = /^Mode/;

describe("New session", () => {
  describe("the screen", () => {
    it("has Cancel, a title, Start, a field and four chips, and no drawer", async () => {
      await open();
      expect(screen.getByText("New session")).toBeTruthy();
      expect(button("Cancel")).toBeTruthy();
      expect(button("Start session")).toBeTruthy();
      expect(field().props.multiline).toBe(true);
      expect(field().props.placeholder).toBe("What should the agent do?");
      expect(chip(/^Mac/)).toBeTruthy();
      expect(chip(/^Folder/)).toBeTruthy();
      expect(chip(/^Engine and model/)).toBeTruthy();
      expect(chip(/^Mode/)).toBeTruthy();
      expect(screen.queryByTestId("drawer")).toBeNull();
    });

    it("sizes every pressable to 44 pt and puts the hub's strings in Text", async () => {
      await open();
      for (const b of [button("Cancel"), button("Start session"), chip(/^Mac/), chip(/^Folder/), chip(/^Engine and model/), chip(/^Mode/)]) {
        const s = StyleSheet.flatten(b?.props.style);
        expect(Number(s.minHeight ?? s.height)).toBeGreaterThanOrEqual(44);
      }
    });

    it("Cancel goes back", async () => {
      const rig = await open();
      await press("Cancel");
      expect(rig.view.getPathname()).toBe("/");
    });

    it("Start is accent with text and muted without", async () => {
      await open();
      const color = () => {
        const label = screen.getByText("Start ↑");
        return StyleSheet.flatten(label.props.style).color;
      };
      expect([dark.muted, light.muted]).toContain(color());
      await write("fix the build");
      expect([dark.accent, light.accent]).toContain(color());
    });

    it("a muted Resume row opens the resume list", async () => {
      const rig = await open();
      await press("Resume a past session");
      expect(rig.view.getPathname()).toBe("/recent");
    });

    it("Resume takes New's place: back from the resumed session returns to the Inbox", async () => {
      const rig = await open();
      await press("Resume a past session");
      expect(rig.view.getPathname()).toBe("/recent");
      await answerRecent(rig, [{ engine: "claude", session: "s-1", workspace: "/Users/me/Projects/octet", title: "one", started: 1 }]);
      await press(/^Resume one/);
      await ack(rig, "start", "Starting octet-a1b2c3 in /Users/me/Projects/octet");
      await hear(rig, [info("live"), info("new-id", { tmux: "octet-a1b2c3", from_phone: true })]);
      await flush();
      expect(rig.view.getPathname()).toBe("/session/new-id");
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
    });
  });

  describe("the chips", () => {
    it("the Mac chip names the Mac, and a tap says where sessions start", async () => {
      await open();
      expect(chip(/^Mac/).props.accessibilityLabel).toBe("Mac mac");
      await fireEvent.press(chip(/^Mac/));
      expect(screen.getByText("Sessions start on mac.")).toBeTruthy();
    });

    it("the folder chip cycles the offered workspaces, by folder name", async () => {
      const rig = await open([info("live"), info("beta"), info("zeta")]);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder beta");
      await press(/^Folder/);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder live");
      await press(/^Folder/);
      await press(/^Folder/);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder beta");
      await press(/^Folder/);
      await write("go");
      await start();
      expect(starts(rig)[0]).toMatchObject({ workspace: "/Users/me/Projects/live" });
    });

    it("offers the recent items' workspaces too, once each", async () => {
      const rig = await open([info("live")]);
      const items: RecentInfo[] = [
        { engine: "claude", workspace: "/Users/me/Projects/zeta", session: "s1", title: "t", started: 1 },
        { engine: "claude", workspace: "/Users/me/Projects/live", session: "s2", title: "t", started: 1 },
      ];
      await act(async () => {
        rig.sockets.last.receive({ type: "recent", req: lastReq(rig, "recent"), items });
        rig.clock.advance(50);
      });
      await flush();
      await press(/^Folder/);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder zeta");
      await press(/^Folder/);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder live");
    });

    it("with no workspace known the folder chip says so and Start explains", async () => {
      const rig = await open([]);
      await answerRecent(rig, []);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder none");
      await write("go");
      await start();
      expect(screen.getByText("Start a session on the Mac first, or add a workspace to hub.json.")).toBeTruthy();
      expect(starts(rig)).toHaveLength(0);
    });

    it("before the Mac's folders arrive the chip says loading, and Start says so rather than none", async () => {
      const rig = await open([]);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder loading…");
      expect(screen.getByText("loading…")).toBeTruthy();
      await write("go");
      await start();
      expect(screen.getByText("Loading the Mac's folders…")).toBeTruthy();
      expect(screen.queryByText(/hub\.json/)).toBeNull();
      expect(starts(rig)).toHaveLength(0);
      await answerRecent(rig, [{ engine: "claude", workspace: "/Users/me/Projects/zeta", session: "s", title: "t", started: 1 }]);
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder zeta");
    });

    it("a failed folder request ends the loading: the live sessions' folders stand", async () => {
      const rig = await open([]);
      await refuse(rig, "recent", "Too many requests", "rate_limited");
      await flush();
      expect(chip(/^Folder/).props.accessibilityLabel).toBe("Folder none");
    });

    it("the engine chip cycles claude and codex with each engine's first model, or default", async () => {
      const rig = await open();
      expect(chip(/^Engine and model/).props.accessibilityLabel).toBe("Engine and model claude / default");
      await hearCatalog(rig);
      expect(chip(/^Engine and model/).props.accessibilityLabel).toBe("Engine and model claude / Sonnet");
      await press(/^Engine and model/);
      expect(chip(/^Engine and model/).props.accessibilityLabel).toBe("Engine and model codex / GPT");
      await write("go");
      await start();
      expect(starts(rig)[0]).toMatchObject({ engine: "codex", model: "gpt" });
      await press(/^Engine and model/); // locked: ignored while a start is out
    });

    it("sends no model when the catalog has none for the engine", async () => {
      const rig = await open();
      await write("go");
      await start();
      expect(starts(rig)[0]).not.toHaveProperty("model");
      expect(starts(rig)[0]).toMatchObject({ type: "start", engine: "claude", workspace: "/Users/me/Projects/live" });
      expect(starts(rig)[0]).not.toHaveProperty("confirm");
    });

    it("the mode chip starts at default mode and a default Start sends no mode", async () => {
      const rig = await open();
      expect(chip(/^Mode/).props.accessibilityLabel).toBe("Mode default mode");
      await write("go");
      await start();
      expect(starts(rig)[0]).not.toHaveProperty("mode");
      expect(starts(rig)[0]).not.toHaveProperty("confirm");
    });

    it("the mode chip cycles default, ask, accept-edits, auto and back, and Start sends it", async () => {
      const rig = await open();
      const label = () => chip(/^Mode/).props.accessibilityLabel;
      const seen = [label()];
      for (let i = 0; i < 4; i++) {
        await press(/^Mode/);
        seen.push(label());
      }
      expect(seen).toEqual(["Mode default mode", "Mode ask", "Mode accept-edits", "Mode auto", "Mode default mode"]);
      await press(/^Mode/); // ask
      await press(/^Mode/); // accept-edits
      await write("go");
      await start();
      expect(starts(rig)[0]).toMatchObject({ mode: "accept-edits" });
      expect(starts(rig)[0]).not.toHaveProperty("confirm");
    });

    it("a tap never reaches full access, however many cycle", async () => {
      const rig = await open();
      for (let i = 0; i < 9; i++) {
        await press(/^Mode/);
        expect(chip(/^Mode/).props.accessibilityLabel).not.toBe("Mode full access");
      }
      await write("go");
      await start();
      expect(starts(rig)[0]?.mode).not.toBe("full-access");
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
    });
  });

  describe("Start", () => {
    it("with empty text says so and sends nothing", async () => {
      const rig = await open();
      await start();
      expect(screen.getByText("Write a prompt first.")).toBeTruthy();
      expect(starts(rig)).toHaveLength(0);
      await write("   \n ");
      await start();
      expect(starts(rig)).toHaveLength(0);
    });

    it("sends the start, then the prompt when the session appears, and opens the conversation", async () => {
      const rig = await open();
      await write("fix the build");
      await start();
      expect(starts(rig)).toHaveLength(1);
      expect(prompts(rig)).toHaveLength(0); // not before the session exists
      await ack(rig, "start", "Starting octet-0a1b2c in /Users/me/Projects/live");
      expect(screen.getByText("Starting octet-0a1b2c — it will appear in the Inbox")).toBeTruthy();
      expect(prompts(rig)).toHaveLength(0);
      await hear(rig, [info("live"), info("other", { tmux: "octet-ffffff" })]); // not the one waited for
      expect(prompts(rig)).toHaveLength(0);
      expect(rig.view.getPathname()).toBe("/new");
      await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c", from_phone: true })]);
      await flush();
      expect(prompts(rig)).toEqual([expect.objectContaining({ type: "prompt", session: "fresh", text: "fix the build" })]);
      expect(rig.view.getPathname()).toBe("/session/fresh");
      // replaced, not pushed: back does not land on New session
      await act(async () => router.back());
      expect(rig.view.getPathname()).toBe("/");
    });

    it("sends the prompt once, however often the list changes", async () => {
      const rig = await open();
      await write("once");
      await start();
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c" })]);
      await flush();
      await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c", phase: "running" })]);
      await flush();
      expect(prompts(rig)).toHaveLength(1);
    });

    it("opens the session at once if it is already listed when the ack comes", async () => {
      const rig = await open();
      await write("hi");
      await start();
      await hear(rig, [info("live"), info("early", { tmux: "octet-0a1b2c" })]);
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await flush();
      expect(prompts(rig)).toEqual([expect.objectContaining({ session: "early", text: "hi" })]);
      expect(rig.view.getPathname()).toBe("/session/early");
    });

    it("still opens the conversation when the prompt is refused, and shows the hub's words", async () => {
      const rig = await open();
      await write("hello");
      await start();
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c" })]);
      await flush();
      expect(rig.view.getPathname()).toBe("/session/fresh");
      await refuse(rig, "prompt", "That session is busy", "busy");
      expect(screen.getByText("That session is busy. Your text is in the prompt.")).toBeTruthy();
      expect(rig.view.getPathname()).toBe("/session/fresh");
    });

    it("a refused first prompt keeps the text as that session's draft, in its composer", async () => {
      const rig = await open();
      await write("hello there");
      await start();
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c" })]);
      await flush();
      expect(rig.view.getPathname()).toBe("/session/fresh");
      await refuse(rig, "prompt", "That session is busy", "busy");
      expect(screen.getByText("That session is busy. Your text is in the prompt.")).toBeTruthy();
      expect(screen.getByLabelText("Message").props.value).toBe("hello there");
    });

    it("a draft waiting for a conversation fills its composer when it opens", async () => {
      const rig = await open();
      octetStore.getState().setDraft("fresh", "kept words");
      await act(async () => router.push("/session/fresh"));
      await hear(rig, [info("live"), info("fresh")]);
      await flush();
      expect(screen.getByLabelText("Message").props.value).toBe("kept words");
      expect(octetStore.getState().drafts.has("fresh")).toBe(false); // taken
    });

    it("shows a refused start as a toast with the hub's words, and allows another try", async () => {
      const rig = await open();
      await write("go");
      await start();
      await refuse(rig, "start", "Only 3 sessions at a time", "busy");
      expect(screen.getByText("Only 3 sessions at a time")).toBeTruthy();
      expect(prompts(rig)).toHaveLength(0);
      expect(field().props.value).toBe("go"); // the draft stays
      await start();
      expect(starts(rig)).toHaveLength(2);
    });

    it("says a lost start may have reached the Mac", async () => {
      const rig = await open();
      await write("go");
      await start();
      await act(async () => rig.clock.advance(46_000));
      expect(screen.getByText("Not sure this reached the Mac. Check the session before sending again.")).toBeTruthy();
      // never resent for the user
      expect(rig.sockets.all.flatMap((sock) => sock.commandsOf("start"))).toHaveLength(1);
    });

    it("allows one start at a time: Start is off while it is out and while the session is awaited", async () => {
      const rig = await open();
      await write("go");
      await start();
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
      await fireEvent.press(screen.getByRole("button", { name: "Start session" }));
      expect(starts(rig)).toHaveLength(1);
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
      await act(async () => void jest.advanceTimersByTime(31_000));
      expect(screen.getByText("octet-0a1b2c hasn't appeared yet. Check tmux on the Mac.")).toBeTruthy();
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(false);
      // it still opens if it comes later
      await hear(rig, [info("late", { tmux: "octet-0a1b2c" })]);
      await flush();
      expect(prompts(rig)).toEqual([expect.objectContaining({ session: "late", text: "go" })]);
      expect(rig.view.getPathname()).toBe("/session/late");
    });

    it("two presses in one frame send one start", async () => {
      const rig = await open();
      await write("go");
      const button = screen.getByRole("button", { name: "Start session" });
      // both presses reach the handler before React renders in between
      const click = button.props.onClick as (event: object) => void;
      const onPress = () => click({ stopPropagation() {}, nativeEvent: {} });
      await act(async () => {
        onPress();
        onPress();
      });
      await flush();
      expect(starts(rig)).toHaveLength(1);
    });

    it("an ack that names no session keeps Start locked for 10 s while the notice shows", async () => {
      const rig = await open();
      await write("go");
      await start();
      await ack(rig, "start", "ok");
      expect(screen.getByText("Started — it will appear in the Inbox")).toBeTruthy();
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
      await act(async () => void jest.advanceTimersByTime(9_000));
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
      await act(async () => void jest.advanceTimersByTime(2_000));
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(false);
      expect(screen.getByText("Started — it will appear in the Inbox")).toBeTruthy();
    });

    it("tells the user it started when the hub's words name no session, and sends no prompt", async () => {
      const rig = await open();
      await write("go");
      await start();
      await ack(rig, "start", "ok");
      expect(screen.getByText("Started — it will appear in the Inbox")).toBeTruthy();
      await hear(rig, [info("live"), info("x")]);
      expect(prompts(rig)).toHaveLength(0);
    });

    it("leaving the screen cancels the watch: no prompt, no navigation", async () => {
      const rig = await open();
      await write("go");
      await start();
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await press("Cancel");
      await hear(rig, [info("live"), info("later", { tmux: "octet-0a1b2c" })]);
      await act(async () => void jest.advanceTimersByTime(60_000));
      expect(prompts(rig)).toHaveLength(0);
      expect(rig.view.getPathname()).toBe("/");
    });

    it("refuses a slash command here: it runs only on the desk", async () => {
      const rig = await open();
      await write("/compact");
      await start();
      expect(screen.getByText("Slash commands run on the desk")).toBeTruthy();
      expect(starts(rig)).toHaveLength(0);
    });

    it("sends a path that starts with a slash as a prompt", async () => {
      const rig = await open();
      await write("/usr/lib is broken");
      await start();
      expect(starts(rig)).toHaveLength(1);
    });

    it("refuses a prompt over 64 KiB before starting anything", async () => {
      const rig = await open();
      await write("a".repeat(65_537));
      await start();
      expect(screen.getByText("That's too long to send (64 KB max).")).toBeTruthy();
      expect(starts(rig)).toHaveLength(0);
    });

    it("is off while the Mac can't be reached, and asks for nothing", async () => {
      const rig = await open();
      await write("go");
      await act(async () => {
        rig.sockets.last.drop();
      });
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(true);
      await fireEvent.press(screen.getByRole("button", { name: "Start session" }));
      expect(rig.sockets.all.flatMap((s) => s.commandsOf("start"))).toEqual([]);
      await fireEvent(chip(FULL), "longPress");
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(field().props.value).toBe("go"); // the draft is kept
      await act(async () => {
        rig.clock.advance(1_000);
        rig.sockets.last.open();
        rig.sockets.last.receive({ type: "welcome", device: "iPhone" });
        rig.clock.advance(50);
      });
      expect(button("Start session")?.props.accessibilityState.disabled).toBe(false);
    });
  });

  describe("full access", () => {
    const arm = async () => {
      await fireEvent(chip(FULL), "longPress");
      await flush();
    };

    it("a screen reader hears the long press, and can start it from the chip's actions", async () => {
      await open();
      const mode = chip(FULL);
      expect(mode.props.accessibilityHint).toBe("Long-press for full access");
      expect(mode.props.accessibilityActions).toEqual(expect.arrayContaining([{ name: "fullAccess", label: "full access" }]));
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      await act(async () => mode.props.onAccessibilityAction({ nativeEvent: { actionName: "fullAccess" } }));
      await flush();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(chip(FULL).props.accessibilityLabel).toBe("Mode full access");
    });

    it("a long press asks for Face ID, then the exact words, then sends with confirm", async () => {
      const rig = await open();
      await write("clean it up");
      await press(/^Mode/); // a plain tap asks nothing
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      await arm();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(chip(FULL).props.accessibilityLabel).toBe("Mode full access");
      expect(screen.getByText("full access")).toBeTruthy();
      const color = StyleSheet.flatten(screen.getByText("full access").props.style).color;
      expect([dark.amber, light.amber]).toContain(color);
      expect(starts(rig)).toHaveLength(0);
      await fireEvent.changeText(typed() as never, "full acces");
      expect(button("Turn on full access")?.props.accessibilityState.disabled).toBe(true);
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      expect(starts(rig)).toEqual([
        expect.objectContaining({ type: "start", engine: "claude", mode: "full-access", confirm: "full access" }),
      ]);
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await hear(rig, [info("live"), info("fresh", { tmux: "octet-0a1b2c" })]);
      await flush();
      expect(prompts(rig)).toEqual([expect.objectContaining({ session: "fresh", text: "clean it up" })]);
    });

    it("sends nothing and shows no field when Face ID fails", async () => {
      auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" } as never);
      const rig = await open();
      await arm();
      expect(typed()).toBeNull();
      expect(chip(/^Mode/).props.accessibilityLabel).toBe("Mode default mode");
      expect(starts(rig)).toHaveLength(0);
    });

    it("says it needs Face ID when the phone has none", async () => {
      auth.isEnrolledAsync.mockResolvedValueOnce(false);
      await open();
      await arm();
      expect(screen.getByText("Full access needs Face ID set up on this phone.")).toBeTruthy();
      expect(typed()).toBeNull();
    });

    it("Start while armed sends nothing: the words come first", async () => {
      const rig = await open();
      await write("go");
      await arm();
      await start();
      expect(starts(rig)).toHaveLength(0);
      expect(typed()).toBeTruthy();
    });

    it("Turn on with no prompt written says so and stays armed", async () => {
      const rig = await open();
      await arm();
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      expect(screen.getByText("Write a prompt first.")).toBeTruthy();
      expect(starts(rig)).toHaveLength(0);
      expect(typed()).toBeTruthy();
    });

    it("can be cancelled; a tap on the mode chip and a change of folder, engine or model disarm it", async () => {
      const rig = await open([info("live"), info("beta")]);
      await hearCatalog(rig);
      await write("go");
      await arm();
      await press("Cancel full access");
      expect(typed()).toBeNull();
      expect(chip(/^Mode/).props.accessibilityLabel).toBe("Mode default mode");
      for (const change of [/^Mode/, /^Folder/, /^Engine and model/]) {
        await arm();
        expect(typed()).toBeTruthy();
        await press(change);
        expect(typed()).toBeNull();
      }
      expect(starts(rig)).toHaveLength(0);
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(4);
    });

    it("disarms when the armed workspace leaves the list: it never falls back to another one", async () => {
      const rig = await open([info("live"), info("beta")]);
      await press(/^Folder/); // live
      await write("go");
      await arm();
      expect(typed()).toBeTruthy();
      await hear(rig, [info("beta")]);
      await flush();
      expect(typed()).toBeNull();
      expect(starts(rig)).toHaveLength(0);
    });

    it("arms nothing when Face ID answers after the form changed", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      await open([info("live"), info("beta")]);
      await arm();
      await press(/^Folder/); // the approved form changes while the prompt is up
      await act(async () => finish({ success: true }));
      expect(typed()).toBeNull();
    });

    it("a refusal disarms it: the next try asks for Face ID again", async () => {
      const rig = await open();
      await write("go");
      await arm();
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      await refuse(rig, "start", "Full access is off on this Mac", "not_allowed");
      expect(screen.getByText("Full access is off on this Mac")).toBeTruthy();
      expect(typed()).toBeNull();
      expect(chip(/^Mode/).props.accessibilityLabel).toBe("Mode default mode");
      await arm();
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
      expect((typed() as never as { props: { value: string } }).props.value).toBe("");
    });

    it("a sent start spends the approval: it needs Face ID and the words again", async () => {
      const rig = await open();
      await write("go");
      await arm();
      await fireEvent.changeText(typed() as never, "full access");
      await press("Turn on full access");
      expect(typed()).toBeNull();
      await ack(rig, "start", "Starting octet-0a1b2c in /w");
      await act(async () => void jest.advanceTimersByTime(31_000));
      expect(typed()).toBeNull();
      expect(button("Turn on full access")).toBeNull();
      await start();
      // what Start sends now is an ordinary default start: the words were spent
      expect(starts(rig)).toHaveLength(2);
      expect(starts(rig)[1]).not.toHaveProperty("mode");
      expect(starts(rig)[1]).not.toHaveProperty("confirm");
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    });
  });
});
