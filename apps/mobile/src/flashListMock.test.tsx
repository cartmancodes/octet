// jest.setup.ts mocks a private FlashList module (its `measureLayout`), because FlashList 2
// measures its parent and rows through native layout, which a test renderer cannot do. Nothing
// public pins that path, so a FlashList upgrade can move it. These tests fail by name when it has:
// the path no longer resolves, a measure is gone, or FlashList no longer measures through it (the
// mock would then do nothing, and every list test would quietly draw no rows).
import { FlashList } from "@shopify/flash-list";
import { act, render, screen } from "@testing-library/react-native";
import { Text } from "react-native";

/** Keep in step with the `jest.mock` in jest.setup.ts. */
const MEASURE_LAYOUT = "@shopify/flash-list/dist/recyclerview/utils/measureLayout";
const MEASURES = ["measureParentSize", "measureFirstChildLayout", "measureItemLayout"] as const;

type Measures = Record<(typeof MEASURES)[number], jest.Mock>;

function resolves(path: string): boolean {
  try {
    require.resolve(path);
    return true;
  } catch {
    return false;
  }
}

describe("the FlashList layout mock in jest.setup.ts", () => {
  it("names a module that still resolves", () => {
    if (!resolves(MEASURE_LAYOUT)) {
      throw new Error(
        `${MEASURE_LAYOUT} no longer resolves: FlashList moved its layout measuring. Point the mock in jest.setup.ts at the new module.`,
      );
    }
  });

  it("replaces measures the real module still has", () => {
    const real = jest.requireActual<Record<string, unknown>>(MEASURE_LAYOUT);
    const missing = MEASURES.filter((name) => typeof real[name] !== "function");
    expect(missing).toEqual([]);
  });

  it("is what FlashList measures with: its rows draw", async () => {
    const mocked = jest.requireMock<Measures>(MEASURE_LAYOUT);
    mocked.measureParentSize.mockClear();
    await render(<FlashList data={["one", "two", "three"]} renderItem={({ item }) => <Text>{item}</Text>} />);
    // FlashList's load timer, inside act.
    await act(async () => void (await new Promise((resolve) => setTimeout(resolve, 5))));
    expect(mocked.measureParentSize).toHaveBeenCalled();
    expect(screen.getByText("three")).toBeTruthy();
  });
});
