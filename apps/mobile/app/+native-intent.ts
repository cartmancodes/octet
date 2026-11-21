// Links the system opens into the app, before the router reads them.
//
// An approve link from the Island's Allow (`octet://approve/...`) is the shell's alone
// (app/_layout.tsx): it opens the Inbox, and the approval's session over it only for a known Mac, a
// live session and a waiting approval. The router is given nothing for it (null: it stays where it
// is, or starts on the Inbox), so no navigation of its own lands after the shell's. The link itself
// never allows anything.
//
// A pairing link (`octet://pair/...`, from the phone's camera, a tap, or a paste into the browser)
// opens Pair with the link filled in, in its add mode (a phone with no Mac shows the first
// pairing's screen anyway). The link only fills the field: pairing waits for the user's tap, and
// then for the owner's answer at the Mac. Every other path is the router's, as it was.
import { handOverApproveLink, isApproveUrl } from "../src/navigation";

/** Whether `url` is meant as a pairing link (well formed or not: Pair says why one is not). */
const isPairUrl = (url: string): boolean => /^octet:\/\/pair\//i.test(url);

export function redirectSystemPath({ path }: { path: string; initial: boolean }): string | null {
  if (isPairUrl(path)) return `/pair?add=1&link=${encodeURIComponent(path)}`;
  if (!isApproveUrl(path)) return path;
  handOverApproveLink(path);
  return null;
}
