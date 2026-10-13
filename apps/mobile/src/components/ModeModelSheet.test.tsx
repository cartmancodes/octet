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
    {
      engine: "claude",
      selection: "sonnet",
      name: "Sonnet",
      id: "claude-sonnet-5",
    },
    { engine: "claude", selection: "opus", name: "Opus", id: "claude-opus-5" },
    { engine: "codex", selection: "gpt", name: "GPT", id: "gpt-6" },
    {
      engine: "claude",
      selection: "haiku",
      name: "Haiku",
      id: "claude-haiku-5",
    },
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
  (measureParentSize as jest.Mock).mockReturnValue({
    x: 0,
    y: 0,
    width: 400,
    height: 5000,
  });
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
    for (const [label, mode] of [
      ["Accept edits", "accept-edits"],
      ["Auto", "auto"],
    ] as const) {
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
    expect(screen.getAllByText("Changing…").length).toBeGreaterThan(0);
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
      expect(screen.getAllByText("Changing…").length).toBeGreaterThan(0);
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
      expect(r.sent).toEqual([
        {
          type: "set_mode",
          session: "s",
          mode: "full-access",
          confirm: "full access",
        },
      ]);
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
      auth.authenticateAsync.mockResolvedValueOnce({
        success: false,
        error: "user_cancel",
      });
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
      expect(screen.getByText("Ask")).toBeTruthy(); // the sheet stays
      // The panel is disarmed: the next try asks for Face ID again.
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      await press(FULL);
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(2);
      expect(screen.getByLabelText("Type full access to confirm").props.value).toBe("");
    });

    it("gives one prompt for a double tap on the row", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      const r = rig();
      await open(CATALOG, r);
      const row = screen.getByRole("button", { name: FULL });
      await fireEvent.press(row);
      await fireEvent.press(row);
      await act(async () => finish({ success: true }));
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    });

    it("says it needs Face ID when the phone has no biometric hardware", async () => {
      auth.hasHardwareAsync.mockResolvedValueOnce(false);
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      expect(screen.getByText("Full access needs Face ID set up on this phone.")).toBeTruthy();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("focuses the field", async () => {
      await open(CATALOG, rig());
      await press(FULL);
      expect(screen.getByLabelText("Type full access to confirm").props.autoFocus).toBe(true);
    });

    it("opens nothing if the sheet was closed and reopened while Face ID was up", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      const r = rig();
      await open(CATALOG, r);
      await fireEvent.press(screen.getByRole("button", { name: FULL }));
      await flush();
      await act(async () => screen.getByTestId("sheet").props.onRequestClose()); // Android back
      await press("Mode and model");
      await settle();
      await act(async () => finish({ success: true }));
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("disarms the panel when the desk changes the mode: the next Turn on needs a fresh Face ID", async () => {
      const r = rig();
      const view = await open(CATALOG, r);
      await press(FULL);
      expect(screen.getByLabelText("Type full access to confirm")).toBeTruthy();
      // The desk flips to auto, then to full access, then back to ask.
      await view.rerender(sheet({ ...CATALOG, current_mode: "auto" }, r.send));
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      await view.rerender(sheet({ ...CATALOG, current_mode: "full-access" }, r.send));
      await view.rerender(sheet({ ...CATALOG, current_mode: "ask" }, r.send));
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(screen.queryByRole("button", { name: "Turn on full access" })).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
      // Choosing it again asks for Face ID again.
      auth.authenticateAsync.mockClear();
      await press(FULL);
      expect(auth.authenticateAsync).toHaveBeenCalledTimes(1);
    });

    it("arms nothing when the desk changes the mode while Face ID is up", async () => {
      let finish: (r: LocalAuthentication.LocalAuthenticationResult) => void = () => {};
      auth.authenticateAsync.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      const r = rig();
      const view = await open(CATALOG, r);
      await fireEvent.press(screen.getByRole("button", { name: FULL }));
      await flush();
      await view.rerender(sheet({ ...CATALOG, current_mode: "auto" }, r.send));
      await act(async () => finish({ success: true }));
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      expect(r.send).not.toHaveBeenCalled();
    });

    it("a sent full access is spent: a second Turn on cannot go without a fresh Face ID", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press(FULL);
      await fireEvent.changeText(screen.getByLabelText("Type full access to confirm"), "full access");
      await press("Turn on full access");
      expect(screen.queryByLabelText("Type full access to confirm")).toBeNull();
      await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish the turn first")));
      expect(screen.queryByRole("button", { name: "Turn on full access" })).toBeNull();
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

  it("closes on Android back and on the scrim when idle", async () => {
    await open(CATALOG, rig());
    await act(async () => screen.getByTestId("sheet").props.onRequestClose());
    expect(screen.queryByText("Ask")).toBeNull();
    await press("Mode and model");
    await fireEvent.press(screen.getByTestId("scrim"));
    expect(screen.queryByText("Ask")).toBeNull();
  });

  describe("while a change is out", () => {
    it("cannot be closed, shows the refusal when it comes, and the ack closes it", async () => {
      const r = rig();
      await open(CATALOG, r);
      await press("Auto");
      const closer = screen.getByRole("button", { name: "Changing…" });
      expect(closer.props.accessibilityState.disabled).toBe(true);
      await fireEvent.press(closer);
      await fireEvent.press(screen.getByTestId("scrim"));
      await act(async () => screen.getByTestId("sheet").props.onRequestClose());
      expect(screen.getByText("Ask")).toBeTruthy();
      await act(async () => r.waiting[0]?.reject(new Refusal("busy", "Finish or cancel the current turn first")));
      expect(screen.getByText("Finish or cancel the current turn first")).toBeTruthy();
      expect(button("Close")).toBeTruthy();
      // Try again; this time the ack closes the sheet.
      await press("Auto");
      await act(async () => screen.getByTestId("sheet").props.onRequestClose());
      expect(screen.getByText("Ask")).toBeTruthy();
      await act(async () => r.waiting[1]?.resolve());
      expect(screen.queryByText("Ask")).toBeNull();
    });
  });

  it("lists a duplicated mode or model once", async () => {
    const dup: CatalogInfo = {
      ...CATALOG,
      modes: ["ask", "auto", "ask"],
      models: [
        ...CATALOG.models,
        {
          engine: "claude",
          selection: "sonnet",
          name: "Sonnet again",
          id: "x",
        },
      ],
    };
    await open(dup, rig());
    expect(screen.getAllByRole("button", { name: /^Ask/ })).toHaveLength(1);
    expect(screen.getAllByRole("button", { name: /^Sonnet/ })).toHaveLength(1);
    expect(screen.queryByText("Sonnet again")).toBeNull();
  });

  it("clips a long model name in its label", async () => {
    const name = "M".repeat(300);
    await open(
      {
        ...CATALOG,
        models: [{ engine: "claude", selection: "long", name, id: "long" }],
      },
      rig(),
    );
    const row = screen.getByRole("button", { name: /^MMM/ });
    expect(row.props.accessibilityLabel.length).toBeLessThanOrEqual(130);
  });

  it("themes and sizes its controls", async () => {
    const r = rig();
    await open(CATALOG, r);
    const style = StyleSheet.flatten(screen.getByRole("button", { name: "Auto" }).props.style);
    expect(style.minHeight).toBeGreaterThanOrEqual(44);
    expect([dark.fg, light.fg]).toContain(StyleSheet.flatten(screen.getByText("Auto").props.style).color);
  });

  describe("offline", () => {
    const offline = (send: ReturnType<typeof rig>["send"], off = true) => (
      <ModeModelSheet session="s" catalog={CATALOG} send={send} offline={off} />
    );

    it("the Mode and model button is disabled", async () => {
      const r = rig();
      await render(offline(r.send));
      expect(button("Mode and model")?.props.accessibilityState?.disabled).toBe(true);
      await fireEvent.press(screen.getByRole("button", { name: "Mode and model" }));
      expect(button("Auto")).toBeNull();
    });

    it("an open sheet's choices send nothing once the link drops", async () => {
      const r = rig();
      const view = await render(offline(r.send, false));
      await press("Mode and model");
      await settle();
      await view.rerender(offline(r.send));
      await settle();
      expect(button("Auto")?.props.accessibilityState?.disabled).toBe(true);
      await fireEvent.press(screen.getByRole("button", { name: "Auto" }));
      await fireEvent.press(screen.getByRole("button", { name: /^Full access$/ }));
      await flush();
      expect(auth.authenticateAsync).not.toHaveBeenCalled();
      expect(r.send).not.toHaveBeenCalled();
      // Close still works.
      await press("Close");
      expect(button("Auto")).toBeNull();
      await settle();
    });

    it("full access, armed before the drop, cannot be turned on while offline", async () => {
      const r = rig();
      const view = await render(offline(r.send, false));
      await press("Mode and model");
      await settle();
      await press(/^Full access$/);
      await fireEvent.changeText(screen.getByLabelText("Type full access to confirm"), "full access");
      await view.rerender(offline(r.send));
      await settle();
      const turnOn = screen.getByRole("button", { name: "Turn on full access" });
      expect(turnOn.props.accessibilityState.disabled).toBe(true);
      await fireEvent.press(turnOn);
      expect(r.send).not.toHaveBeenCalled();
      await settle();
    });
  });

  describe("an approval waiting", () => {
    const NOTE = "An approval is waiting \u2014 close this to answer it.";
    const withApproval = (send: ReturnType<typeof rig>["send"], waiting: boolean) => (
      <ModeModelSheet session="s" catalog={CATALOG} send={send} approval={waiting} />
    );

    it("says so in amber at the top of the open sheet, as soon as one comes", async () => {
      const r = rig();
      const view = await render(withApproval(r.send, false));
      await press("Mode and model");
      await settle();
      expect(screen.queryByText(NOTE)).toBeNull();
      await view.rerender(withApproval(r.send, true));
      await settle();
      const note = screen.getByText(NOTE);
      expect([dark.amber, light.amber]).toContain(StyleSheet.flatten(note.props.style).color);
      // It goes when the approval is answered elsewhere.
      await view.rerender(withApproval(r.send, false));
      await settle();
      expect(screen.queryByText(NOTE)).toBeNull();
    });

    it("is not shown while the sheet is closed", async () => {
      await render(withApproval(rig().send, true));
      expect(screen.queryByText(NOTE)).toBeNull();
    });
  });
});
