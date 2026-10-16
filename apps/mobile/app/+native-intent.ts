// Links the system opens into the app, before the router reads them. An approve link from the
// Island's Allow (`octet://approve/...`) is the shell's alone (app/_layout.tsx): it opens the Inbox,
// and the approval's session over it only for a known Mac, a live session and a waiting approval.
// The router is given nothing for it (null: it stays where it is, or starts on the Inbox), so no
// navigation of its own lands after the shell's. The link itself never allows anything. Every
// other path is the router's, as it was.
import { handOverApproveLink, isApproveUrl } from "../src/navigation";

export function redirectSystemPath({ path }: { path: string; initial: boolean }): string | null {
  if (!isApproveUrl(path)) return path;
  handOverApproveLink(path);
  return null;
}
