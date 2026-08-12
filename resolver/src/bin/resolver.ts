#!/usr/bin/env node
import { parseArgs } from "node:util";
import { privateKeyToAccount } from "viem/accounts";
import type { Hex } from "viem";
import { Chain } from "../chain.js";
import { loadConfig } from "../config.js";
import { Resolver } from "../resolver.js";

const { values } = parseArgs({
  options: {
    config: { type: "string", short: "c", default: "config.json" },
    interval: { type: "string", default: "2000" },
    confirmations: { type: "string", default: "0" },
  },
});

const key = process.env.RESOLVER_KEY as Hex | undefined;
if (!key) throw new Error("RESOLVER_KEY is not set");

const account = privateKeyToAccount(key);
const cfg = loadConfig(values.config!);
const chains = cfg.chains.map((c) => new Chain(c, account));
const log = (m: string) => console.log(`${new Date().toISOString()} ${m}`);

const resolver = new Resolver(chains, {
  routes: cfg.routes,
  confirmations: BigInt(values.confirmations!),
  log,
});
await resolver.init();
log(`resolver ${account.address} watching ${chains.map((c) => c.name).join(", ")}`);

let stopping = false;
process.on("SIGINT", () => (stopping = true));
process.on("SIGTERM", () => (stopping = true));

while (!stopping) {
  await resolver.tick().catch((e) => log(`tick error: ${e}`));
  await new Promise((r) => setTimeout(r, Number(values.interval)));
}
log("stopped");
