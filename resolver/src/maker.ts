import type { Address, Hex } from "viem";
import type { Chain } from "./chain.js";
import { RelayClient } from "./relay.js";
import { hashOrder, OrderSecrets, signOrder, type Order } from "./order.js";
import { DEFAULT_POLICY, type TimelockPolicy } from "./timelock.js";

export interface IntentRequest {
  srcToken: Address;
  srcAmount: bigint;
  dstToken: Address;
  /** Opening ask for the whole order; decays to `endDstAmount`. */
  startDstAmount: bigint;
  endDstAmount: bigint;
  auctionDuration: bigint;
  /** How long the order may be filled. */
  ttl: bigint;
  /** Lock on each source escrow. */
  srcLock: bigint;
  parts: number;
  receiver?: Address;
}

export interface OpenIntent {
  order: Order;
  signature: Hex;
  hash: Hex;
  secrets: OrderSecrets;
}

/**
 * Maker side of an intent. Signs once, then for every fill checks that the
 * resolver locked enough on the destination chain before releasing that
 * fill's secret. The maker sends no transactions.
 */
export class Maker {
  readonly released = new Set<number>();
  private cursor = 0n;
  private dstCursor = 0n;

  constructor(
    readonly src: Chain,
    readonly dst: Chain,
    readonly relay: RelayClient,
    readonly policy: TimelockPolicy = DEFAULT_POLICY,
    private readonly log: (m: string) => void = () => {},
  ) {}

  async open(req: IntentRequest, nonce: bigint = BigInt(Date.now())): Promise<OpenIntent> {
    const now = await this.src.now();
    const secrets = new OrderSecrets(req.parts);
    const order: Order = {
      maker: this.src.account.address,
      receiver: req.receiver ?? this.src.account.address,
      srcToken: req.srcToken,
      srcAmount: req.srcAmount,
      dstChainId: BigInt(this.dst.id),
      dstToken: req.dstToken,
      startDstAmount: req.startDstAmount,
      endDstAmount: req.endDstAmount,
      auctionStart: now,
      auctionEnd: now + req.auctionDuration,
      deadline: now + req.ttl,
      srcLock: req.srcLock,
      parts: req.parts,
      secretsRoot: secrets.root,
      nonce,
    };
    const signature = await signOrder(this.src.account, order, this.src.id, this.src.contract);
    const hash = await this.relay.post({
      order,
      signature,
      srcChainId: this.src.id,
      contract: this.src.contract,
      secretHashes: secrets.secrets.map((x) => x.secretHash),
    });
    if (hash !== hashOrder(order, this.src.id, this.src.contract)) throw new Error("relay hash mismatch");
    this.cursor = await this.src.head();
    this.dstCursor = await this.dst.head();
    this.log(`opened intent ${hash}`);
    return { order, signature, hash, secrets };
  }

  /** Release secrets for fills whose destination escrow checks out. */
  async tick(intent: OpenIntent): Promise<void> {
    const head = await this.src.head();
    const dstHead = await this.dst.head();
    const fills = await this.src.filledEvents(this.cursor, head, intent.hash);
    const dstLocks = await this.dst.initiated(this.dstCursor, dstHead, {
      redeemer: intent.order.receiver,
    });

    for (const f of fills) {
      const idx = Number(f.secretIndex);
      if (this.released.has(idx)) continue;
      const s = intent.secrets.secrets[idx];
      if (!s) continue;
      const srcEscrow = await this.src.swap(f.swapId);

      const lock = dstLocks.find(
        (l) =>
          l.initiator.toLowerCase() === f.resolver.toLowerCase() &&
          l.token.toLowerCase() === intent.order.dstToken.toLowerCase() &&
          l.secretHash === s.secretHash &&
          l.amount >= f.owedDstAmount &&
          // we must be able to redeem on dst well before the resolver can refund
          // and before our own source escrow is refundable
          l.expiry + this.policy.safetyWindow <= srcEscrow.expiry,
      );
      if (!lock) continue; // not (yet) a valid counter-lock: keep the secret

      const live = await this.dst.swap(lock.swapId);
      if (live.settled) continue;

      await this.relay.release(intent.hash, idx, s.secret, s.secretHash, intent.secrets.tree.proof(idx));
      this.released.add(idx);
      this.log(`released secret ${idx} for ${lock.swapId}`);
    }
  }
}
