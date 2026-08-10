import { readFileSync } from "node:fs";
import type { Address } from "viem";
import type { ChainConfig } from "./chain.js";
import type { Route } from "./quote.js";

export interface Config {
  chains: ChainConfig[];
  routes: Route[];
}

interface RawRoute extends Omit<Route, "rateNum" | "rateDen" | "minSrc" | "maxSrc"> {
  rateNum: string;
  rateDen: string;
  minSrc: string;
  maxSrc: string;
}

/** Load a JSON config. Big numbers are strings so they survive JSON. */
export function loadConfig(path: string): Config {
  const raw = JSON.parse(readFileSync(path, "utf8")) as {
    chains: ChainConfig[];
    routes?: RawRoute[];
  };
  if (!raw.chains?.length) throw new Error(`${path}: no chains configured`);
  for (const c of raw.chains) {
    if (!c.id || !c.rpc || !c.contract) throw new Error(`${path}: incomplete chain ${c.name}`);
  }
  const routes = (raw.routes ?? []).map((r) => ({
    ...r,
    srcToken: r.srcToken as Address,
    dstToken: r.dstToken as Address,
    rateNum: BigInt(r.rateNum),
    rateDen: BigInt(r.rateDen),
    minSrc: BigInt(r.minSrc),
    maxSrc: BigInt(r.maxSrc),
  }));
  return { chains: raw.chains, routes };
}
