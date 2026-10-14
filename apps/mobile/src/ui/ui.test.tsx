import { act, fireEvent, render, screen } from "@testing-library/react-native";
import { StyleSheet, Text } from "react-native";
import { SafeAreaProvider } from "react-native-safe-area-context";

import { dark, light, radius, type } from "../theme";
import { hideToast, showToast, TOAST_MS, toastStore } from "../toast";
import { BAR_HEIGHT, Button, Card, Chip, Label, Segmented, ToastHost as BareToastHost } from "./index";

const METRICS = { frame: { x: 0, y: 0, width: 390, height: 844 }, insets: { top: 47, left: 0, right: 0, bottom: 34 } };
/** The host as the app has it: inside the safe area. */
const ToastHost = () => (
  <SafeAreaProvider initialMetrics={METRICS}>
    <BareToastHost />
  </SafeAreaProvider>
);

const style = (node: { props: { style?: unknown } }) => StyleSheet.flatten(node.props.style as never) as Record<string, unknown>;

beforeEach(() => {
  jest.useFakeTimers();
  hideToast();
});
afterEach(() => {
  hideToast();
  jest.useRealTimers();
});

describe("Card", () => {
  it("has a 1 px edge border, a 12 px radius, and its title notched into the top border", async () => {
    await render(
      <Card title="Permission required" testID="card">
        <Text>inside</Text>
      </Card>,
    );
    const card = style(screen.getByTestId("card"));
    expect(card.borderWidth).toBe(1);
    expect(card.borderRadius).toBe(radius.card);
    expect(radius.card).toBe(12);
    expect([dark.edge, light.edge]).toContain(card.borderColor);
    const title = style(screen.getByText("Permission required"));
    // On the border: lifted half a line over the card's top edge, over the card's own background,
    // so the border looks cut where the title sits.
    expect(title.position).toBe("absolute");
    expect(Number(title.top)).toBeLessThan(0);
    expect(title.backgroundColor).toBe(card.backgroundColor);
    expect(screen.getByText("inside")).toBeTruthy();
  });

  it("an amber card has an amber border and title, for approvals", async () => {
    await render(
      <Card title="Permission required" tone="amber" testID="card">
        <Text>x</Text>
      </Card>,
    );
    expect([dark.amber, light.amber]).toContain(style(screen.getByTestId("card")).borderColor);
    expect([dark.amber, light.amber]).toContain(style(screen.getByText("Permission required")).color);
  });

  it("has no notch without a title", async () => {
    await render(
      <Card testID="card">
        <Text>x</Text>
      </Card>,
    );
    expect(screen.queryAllByText(/./)).toHaveLength(1);
  });
});

describe("Label", () => {
  it("is 12 / 16 monospace, upper-case and muted", async () => {
    await render(<Label>Tap to change</Label>);
    const s = style(screen.getByText("Tap to change"));
    expect(s).toMatchObject({ fontSize: 12, lineHeight: 16, fontFamily: "Menlo", textTransform: "uppercase" });
    expect([dark.muted, light.muted]).toContain(s.color);
  });
});

describe("type", () => {
  it("body is 14 / 20 and label 12 / 16, both monospace", () => {
    expect(type.body).toMatchObject({ fontSize: 14, lineHeight: 20, fontFamily: "Menlo" });
    expect(type.label).toMatchObject({ fontSize: 12, lineHeight: 16, fontFamily: "Menlo" });
  });
});

describe("Segmented", () => {
  const options = [
    { key: "needs", label: "Needs you 2" },
    { key: "running", label: "Running 1" },
    { key: "done", label: "Done" },
  ] as const;

  it("marks the selected option and reports a tap on another", async () => {
    const change = jest.fn();
    await render(<Segmented options={options} value="running" onChange={change} />);
    const running = screen.getByRole("tab", { name: "Running 1" });
    expect(running.props.accessibilityState).toMatchObject({ selected: true });
    expect(screen.getByRole("tab", { name: "Done" }).props.accessibilityState).toMatchObject({ selected: false });
    expect([dark.selected, light.selected]).toContain(style(running).backgroundColor);
    expect([dark.accent, light.accent]).toContain(style(screen.getByText("Running 1")).color);
    await fireEvent.press(screen.getByRole("tab", { name: "Done" }));
    expect(change).toHaveBeenCalledWith("done");
  });

  it("each option is at least 44 pt high", async () => {
    await render(<Segmented options={options} value="needs" onChange={jest.fn()} />);
    for (const tab of screen.getAllByRole("tab")) expect(Number(style(tab).minHeight)).toBeGreaterThanOrEqual(44);
  });
});

describe("Button", () => {
  it("has a hit area of at least 44 × 44 and a role and a label", async () => {
    const press = jest.fn();
    await render(<Button label="+" accessibilityLabel="New session" onPress={press} />);
    const button = screen.getByRole("button", { name: "New session" });
    expect(Number(style(button).minHeight)).toBeGreaterThanOrEqual(44);
    expect(Number(style(button).minWidth)).toBeGreaterThanOrEqual(44);
    await fireEvent.press(button);
    expect(press).toHaveBeenCalledTimes(1);
  });

  it("does nothing while disabled, and says so", async () => {
    const press = jest.fn();
    await render(<Button label="Allow once" onPress={press} disabled />);
    const button = screen.getByRole("button", { name: "Allow once" });
    expect(button.props.accessibilityState).toMatchObject({ disabled: true });
    await fireEvent.press(button);
    expect(press).not.toHaveBeenCalled();
  });

  it("an accent button has an accent border and text", async () => {
    await render(<Button label="Allow once" variant="accent" onPress={jest.fn()} />);
    expect([dark.accent, light.accent]).toContain(style(screen.getByRole("button", { name: "Allow once" })).borderColor);
    expect([dark.accent, light.accent]).toContain(style(screen.getByText("Allow once")).color);
  });
});

describe("Chip", () => {
  it("is a 44 pt button with its dot and label", async () => {
    const press = jest.fn();
    await render(<Chip label="studio-mac" dot="●" onPress={press} />);
    const chip = screen.getByRole("button", { name: "studio-mac" });
    expect(Number(style(chip).minHeight)).toBeGreaterThanOrEqual(44);
    expect(screen.getByText("●")).toBeTruthy();
    await fireEvent.press(chip);
    expect(press).toHaveBeenCalledTimes(1);
  });
});

describe("Toast", () => {
  it("shows the newest message and hides itself after 3.4 s", async () => {
    await render(<ToastHost />);
    expect(TOAST_MS).toBe(3_400);
    await act(async () => showToast("Write a prompt first."));
    expect(screen.getByText("Write a prompt first.")).toBeTruthy();
    await act(async () => void jest.advanceTimersByTime(3_399));
    expect(screen.getByText("Write a prompt first.")).toBeTruthy();
    await act(async () => void jest.advanceTimersByTime(1));
    expect(screen.queryByText("Write a prompt first.")).toBeNull();
    expect(toastStore.getState().toast).toBeNull();
  });

  it("a new message replaces the old and starts the clock again", async () => {
    await render(<ToastHost />);
    await act(async () => showToast("one"));
    await act(async () => void jest.advanceTimersByTime(3_000));
    await act(async () => showToast("two"));
    expect(screen.queryByText("one")).toBeNull();
    await act(async () => void jest.advanceTimersByTime(3_000));
    expect(screen.getByText("two")).toBeTruthy();
    await act(async () => void jest.advanceTimersByTime(400));
    expect(screen.queryByText("two")).toBeNull();
  });

  it("its action runs once and closes it", async () => {
    const view = jest.fn();
    await render(<ToastHost />);
    await act(async () => showToast("octet needs approval", view, "View"));
    const button = screen.getByRole("button", { name: "View" });
    expect(Number(style(button).minHeight)).toBeGreaterThanOrEqual(44);
    await fireEvent.press(button);
    expect(view).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("octet needs approval")).toBeNull();
  });

  it("sits above a screen's bottom button and the home indicator", async () => {
    await render(<ToastHost />);
    await act(async () => showToast("hello"));
    const wrap = style(screen.getByTestId("toast-wrap"));
    expect(Number(wrap.bottom)).toBeGreaterThanOrEqual(METRICS.insets.bottom + BAR_HEIGHT);
  });

  it("is a polite live region and draws the text as text", async () => {
    await render(<ToastHost />);
    await act(async () => showToast("<b>x</b> [y](http://evil)"));
    const text = screen.getByText("<b>x</b> [y](http://evil)");
    expect(text).toBeTruthy();
    expect(screen.getByTestId("toast").props.accessibilityLiveRegion).toBe("polite");
  });
});
