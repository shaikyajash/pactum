import type { Address } from "viem";

/** A one-directional route the resolver is willing to fill. */
export interface Route {
  srcChainId: number;
  srcToken: Address;
  dstChainId: number;
  dstToken: Address;
  /** dstAmount = srcAmount * rateNum / rateDen */
  rateNum: bigint;
  rateDen: bigint;
  minSrc: bigint;
  maxSrc: bigint;
}

export type Quote =
  | { ok: true; route: Route; dstAmount: bigint }
  | { ok: false; reason: string };

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

export function findRoute(routes: Route[], srcChainId: number, srcToken: Address) {
  return routes.find((r) => r.srcChainId === srcChainId && eq(r.srcToken, srcToken));
}

export function quote(
  routes: Route[],
  srcChainId: number,
  srcToken: Address,
  srcAmount: bigint,
): Quote {
  const route = findRoute(routes, srcChainId, srcToken);
  if (!route) return { ok: false, reason: "no route for source asset" };
  if (srcAmount < route.minSrc) return { ok: false, reason: "amount below minimum" };
  if (srcAmount > route.maxSrc) return { ok: false, reason: "amount above maximum" };

  const dstAmount = (srcAmount * route.rateNum) / route.rateDen;
  if (dstAmount === 0n) return { ok: false, reason: "quote rounds to zero" };
  return { ok: true, route, dstAmount };
}
