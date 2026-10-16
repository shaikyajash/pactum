import {
  createPublicClient,
  createWalletClient,
  defineChain,
  erc20Abi,
  http,
  maxUint256,
  type Account,
  type Address,
  type Hex,
  type PublicClient,
  type WalletClient,
} from "viem";
import { parseEventLogs } from "viem";
import { hashLockAbi, pactumIntentsAbi } from "./abi.js";
import type { Order } from "./order.js";
import { NATIVE, type SwapParams } from "./swap.js";

export interface ChainConfig {
  id: number;
  name: string;
  rpc: string;
  contract: Address;
}

export interface InitiatedEvent {
  swapId: Hex;
  initiator: Address;
  redeemer: Address;
  token: Address;
  amount: bigint;
  secretHash: Hex;
  expiry: bigint;
  blockNumber: bigint;
}

export interface OnchainSwap {
  initiator: Address;
  expiry: bigint;
  settled: boolean;
  redeemer: Address;
  token: Address;
  amount: bigint;
  secretHash: Hex;
}

/** Thin, typed wrapper around one HashLock deployment. */
export class Chain {
  readonly id: number;
  readonly name: string;
  readonly contract: Address;
  readonly public: PublicClient;
  readonly wallet: WalletClient;

  constructor(cfg: ChainConfig, readonly account: Account) {
    this.id = cfg.id;
    this.name = cfg.name;
    this.contract = cfg.contract;
    const chain = defineChain({
      id: cfg.id,
      name: cfg.name,
      nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
      rpcUrls: { default: { http: [cfg.rpc] } },
    });
    this.public = createPublicClient({ chain, transport: http(cfg.rpc) });
    this.wallet = createWalletClient({ chain, account, transport: http(cfg.rpc) });
  }

  async now(): Promise<bigint> {
    return (await this.public.getBlock()).timestamp;
  }

  async head(): Promise<bigint> {
    return this.public.getBlockNumber();
  }

  async swap(swapId: Hex): Promise<OnchainSwap> {
    const [initiator, expiry, settled, redeemer, token, amount, secretHash] =
      await this.public.readContract({
        address: this.contract,
        abi: hashLockAbi,
        functionName: "swaps",
        args: [swapId],
      });
    return { initiator, expiry, settled, redeemer, token, amount, secretHash };
  }

  async initiate(p: Omit<SwapParams, "initiator">): Promise<Hex> {
    const hash = await this.wallet.writeContract({
      chain: this.wallet.chain,
      account: this.account,
      address: this.contract,
      abi: hashLockAbi,
      functionName: "initiate",
      args: [p.redeemer, p.token, p.amount, p.secretHash, p.expiry, p.salt],
      value: p.token === NATIVE ? p.amount : 0n,
    });
    return this.confirm(hash);
  }

  async redeem(swapId: Hex, secret: Hex): Promise<Hex> {
    const hash = await this.wallet.writeContract({
      chain: this.wallet.chain,
      account: this.account,
      address: this.contract,
      abi: hashLockAbi,
      functionName: "redeem",
      args: [swapId, secret],
    });
    return this.confirm(hash);
  }

  async refund(swapId: Hex): Promise<Hex> {
    const hash = await this.wallet.writeContract({
      chain: this.wallet.chain,
      account: this.account,
      address: this.contract,
      abi: hashLockAbi,
      functionName: "refund",
      args: [swapId],
    });
    return this.confirm(hash);
  }

  /** Ensure the contract can pull `token` from our account. */
  async ensureAllowance(token: Address, amount: bigint): Promise<void> {
    if (token === NATIVE) return;
    const current = await this.public.readContract({
      address: token,
      abi: erc20Abi,
      functionName: "allowance",
      args: [this.account.address, this.contract],
    });
    if (current >= amount) return;
    const hash = await this.wallet.writeContract({
      chain: this.wallet.chain,
      account: this.account,
      address: token,
      abi: erc20Abi,
      functionName: "approve",
      args: [this.contract, maxUint256],
    });
    await this.confirm(hash);
  }

  async initiated(
    fromBlock: bigint,
    toBlock: bigint,
    filter: { redeemer?: Address; initiator?: Address } = {},
  ): Promise<InitiatedEvent[]> {
    const logs = await this.public.getContractEvents({
      address: this.contract,
      abi: hashLockAbi,
      eventName: "Initiated",
      args: filter,
      fromBlock,
      toBlock,
      strict: true,
    });
    return logs.map((l) => ({ ...l.args, blockNumber: l.blockNumber }));
  }

  /** Secrets revealed for the given swaps in the block range. */
  async revealed(fromBlock: bigint, toBlock: bigint, swapIds: Hex[]): Promise<Map<Hex, Hex>> {
    const out = new Map<Hex, Hex>();
    if (swapIds.length === 0) return out;
    const logs = await this.public.getContractEvents({
      address: this.contract,
      abi: hashLockAbi,
      eventName: "Redeemed",
      args: { swapId: swapIds },
      fromBlock,
      toBlock,
      strict: true,
    });
    for (const l of logs) out.set(l.args.swapId, l.args.secret);
    return out;
  }

  /** Fill a signed intent; returns the opened escrow and what the maker is owed. */
  async fillOrder(
    order: Order,
    signature: Hex,
    fillAmount: bigint,
    secretIndex: number,
    secretHash: Hex,
    proof: Hex[],
  ): Promise<{ swapId: Hex; owedDstAmount: bigint; expiry: bigint }> {
    const hash = await this.wallet.writeContract({
      chain: this.wallet.chain,
      account: this.account,
      address: this.contract,
      abi: pactumIntentsAbi,
      functionName: "fill",
      args: [order, signature, fillAmount, BigInt(secretIndex), secretHash, proof],
    });
    const receipt = await this.public.waitForTransactionReceipt({ hash });
    if (receipt.status !== "success") throw new Error(`${this.name}: fill ${hash} reverted`);
    const [filled] = parseEventLogs({
      abi: pactumIntentsAbi,
      eventName: "OrderFilled",
      logs: receipt.logs,
    });
    if (!filled) throw new Error(`${this.name}: no OrderFilled in ${hash}`);
    const s = await this.swap(filled.args.swapId);
    return { swapId: filled.args.swapId, owedDstAmount: filled.args.owedDstAmount, expiry: s.expiry };
  }

  async filledEvents(fromBlock: bigint, toBlock: bigint, orderHash: Hex) {
    const logs = await this.public.getContractEvents({
      address: this.contract,
      abi: pactumIntentsAbi,
      eventName: "OrderFilled",
      args: { orderHash },
      fromBlock,
      toBlock,
      strict: true,
    });
    return logs.map((l) => l.args);
  }

  async orderProgress(orderHash: Hex): Promise<{ filled: bigint; nextIndex: number }> {
    const [filled, nextIndex] = await this.public.readContract({
      address: this.contract,
      abi: pactumIntentsAbi,
      functionName: "fills",
      args: [orderHash],
    });
    return { filled, nextIndex };
  }

  private async confirm(hash: Hex): Promise<Hex> {
    const r = await this.public.waitForTransactionReceipt({ hash });
    if (r.status !== "success") throw new Error(`${this.name}: tx ${hash} reverted`);
    return hash;
  }
}
