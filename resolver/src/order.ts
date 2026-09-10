import type { Account, Address, Hex, TypedDataDomain } from "viem";
import { hashTypedData } from "viem";
import { newSecret, type Secret } from "./secret.js";
import { SecretTree } from "./merkle.js";

/** Mirror of `PactumIntents.Order`. */
export interface Order {
  maker: Address;
  receiver: Address;
  srcToken: Address;
  srcAmount: bigint;
  dstChainId: bigint;
  dstToken: Address;
  startDstAmount: bigint;
  endDstAmount: bigint;
  auctionStart: bigint;
  auctionEnd: bigint;
  deadline: bigint;
  srcLock: bigint;
  parts: number;
  secretsRoot: Hex;
  nonce: bigint;
}

export const ORDER_TYPES = {
  Order: [
    { name: "maker", type: "address" },
    { name: "receiver", type: "address" },
    { name: "srcToken", type: "address" },
    { name: "srcAmount", type: "uint256" },
    { name: "dstChainId", type: "uint256" },
    { name: "dstToken", type: "address" },
    { name: "startDstAmount", type: "uint256" },
    { name: "endDstAmount", type: "uint256" },
    { name: "auctionStart", type: "uint64" },
    { name: "auctionEnd", type: "uint64" },
    { name: "deadline", type: "uint64" },
    { name: "srcLock", type: "uint64" },
    { name: "parts", type: "uint16" },
    { name: "secretsRoot", type: "bytes32" },
    { name: "nonce", type: "uint256" },
  ],
} as const;

export function domain(chainId: number, verifyingContract: Address): TypedDataDomain {
  return { name: "HashLock", version: "1", chainId, verifyingContract };
}

export function hashOrder(order: Order, chainId: number, contract: Address): Hex {
  return hashTypedData({
    domain: domain(chainId, contract),
    types: ORDER_TYPES,
    primaryType: "Order",
    message: order,
  });
}

export async function signOrder(
  account: Account,
  order: Order,
  chainId: number,
  contract: Address,
): Promise<Hex> {
  if (!account.signTypedData) throw new Error("account cannot sign typed data");
  return account.signTypedData({
    domain: domain(chainId, contract),
    types: ORDER_TYPES,
    primaryType: "Order",
    message: order,
  });
}

/** Secrets for a `parts`-way order: one per bucket, plus one for the final fill. */
export class OrderSecrets {
  readonly secrets: Secret[];
  readonly tree: SecretTree;

  constructor(readonly parts: number, secrets?: Secret[]) {
    const n = parts === 1 ? 1 : parts + 1;
    this.secrets = secrets ?? Array.from({ length: n }, newSecret);
    if (this.secrets.length !== n) throw new Error(`need ${n} secrets for ${parts} parts`);
    this.tree = new SecretTree(this.secrets.map((s) => s.secretHash));
  }

  get root(): Hex {
    return this.tree.root;
  }
}

// ----------------------------------------------------------- pricing / buckets

export function auctionPrice(o: Order, t: bigint): bigint {
  if (t <= o.auctionStart) return o.startDstAmount;
  if (t >= o.auctionEnd) return o.endDstAmount;
  const elapsed = t - o.auctionStart;
  const span = o.auctionEnd - o.auctionStart;
  return o.startDstAmount - ((o.startDstAmount - o.endDstAmount) * elapsed) / span;
}

/** Rounds up in the maker's favour, like the contract. */
export function owedDstAmount(o: Order, fillAmount: bigint, t: bigint): bigint {
  return (auctionPrice(o, t) * fillAmount + o.srcAmount - 1n) / o.srcAmount;
}

export function secretIndexFor(srcAmount: bigint, parts: number, filledAfter: bigint): number {
  if (parts === 1) {
    if (filledAfter !== srcAmount) throw new Error("single-part orders fill whole");
    return 0;
  }
  if (filledAfter === srcAmount) return parts;
  return Number((filledAfter * BigInt(parts) - 1n) / srcAmount);
}
