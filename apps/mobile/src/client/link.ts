/** The pairing link the Mac shows: `https://<host>/#pair=<64 hex digits>`. Returns the hub's
 * WebSocket URL and the secret, or null for anything else. Parsed by hand: React Native's `URL`
 * is partial, and a strict shape is the point. */

const LABEL = "[a-z0-9](?:[a-z0-9-]*[a-z0-9])?";
const LINK = new RegExp(`^https://(${LABEL}(?:\\.${LABEL})*)(?::(\\d{1,5}))?/?#pair=([0-9a-f]{64})$`, "i");

export function parsePairLink(text: string): { url: string; secret: string } | null {
  const match = LINK.exec(text.trim());
  if (!match) return null;
  const [, host, port, secret] = match;
  if (host === undefined || secret === undefined) return null;
  if (port !== undefined) {
    const number = Number(port);
    if (number < 1 || number > 65535) return null;
  }
  const authority = port === undefined ? host : `${host}:${Number(port)}`;
  return { url: `wss://${authority.toLowerCase()}/`, secret: secret.toLowerCase() };
}
