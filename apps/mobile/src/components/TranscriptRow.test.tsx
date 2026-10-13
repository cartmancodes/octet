import { fireEvent, render, screen } from "@testing-library/react-native";

import type { Entry } from "../model/transcript";
import { TranscriptRow } from "./TranscriptRow";

const noop = () => undefined;

describe("a tools row", () => {
  const entry: Entry = {
    kind: "tools",
    key: "1",
    calls: [{ id: "1", name: "Read", target: "a.rs", phase: "done", detail: "" }],
    over: false,
    more: 4_500,
  };

  it("says how many calls only the Mac holds, collapsed and open", async () => {
    const view = await render(<TranscriptRow entry={entry} open={false} onToggle={noop} />);
    expect(screen.getByText("4500 more calls on the Mac")).toBeTruthy();
    expect(screen.getByRole("button").props.accessibilityLabel).toMatch(/4500 more calls on the Mac/);
    await view.rerender(<TranscriptRow entry={entry} open onToggle={noop} />);
    expect(screen.getByText("4500 more calls on the Mac")).toBeTruthy();
    await fireEvent.press(screen.getByRole("button"));
  });

  it("says nothing of the kind when it holds every call", async () => {
    await render(<TranscriptRow entry={{ ...entry, more: 0 }} open={false} onToggle={noop} />);
    expect(screen.queryByText(/more calls on the Mac/)).toBeNull();
  });
});
