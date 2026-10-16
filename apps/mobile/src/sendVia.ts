// Which Mac a command goes to: a session's key names its Mac, and only the hub's raw id goes on
// the wire. Plain TypeScript (no React, no storage), so the contract test drives it against real
// hubs; `src/components/send.ts` holds the hooks over it.
import { Refusal, type Ack, type RequestBody } from "./client/HubClient";
import { splitKey, type Links } from "./macs";

export const UNREACHABLE = "Can't reach your Mac";
export const CHOOSE_MAC = "Choose a Mac";

/**
 * Sends `body` to the Mac it is for, through `links`, with the hub's raw id in place of the key
 * in its `session`. The Mac is the one of the body's `session` (a key), else of `key`; a body
 * that names neither goes to the only Mac. Never sent, refused as `unsent`: a session that is not
 * a key, a body for another Mac than `key`'s, no Mac connected, or no Mac named while there are
 * several.
 */
export function sendVia(links: Links, key: string | undefined, body: RequestBody): Promise<Ack> {
  const unsent = (words: string) => Promise.reject(new Refusal("unsent", words));
  const named = "session" in body ? body.session : undefined;
  const target = named ?? key;
  let mac: string;
  if (target === undefined) {
    const macs = links.macs();
    if (macs.length > 1) return unsent(CHOOSE_MAC);
    const only = macs[0];
    if (only === undefined) return unsent(UNREACHABLE);
    mac = only;
  } else {
    const split = splitKey(target);
    if (split === null) return unsent(UNREACHABLE);
    if (key !== undefined && named !== undefined && splitKey(key)?.mac !== split.mac) return unsent(UNREACHABLE);
    mac = split.mac;
  }
  const client = links.client(mac);
  if (client === null) return unsent(UNREACHABLE);
  if (named === undefined) return client.request(body);
  const raw = splitKey(named)?.raw ?? "";
  return client.request({ ...body, session: raw } as RequestBody);
}

/**
 * `sendVia` for Mac `mac`, chosen by the user (New session's chip, a Recent row): a body that
 * names no session (`start`, `recent`) goes to that Mac, and one that names a session goes to
 * the session's Mac, which must be `mac`. Refused as `unsent`: a session of another Mac, or a Mac
 * with no connection. With no `mac`, `sendVia` with no key.
 */
export function sendTo(links: Links, mac: string | undefined, body: RequestBody): Promise<Ack> {
  if (mac === undefined) return sendVia(links, undefined, body);
  if ("session" in body) {
    if (splitKey(body.session)?.mac !== mac) return Promise.reject(new Refusal("unsent", UNREACHABLE));
    return sendVia(links, body.session, body);
  }
  const client = links.client(mac);
  if (client === null) return Promise.reject(new Refusal("unsent", UNREACHABLE));
  return client.request(body);
}
