#!/usr/bin/env node
import { parseArgs } from "node:util";
import { privateKeyToAccount } from "viem/accounts";
import { parseUnits, type Address, type Hex } from "viem";
import { Chain } from "../chain.js";
import { loadConfig } from "../config.js";
import { swapAcross } from "../client.js";
import { NATIVE } from "../swap.js";

const { values } = parseArgs({
  options: {
    config: { type: "string", short: "c", default: "config.json" },
    from: { type: "string" },
    to: { type: "string" },
    resolver: { type: "string" },
    "src-token": { type: "string", default: NATIVE },
    "dst-token": { type: "string", default: NATIVE },
    amount: { type: "string" },
    "min-out": { type: "string" },
    decimals: { type: "string", default: "18" },
    "dst-decimals": { type: "string", default: "18" },
    lock: { type: "string", default: "86400" },
  },
});

for (const k of ["from", "to", "resolver", "amount", "min-out"] as const) {
  if (!values[k]) throw new Error(`--${k} is required`);
}

const key = process.env.USER_KEY as Hex | undefined;
if (!key) throw new Error("USER_KEY is not set");
const account = privateKeyToAccount(key);
const cfg = loadConfig(values.config!);

const pick = (name: string) => {
  const c = cfg.chains.find((c) => c.name === name || String(c.id) === name);
  if (!c) throw new Error(`unknown chain ${name}`);
  return new Chain(c, account);
};

const result = await swapAcross(
  pick(values.from!),
  pick(values.to!),
  {
    resolver: values.resolver as Address,
    srcToken: values["src-token"] as Address,
    srcAmount: parseUnits(values.amount!, Number(values.decimals)),
    dstToken: values["dst-token"] as Address,
    minDstAmount: parseUnits(values["min-out"]!, Number(values["dst-decimals"])),
    lockDuration: BigInt(values.lock!),
  },
  (m) => console.log(m),
);
console.log(JSON.stringify(result, (_, v) => (typeof v === "bigint" ? v.toString() : v), 2));
