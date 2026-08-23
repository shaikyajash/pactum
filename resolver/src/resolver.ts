import type { Address, Hex } from "viem";
import type { Chain, InitiatedEvent } from "./chain.js";
import { quote, type Route } from "./quote.js";
import { DEFAULT_POLICY, followerExpiry, type TimelockPolicy } from "./timelock.js";
import { matchesHash } from "./secret.js";
import { computeSwapId } from "./swap.js";

export type JobStatus = "filled" | "claimed" | "refunded" | "failed";

export interface Job {
  src: InitiatedEvent & { chainId: number };
  dst: {
    chainId: number;
    swapId: Hex;
    token: Address;
    amount: bigint;
    expiry: bigint;
    /** Block the fill landed in; lower bound when searching for the secret. */
    block: bigint;
  };
  status: JobStatus;
  error?: string;
}

export interface ResolverOptions {
  routes: Route[];
  policy?: TimelockPolicy;
  /** Blocks to wait before acting on an Initiated event. */
  confirmations?: bigint;
  log?: (msg: string) => void;
}

/**
 * Market-maker side of the swap.
 *
 *   1. Watch every chain for swaps locked *to us*.
 *   2. Price them against configured routes and mirror the lock on the
 *      destination chain with the same secret hash and a shorter timelock.
 *   3. When the user redeems on the destination chain, the secret becomes
 *      public. Pick it up and redeem our leg on the source chain.
 *   4. If the user never redeems, refund our leg once it expires.
 *
 * The resolver never holds the secret, so it can never take the user's funds
 * without delivering its own.
 */
export class Resolver {
  readonly jobs = new Map<Hex, Job>();
  private readonly cursors = new Map<number, bigint>();
  private readonly chains: Map<number, Chain>;
  private readonly policy: TimelockPolicy;
  private readonly confirmations: bigint;
  private readonly log: (msg: string) => void;

  constructor(chains: Chain[], private readonly opts: ResolverOptions) {
    this.chains = new Map(chains.map((c) => [c.id, c]));
    this.policy = opts.policy ?? DEFAULT_POLICY;
    this.confirmations = opts.confirmations ?? 0n;
    this.log = opts.log ?? (() => {});
  }

  get address(): Address {
    const first = this.chains.values().next().value;
    if (!first) throw new Error("no chains configured");
    return first.account.address;
  }

  /** Start scanning from the current head of each chain. */
  async init(): Promise<void> {
    for (const c of this.chains.values()) this.cursors.set(c.id, await c.head());
  }

  async tick(): Promise<void> {
    for (const chain of this.chains.values()) {
      const from = this.cursors.get(chain.id) ?? 0n;
      const to = (await chain.head()) - this.confirmations;
      if (to < from) continue;

      for (const ev of await chain.initiated(from, to, { redeemer: this.address })) {
        await this.fill(chain, ev).catch((e) => this.log(`fill ${ev.swapId} failed: ${e}`));
      }
      await this.settle(chain, from, to);
      this.cursors.set(chain.id, to + 1n);
    }
    await this.refundExpired();
  }

  private chain(id: number): Chain {
    const c = this.chains.get(id);
    if (!c) throw new Error(`unknown chain ${id}`);
    return c;
  }

  private async fill(src: Chain, ev: InitiatedEvent): Promise<void> {
    if (this.jobs.has(ev.swapId)) return;

    const q = quote(this.opts.routes, src.id, ev.token, ev.amount);
    if (!q.ok) return this.log(`skip ${ev.swapId}: ${q.reason}`);

    const dst = this.chain(q.route.dstChainId);
    const exp = followerExpiry(ev.expiry, await dst.now(), this.policy);
    if (!exp.ok) return this.log(`skip ${ev.swapId}: ${exp.reason}`);

    // the user's lock must still be live and match what the event claimed
    const onchain = await src.swap(ev.swapId);
    if (onchain.settled || onchain.amount !== ev.amount) {
      return this.log(`skip ${ev.swapId}: source swap no longer live`);
    }

    await dst.ensureAllowance(q.route.dstToken, q.dstAmount);
    // salt = source swap id, so each source lock maps to exactly one fill
    const params = {
      redeemer: ev.initiator,
      token: q.route.dstToken,
      amount: q.dstAmount,
      secretHash: ev.secretHash,
      expiry: exp.expiry,
      salt: ev.swapId,
    };
    const fillBlock = await dst.head();
    await dst.initiate(params);

    const dstSwapId = computeSwapId(dst.id, dst.contract, {
      ...params,
      initiator: dst.account.address,
    });

    this.jobs.set(ev.swapId, {
      src: { ...ev, chainId: src.id },
      dst: {
        chainId: dst.id,
        swapId: dstSwapId,
        token: params.token,
        amount: params.amount,
        expiry: params.expiry,
        block: fillBlock,
      },
      status: "filled",
    });
    this.log(`filled ${ev.swapId} on ${src.name} -> ${dstSwapId} on ${dst.name}`);
  }

  /** Look for secrets revealed on `chain` for jobs whose destination is `chain`. */
  private async settle(chain: Chain, from: bigint, to: bigint): Promise<void> {
    const pending = [...this.jobs.values()].filter(
      (j) => j.status === "filled" && j.dst.chainId === chain.id,
    );
    if (pending.length === 0) return;

    const secrets = await chain.revealed(
      from,
      to,
      pending.map((j) => j.dst.swapId),
    );
    for (const job of pending) {
      const secret = secrets.get(job.dst.swapId);
      if (secret) await this.claim(job, secret);
    }
  }

  private async claim(job: Job, secret: Hex): Promise<void> {
    if (!matchesHash(secret, job.src.secretHash)) {
      job.status = "failed";
      job.error = "revealed secret does not match";
      return;
    }
    try {
      await this.chain(job.src.chainId).redeem(job.src.swapId, secret);
      job.status = "claimed";
      this.log(`claimed ${job.src.swapId}`);
    } catch (e) {
      job.status = "failed";
      job.error = String(e);
      this.log(`claim ${job.src.swapId} failed: ${e}`);
    }
  }

  private async refundExpired(): Promise<void> {
    for (const job of this.jobs.values()) {
      if (job.status !== "filled") continue;
      const dst = this.chain(job.dst.chainId);
      if ((await dst.now()) < job.dst.expiry) continue;

      const s = await dst.swap(job.dst.swapId);
      if (s.settled) {
        // redeemed but the reveal slipped past our scan window: search from the fill
        const found = await dst.revealed(job.dst.block, await dst.head(), [job.dst.swapId]);
        const secret = found.get(job.dst.swapId);
        if (secret) await this.claim(job, secret);
        continue;
      }
      await dst.refund(job.dst.swapId);
      job.status = "refunded";
      this.log(`refunded ${job.dst.swapId} on ${dst.name}`);
    }
  }
}
