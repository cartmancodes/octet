// The app's links to its Macs: up to `MAX_MACS` pairings, each a `MacLink` (its client, its device
// token in the keychain, its push registration and its notice), and the `Fleet` that owns them:
// where the pairings come from, how pairing and unpairing keep them, and what the app's lifecycle
// does to every link. No JSX here; `app/_layout.tsx` provides the Fleet to screens.
//
// The parts are in src/fleet/: the keychain (`keychain.ts`), one link (`macLink.ts`), the Fleet
// (`fleet.ts`) and the hooks (`hooks.ts`). This module names them all, as it always has.
export { TOKEN_KEY, tokenKey, tokenKeyOf, type Keychain, type TokenStore } from "./fleet/keychain";
export { MacLink, type MacInfo, type PushResult } from "./fleet/macLink";
export {
  Fleet,
  NOT_A_LINK,
  UNPAIR_FAILED_NOTICE,
  unpairedNotice,
  type FleetOptions,
  type FleetSnapshot,
  type HubStatus,
  type PairResult,
} from "./fleet/fleet";
export {
  HubContext,
  hub,
  useClient,
  useFleetStatus,
  useHubController,
  useIsPaired,
  useLinks,
  useMac,
  useMacNames,
  useMacs,
  useNotice,
  useNotices,
  useOffer,
} from "./fleet/hooks";
