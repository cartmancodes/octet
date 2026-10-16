import { PROTOCOL_VERSION, type ToPhone } from "../protocol";

/** The largest frame taken, in bytes of UTF-8; a larger one parses to null. The hub keeps its
 * `sessions`, `changes` and `diff` frames within `MAX_MESSAGE` (96 KiB, protocol.ts), well under
 * this. */
export const MAX_FRAME_BYTES = 256 * 1024;

type Obj = Record<string, unknown>;

const isObj = (x: unknown): x is Obj => typeof x === "object" && x !== null && !Array.isArray(x);
const isStr = (x: unknown): x is string => typeof x === "string";
const isUint = (x: unknown): x is number => typeof x === "number" && Number.isSafeInteger(x) && x >= 0;

/** UTF-8 length of `text`, counted without allocating a copy. */
export function utf8Length(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    if (unit < 0x80) bytes += 1;
    else if (unit < 0x800) bytes += 2;
    else if (unit >= 0xd800 && unit <= 0xdbff) {
      bytes += 4; // a surrogate pair
      i += 1;
    } else bytes += 3;
  }
  return bytes;
}

/**
 * Parses one text frame from the hub, or returns null for anything not worth acting on: too
 * large, not JSON, a wrong or missing `v`, an unknown `type`, or fields of the wrong shape.
 * Only the fields the client reads are checked; the payloads inside `sessions`, `state` and
 * `catalog` are the hub's, and a consumer reads them as untrusted.
 */
export function parseFrame(text: string): ToPhone | null {
  // A UTF-8 byte is at most three UTF-16 units, so short texts need no count.
  if (text.length > MAX_FRAME_BYTES) return null;
  if (text.length * 3 > MAX_FRAME_BYTES && utf8Length(text) > MAX_FRAME_BYTES) return null;

  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObj(value) || value.v !== PROTOCOL_VERSION || !isStr(value.type)) return null;

  const ok = ((): boolean => {
    switch (value.type) {
      case "welcome":
        return isStr(value.device);
      case "paired":
        return isStr(value.token) && isStr(value.device);
      case "sessions":
        return Array.isArray(value.sessions);
      case "subscribed":
        return (
          isStr(value.session) &&
          (value.from === "cursor" || value.from === "start") &&
          Array.isArray(value.journals) &&
          value.journals.every(isStr)
        );
      case "event":
        return (
          isStr(value.session) &&
          isStr(value.journal) &&
          isUint(value.seq) &&
          isObj(value.event) &&
          isStr(value.event.type)
        );
      case "state":
        return isStr(value.session) && isObj(value.state);
      case "catalog":
        return isStr(value.session) && isObj(value.catalog);
      case "recent":
        return isUint(value.req) && Array.isArray(value.items);
      // Their strings and numbers are cleaned before the request resolves (answers.ts).
      case "changes":
        return isUint(value.req) && Array.isArray(value.files);
      case "diff":
        return isUint(value.req) && isStr(value.path) && isStr(value.text);
      case "ack":
        return isUint(value.req) && isStr(value.message);
      case "refused":
        return isUint(value.req) && isStr(value.reason);
      default:
        return false;
    }
  })();
  // The shape was checked field by field above.
  return ok ? (value as unknown as ToPhone) : null;
}
