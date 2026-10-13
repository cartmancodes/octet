import { render } from "@testing-library/react-native";

import { Mascot } from "./Mascot";
import { POSE_NAMES } from "./poses";

describe("Mascot", () => {
  it.each(POSE_NAMES)("renders the %s pose", async (pose) => {
    const view = await render(<Mascot pose={pose} size={96} />);
    expect(view.toJSON()).not.toBeNull();
  });
});
