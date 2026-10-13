/** The pairing link the Mac shows: `https://<host>/#pair=<64 hex digits>`. Returns the hub's
 * WebSocket URL and the secret, or null for anything else. Parsed by hand: React Native's `URL`
 * is partial, and a strict shape is the point. */

const LABEL = "[a-z0-9](?:[a-z0-9-]*[a-z0-9])?";
const LINK = new RegExp(`^https://(${LABEL}(?:\\.${LABEL})*)(?::(\\d{1,5}))?/?#pair=([0-9a-f]{64})$`, "i");

const HUB_URL = new RegExp(`^wss://(${LABEL}(?:\\.${LABEL})*)(?::(\\d{1,5}))?/$`, "i");

/** `host` or `host:port` in the form the hub URL carries it, or null for a port out of range. */
function authorityOf(host: string, port: string | undefined): string | null {
  if (port === undefined) return host.toLowerCase();
  const number = Number(port);
  if (number < 1 || number > 65535) return null;
  return `${host}:${number}`.toLowerCase();
}

export function parsePairLink(text: string): { url: string; secret: string } | null {
  const match = LINK.exec(text.trim());
  if (!match) return null;
  const [, host, port, secret] = match;
  if (host === undefined || secret === undefined) return null;
  const authority = authorityOf(host, port);
  if (authority === null) return null;
  return { url: `wss://${authority}/`, secret: secret.toLowerCase() };
}

/** Whether `url` is a hub address this app would have saved: the pairing link's host and port
 * rules, `wss://`, and nothing after the slash. A stored value that fails is not trusted. */
export function isHubUrl(url: string): boolean {
  const match = HUB_URL.exec(url);
  if (!match) return false;
  const [, host, port] = match;
  if (host === undefined) return false;
  const authority = authorityOf(host, port);
  return authority !== null && url === `wss://${authority}/`;
}

/** The host (and port) of a hub URL for showing to the user. */
export function hubHost(url: string): string {
  return url.replace(/^wss:\/\//, "").replace(/\/$/, "");
}
