import { act, fireEvent, render, screen } from "@testing-library/react-native";
import * as LocalAuthentication from "expo-local-authentication";
import { measureParentSize } from "@shopify/flash-list/dist/recyclerview/utils/measureLayout";
import { StyleSheet } from "react-native";

import { Refusal, type RequestBody } from "../client/HubClient";
import type { CatalogInfo } from "../protocol";
import { saveSettings } from "../persist";
import { dark, light } from "../theme";
import { ModeModelSheet } from "./ModeModelSheet";

const auth = LocalAuthentication as jest.Mocked<typeof LocalAuthentication>;

const CATALOG: CatalogInfo = {
  modes: ["ask", "accept-edits", "auto", "full-access"],
  current_mode: "ask",
  models: [
    { engine: "claude", selection: "sonnet", name: "Sonnet", id: "claude-sonnet-5" },
    { engine: "claude", selection: "opus", name: "Opus", id: "claude-opus-5" },
    { engine: "codex", selection: "gpt", name: "GPT", id: "gpt-6" },
    { engine: "claude", selection: "haiku", name: "Haiku", id: "claude-haiku-5" },
  ],
  current: { engine: "claude", selection: "sonnet" },
};

/** A `send` the test settles by hand. */
function rig() {
  const sent: RequestBody[] = [];
  const waiting: { resolve: () => void; reject: (e: unknown) => void }[] = [];
  const send = jest.fn((body: RequestBody) => {
    sent.push(body);
    return new Promise<unknown>((resolve, reject) => waiting.push({ resolve: () => resolve({}), reject }));
  });
  return { sent, send, waiting };
}
const flush = () => act(async () => {});
/** Lets FlashList's load timer fire inside act. */
const settle = () => act(async () => void (await new Promise((resolve) => setTimeout(resolve, 5))));
const sheet = (catalog: CatalogInfo | null, send: ReturnType<typeof rig>["send"]) => (
  <ModeModelSheet session="s" catalog={catalog} send={send} />
);
const button = (name: string | RegExp) => screen.queryByRole("button", { name });
const press = async (name: string | RegExp) => {
  await fireEvent.press(screen.getByRole("button", { name }));
  await flush();
};
const open = async (catalog: CatalogInfo | null, r: ReturnType<typeof rig>) => {
  const view = await render(sheet(catalog, r.send));
  if (catalog) {
    await press("Mode and model");
    await settle();
  }
  return view;
};

beforeEach(async () => {
  // A tall list, so FlashList draws every row of the catalog (it draws only what is near the view).
  (measureParentSize as jest.Mock).mockReturnValue({ x: 0, y: 0, width: 400, height: 5000 });
  auth.hasHardwareAsync.mockResolvedValue(true);
  auth.isEnrolledAsync.mockResolvedValue(true);
  auth.authenticateAsync.mockResolvedValue({ success: true });
  auth.authenticateAsync.mockClear();
  await saveSettings({ faceId: false });
});

describe("the mode and model sheet", () => {
  it("shows its button only once the catalog is known, and opens on it", async () => {
    const r = rig();
    const view = await render(sheet(null, r.send));
    expect(button("Mode and model")).toBeNull();
    await view.rerender(sheet(CATALOG, r.send));
    expect(screen.queryByText("Ask")).toBeNull();
    await press("Mode and model");
    expect(screen.getByText("Ask")).toBeTruthy();
    await press("Close");
    expect(screen.queryByText("Ask")).toBeNull();
  });

  it("lists the modes with the current one marked", async () => {
    await open(CATALOG, rig());
    for (const name of ["Ask", "Accept edits", "Auto", "Full access"]) expect(screen.getByText(name)).toBeTruthy();
    expect(button("Ask, current")).toBeTruthy();
    expect(button("Auto")).toBeTruthy();
    expect(button("Auto, current")).toBeNull();
  });

  it("lists the models grouped by engine, in catalog order, with the current one marked", async () => {
    await open(CATALOG, rig());
    const names = screen.getAllByText(/^(Claude|Codex|Sonnet|Opus|Haiku|GPT)$/).map((n) => n.props.children);
    // Haiku follows its engine's group even though the catalog listed it after a Codex model.
    expect(names).toEqual(["Claude", "Sonnet", "Opus", "Haiku", "Codex", "GPT"]);
    expect(button("Sonnet, current")).toBeTruthy();
    expect(button("Opus, current")).toBeNull();
    expect(screen.getByText("claude-opus-5")).toBeTruthy();
  });

  it("marks no model current when the catalog names none", async () => {
    await open({ ...CATALOG, current: null }, rig());
    expect(screen.queryByText("Current")).toBeTruthy(); // the current mode only
    expect(button("Sonnet, current")).toBeNull();
    const { current: _drop, ...bare } = CATALOG;
    void _drop;
    await press("Close");
    await screen.unmount();
    await open(bare, rig());
    expect(button("Sonnet, current")).toBeNull();
  });

  it("sends ask, accept-edits and auto at once, with no biometrics", async () => {
    for (const [label, mode] of [["Accept edits", "accept-edits"], ["Auto", "auto"]] as const) {
      const r = rig();
      await open(CATALOG, r);
      await press(label);
      expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode }]);
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      await screen.unmount();
    }
    const r = rig();
    await open({ ...CATALOG, current_mode: "auto" }, r);
    await press("Ask");
    expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode: "ask" }]);
  });

  it("does nothing for the current mode or model", async () => {
    const r = rig();
    await open(CATALOG, r);
    await press("Ask, current");
    await press("Sonnet, current");
    expect(r.send).not.toHaveBeenCalled();
  });

  it("shows a pending state until the ack, then closes", async () => {
    const r = rig();
    await open(CATALOG, r);
    await press("Auto");
    expect(screen.getByText("Changing…")).toBeTruthy();
    await press("Accept edits"); // a second choice while one is out
    expect(r.send).toHaveBeenCalledTimes(1);
    await act(async () => r.waiting[0]?.resolve());
    expect(screen.queryByText("Ask")).toBeNull();
    expect(screen.queryByText("Changing…")).toBeNull();
  });

  it("shows a refusal in the hub's words, in the foreground colour, and stays open", async () => {
    const r = rig();
    await open(CATALOG, r);
    await press("Auto");
    await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish or cancel the current turn first")));
    const note = screen.getByText("Finish or cancel the current turn first");
    expect([dark.fg, light.fg]).toContain(StyleSheet.flatten(note.props.style).color);
    expect(screen.getByText("Ask")).toBeTruthy();
    expect(screen.queryByText("Changing…")).toBeNull();
    // Sending again clears it.
    await press("Auto");
    expect(screen.queryByText("Finish or cancel the current turn first")).toBeNull();
  });

  it("says a lost command may have reached the Mac", async () => {
    const r = rig();
    await open(CATALOG, r);
    await press("Auto");
    await act(async () => r.waiting[0]?.reject(new Refusal("lost", "connection dropped")));
    expect(screen.getByText(/Not sure this reached the Mac/)).toBeTruthy();
  });

  it("does not move the current mark itself: the catalog does", async () => {
    const r = rig();
    const view = await open(CATALOG, r);
    await press("Auto");
    await act(async () => r.waiting[0]?.resolve());
    await view.rerender(sheet({ ...CATALOG, current_mode: "auto" }, r.send));
    await press("Mode and model");
    expect(button("Auto, current")).toBeTruthy();
    expect(button("Ask, current")).toBeNull();
  });

  describe("models", () => {
    it("sends a same-engine model at once", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press("Opus");
      expect(r.sent).toEqual([{ type: "set_model", session: "s", selection: "opus" }]);
      expect(screen.queryByText(/Switches to/)).toBeNull();
    });

    it("warns about a provider switch and sends only after Switch", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press("GPT");
      expect(r.send).not.toHaveBeenCalled();
      expect(screen.getByText("Switches to Codex; the conversation carries over")).toBeTruthy();
      await press("Switch");
      expect(r.sent).toEqual([{ type: "set_model", session: "s", selection: "gpt" }]);
      expect(screen.getByText("Changing…")).toBeTruthy();
      await act(async () => r.waiting[0]?.resolve());
      expect(screen.queryByText("Ask")).toBeNull();
    });

    it("drops the warning when another model is chosen", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press("GPT");
      await press("Opus");
      expect(screen.queryByText(/Switches to/)).toBeNull();
      expect(r.sent).toEqual([{ type: "set_model", session: "s", selection: "opus" }]);
    });

    it("treats every model as the same engine when none is current", async () => {
      const r = rig();
      await open({ ...CATALOG, current: null }, r);
      await press("GPT");
      expect(screen.queryByText(/Switches to/)).toBeNull();
      expect(r.sent).toEqual([{ type: "set_model", session: "s", selection: "gpt" }]);
    });

    it("shows a refusal", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press("Opus");
      await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish or cancel the current turn first")));
      expect(screen.getByText("Finish or cancel the current turn first")).toBeTruthy();
    });
  });

  describe("full access", () => {
    const FULL = /^Full access$/;

    it("warns in amber on its row", async () => {
      await open(CATALOG, rig());
      const warning = screen.getByText("The agent can run any command without asking.");
      expect([dark.amber, light.amber]).toContain(StyleSheet.flatten(warning.props.style).color);
    });

    it("asks for biometrics, then the exact words, then sends with confirm", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
      expect(r.send).not.toHaveBeenCalled();
      const field = screen.getByLabelText("Type full access to confirm");
      const turnOn = () => screen.getByRole("button", { name: "Turn on full access" });
      expect(turnOn().props.accessibilityState.disabled).toBe(true);
      for (const wrong of ["Full access", "full access ", " full access", "full  access", "fullaccess", "full acces"]) {
        await fireEvent.changeText(field, wrong);
        expect(turnOn().props.accessibilityState.disabled).toBe(true);
      }
      await fireEvent.changeText(field, "full access");
      expect(turnOn().props.accessibilityState.disabled).toBe(false);
      await press("Turn on full access");
      expect(r.sent).toEqual([{ type: "set_mode", session: "s", mode: "full-access", confirm: "full access" }]);
      await act(async () => r.waiting[0]?.resolve());
      expect(screen.queryByText("Ask")).toBeNull();
    });

    it("asks for biometrics even when the Face ID setting is off", async () => {
      await saveSettings({ faceId: false });
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    });

    it("sends nothing and shows no field when biometrics fail", async () => {
      auth.authenticateAsync.mockResolvedValueOnce({ success: false, error: "user_cancel" });
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
      expect(screen.getByText("Ask")).toBeTruthy();
    });

    it("says it needs Face ID when there is none, and sends nothing", async () => {
      auth.isEnrolledAsync.mockResolvedValueOnce(false);
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      expect(screen.getByText("Full access needs Face ID set up on this phone.")).toBeTruthy();
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("sends nothing if the screen left while Face ID was up", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      const r = rig();
      const view = await open(CATALOG, r);
      await fireEvent.press(screen.getByRole("button", { name: FULL }));
      await flush();
      await view.unmount();
      await act(async () => finish({ success: true }));
      expect(r.send).not.toHaveBeenCalled();
    });

    it("does nothing for full access when it is the current mode", async () => {
      const r = rig();
      await open({ ...CATALOG, current_mode: "full-access" }, r);
      await press(/^Full access, current$/);
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("keeps the sheet and shows the refusal when the hub refuses", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      await fireEvent.changeText(screen.getByLabelText("Type full access to confirm"), "full access");
      await press("Turn on full access");
      await act(async () => r.waiting[0]?.reject(new Refusal("not_allowed", "Full access is off on this Mac")));
      expect(screen.getByText("Full access is off on this Mac")).toBeTruthy();
      expect(screen.getByLabelText("Type full access to confirm")).toBeTruthy();
    });

    it("can be cancelled before it is turned on", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      await press("Cancel full access");
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });
  });

  it("themes and sizes its controls", async () => {
    const r = rig();
    await open(CATALOG, r);
    const style = StyleSheet.flatten(screen.getByRole("button", { name: "Auto" }).props.style);
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
    expect([dark.fg, light.fg]).toContain(StyleSheet.flatten(screen.getByText("Auto").props.style).color);
  });
});
