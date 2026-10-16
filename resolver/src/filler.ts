import type { Hex } from "viem";
import type { Chain } from "./chain.js";
import type { RelayClient, SignedOrder } from "./relay.js";
import { owedDstAmount, secretIndexFor, type Order } from "./order.js";
import { SecretTree } from "./merkle.js";
import { computeSwapId } from "./swap.js";
import { DEFAULT_POLICY, followerExpiry, type TimelockPolicy } from "./timelock.js";

export interface FillerOptions {
  /** Most destination tokens we will pay for `srcAmount` of this order, or undefined to skip. */
  bid: (order: Order, srcAmount: bigint) => bigint | undefined;
  /** Largest slice to take per fill; defaults to the whole remainder. */
  maxFill?: (order: Order) => bigint;
  policy?: TimelockPolicy;
  log?: (m: string) => void;
}

export interface IntentJob {
  orderHash: Hex;
  index: number;
  secretHash: Hex;
  srcChain: number;
  srcSwapId: Hex;
  dstChain: number;
  dstSwapId: Hex;
  dstExpiry: bigint;
  status: "locked" | "settled" | "refunded";
}

/**
 * Resolver for gasless intents. Waits for the Dutch auction to reach a price
 * it is willing to pay, fills (possibly a slice), mirrors the lock on the
 * destination chain, then completes both legs once the maker releases the
 * secret: redeeming on dst pays the maker, redeeming on src pays us.
 */
export class IntentFiller {
  readonly jobs: IntentJob[] = [];
  private readonly policy: TimelockPolicy;
  private readonly log: (m: string) => void;

  constructor(
    private readonly chains: Map<number, Chain>,
    private readonly relay: RelayClient,
    private readonly opts: FillerOptions,
  ) {
    this.policy = opts.policy ?? DEFAULT_POLICY;
    this.log = opts.log ?? (() => {});
  }

  async tick(): Promise<void> {
    for (const o of await this.relay.orders()) {
      await this.tryFill(o).catch((e) => this.log(`fill ${o.hash} skipped: ${e.shortMessage ?? e}`));
    }
    await this.settle();
  }

  private async tryFill(o: SignedOrder & { hash: Hex }): Promise<void> {
    const src = this.chains.get(o.srcChainId);
    const dst = this.chains.get(Number(o.order.dstChainId));
    if (!src || !dst) return;

    const now = await src.now();
    if (now > o.order.deadline) return;
    const { filled, nextIndex } = await src.orderProgress(o.hash);
    const remaining = o.order.srcAmount - filled;
    if (remaining === 0n) return;

    let amount = this.opts.maxFill ? min(this.opts.maxFill(o.order), remaining) : remaining;
    let index = secretIndexFor(o.order.srcAmount, o.order.parts, filled + amount);
    // a slice that stays inside an already-used bucket would reuse a public
    // secret; the contract rejects it, so take the rest instead
    if (index < nextIndex || (o.order.parts === 1 && amount !== remaining)) {
      amount = remaining;
      index = secretIndexFor(o.order.srcAmount, o.order.parts, filled + amount);
    }

    const bid = this.opts.bid(o.order, amount);
    // small buffer for the block that lands the fill
    if (bid === undefined || owedDstAmount(o.order, amount, now + 15n) > bid) return;

    const hashes = await this.hashesFor(o, index);
    if (!hashes) return;

    const fill = await src.fillOrder(o.order, o.signature, amount, index, hashes.secretHash, hashes.proof);
    const exp = followerExpiry(fill.expiry, await dst.now(), this.policy);
    if (!exp.ok) throw new Error(exp.reason); // src escrow will be refunded to the maker

    await dst.ensureAllowance(o.order.dstToken, fill.owedDstAmount);
    const params = {
      redeemer: o.order.receiver,
      token: o.order.dstToken,
      amount: fill.owedDstAmount,
      secretHash: hashes.secretHash,
      expiry: exp.expiry,
      salt: fill.swapId,
    };
    await dst.initiate(params);
    const dstSwapId = computeSwapId(dst.id, dst.contract, { ...params, initiator: dst.account.address });

    this.jobs.push({
      orderHash: o.hash,
      index,
      secretHash: hashes.secretHash,
      srcChain: src.id,
      srcSwapId: fill.swapId,
      dstChain: dst.id,
      dstSwapId,
      dstExpiry: exp.expiry,
      status: "locked",
    });
    this.log(`filled ${amount} of ${o.hash} (secret ${index}), owe ${fill.owedDstAmount}`);
  }

  private async hashesFor(o: SignedOrder & { hash: Hex }, index: number) {
    const secretHash = o.secretHashes[index];
    if (!secretHash) return undefined;
    const tree = new SecretTree(o.secretHashes);
    if (tree.root !== o.order.secretsRoot) return undefined;
    return { secretHash, proof: tree.proof(index) };
  }

  private async settle(): Promise<void> {
    for (const job of this.jobs) {
      if (job.status !== "locked") continue;
      const src = this.chains.get(job.srcChain)!;
      const dst = this.chains.get(job.dstChain)!;
      const secret = await this.relay.secret(job.orderHash, job.index);

      if (secret) {
        // pay the maker first (gasless for them), then ourselves
        if (!(await dst.swap(job.dstSwapId)).settled) await dst.redeem(job.dstSwapId, secret);
        if (!(await src.swap(job.srcSwapId)).settled) await src.redeem(job.srcSwapId, secret);
        job.status = "settled";
        this.log(`settled ${job.srcSwapId}`);
      } else if ((await dst.now()) >= job.dstExpiry && !(await dst.swap(job.dstSwapId)).settled) {
        await dst.refund(job.dstSwapId);
        job.status = "refunded";
      }
    }
  }
}

const min = (a: bigint, b: bigint) => (a < b ? a : b);
