// For the tests, which share the text-size module: back to the default size with nothing read yet.
// Not imported by the app.
import { textSizeStore } from "./textSize";

export function resetTextSize(): void {
  textSizeStore.setState(textSizeStore.getInitialState(), true);
}
