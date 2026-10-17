// The app's Fleet, given to screens by `HubContext` (app/_layout.tsx), and the hooks that read it.
import { createContext, useContext, useMemo, useSyncExternalStore } from "react";

import type { HubClient } from "../client/HubClient";
import type { Links } from "../macs";
import { Fleet, type FleetSnapshot, type HubStatus } from "./fleet";
import type { MacInfo } from "./macLink";

/** The app's Macs. */
export const hub = new Fleet();

export const HubContext = createContext<Fleet>(hub);

export const useHubController = (): Fleet => useContext(HubContext);

/** One part of the snapshot. `pick` must return a part as it is (each keeps its identity while
 * it is the same) or a primitive: the hook renders again only when that changes. */
function useFleetPart<T>(pick: (snapshot: FleetSnapshot) => T): T {
  const fleet = useHubController();
  const get = () => pick(fleet.snapshot());
  return useSyncExternalStore(fleet.subscribe, get, get);
}

export const useFleetStatus = (): HubStatus => useFleetPart((s) => s.status);
export const useOffer = (): boolean => useFleetPart((s) => s.offer);
/** The paired Macs; the same array until one of them changes. */
export const useMacs = (): MacInfo[] => useFleetPart((s) => s.macs);
export const useNotice = (): string | null => useFleetPart((s) => s.notice);
/** Each Mac's push notice; the same array until one changes. */
export const useNotices = (): string[] => useFleetPart((s) => s.notices);

/** One paired Mac; the same object until that Mac changes (another Mac's change does not render
 * it again). Null for a Mac not paired. */
export const useMac = (id: string): MacInfo | null => useFleetPart((s) => s.macs.find((m) => m.id === id) ?? null);

/** Whether Mac `mac` is paired (with a link or not); false for null. Renders again only when
 * that changes. */
export const useIsPaired = (mac: string | null): boolean => useFleetPart((s) => mac !== null && s.macs.some((m) => m.id === mac));

/** The paired Macs' names by id, in pairing order: the same map until a Mac is paired, unpaired
 * or renamed (a link's state changing does not make a new one). */
export function useMacNames(): ReadonlyMap<string, string> {
  const key = useFleetPart((s) => s.macs.map((m) => `${m.id}\t${m.macName}`).join("\n"));
  return useMemo(() => {
    const names = new Map<string, string>();
    for (const line of key === "" ? [] : key.split("\n")) {
      const at = line.indexOf("\t");
      names.set(line.slice(0, at), line.slice(at + 1));
    }
    return names;
  }, [key]);
}

/** The connection by Mac, for code that holds a session key: the Fleet's `links`, one object
 * that resolves each Mac at the call, so nothing that holds it renders again as Macs come or go. */
export const useLinks = (): Links => useHubController().links;

/** The connection to Mac `mac`; null for none, and for a Mac with no link. Renders again only when
 * that Mac's connection changes (it is paired again, or goes). */
export function useClient(mac: string | null): HubClient | null {
  const fleet = useHubController();
  const get = () => (mac === null ? null : fleet.links.client(mac));
  return useSyncExternalStore(fleet.subscribe, get, get);
}
