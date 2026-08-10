import { toHex, type Address, type Hex } from "viem";
import { randomBytes } from "node:crypto";
import type { Chain, InitiatedEvent } from "./chain.js";
import { newSecret } from "./secret.js";
import { computeSwapId } from "./swap.js";

export interface SwapRequest {
  resolver: Address;
  srcToken: Address;
  srcAmount: bigint;
  dstToken: Address;
  /** Smallest destination amount we will accept from the resolver. */
  minDstAmount: bigint;
  /** How long our lock on the source chain lasts, in seconds. */
  lockDuration: bigint;
  pollMs?: number;
  /** Give up waiting for the resolver after this long. */
  timeoutMs?: number;
}

export interface SwapResult {
  srcSwapId: Hex;
  dstSwapId: Hex;
  dstAmount: bigint;
  redeemTx: Hex;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Taker side of the swap: lock on `src`, wait for the resolver to mirror the
 * lock on `dst`, verify it, then redeem — which reveals the secret the
 * resolver needs to claim our source funds.
 */
export async function swapAcross(
  src: Chain,
  dst: Chain,
  req: SwapRequest,
  log: (msg: string) => void = () => {},
): Promise<SwapResult> {
  const { secret, secretHash } = newSecret();
  const me = src.account.address;
  const expiry = (await src.now()) + req.lockDuration;
  const salt = toHex(randomBytes(32));

  await src.ensureAllowance(req.srcToken, req.srcAmount);
  const dstFrom = await dst.head();
  const params = {
    redeemer: req.resolver,
    token: req.srcToken,
    amount: req.srcAmount,
    secretHash,
    expiry,
    salt,
  };
  await src.initiate(params);
  const srcSwapId = computeSwapId(src.id, src.contract, { ...params, initiator: me });
  log(`locked on ${src.name}: ${srcSwapId}`);

  const fill = await waitForFill(dst, dstFrom, req, me, secretHash);
  if (!fill) {
    throw new Error(
      `resolver did not fill in time; refund ${srcSwapId} on ${src.name} after ${expiry}`,
    );
  }
  log(`resolver filled on ${dst.name}: ${fill.swapId} (${fill.amount})`);

  const redeemTx = await dst.redeem(fill.swapId, secret);
  log(`redeemed on ${dst.name}: ${redeemTx}`);
  return { srcSwapId, dstSwapId: fill.swapId, dstAmount: fill.amount, redeemTx };
}

/** A fill is only acceptable if it pays us enough of the right token. */
export function acceptableFill(
  ev: InitiatedEvent,
  req: Pick<SwapRequest, "resolver" | "dstToken" | "minDstAmount">,
  me: Address,
  secretHash: Hex,
): boolean {
  const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();
  return (
    eq(ev.initiator, req.resolver) &&
    eq(ev.redeemer, me) &&
    eq(ev.token, req.dstToken) &&
    eq(ev.secretHash, secretHash) &&
    ev.amount >= req.minDstAmount
  );
}

async function waitForFill(
  dst: Chain,
  from: bigint,
  req: SwapRequest,
  me: Address,
  secretHash: Hex,
): Promise<InitiatedEvent | undefined> {
  const deadline = Date.now() + (req.timeoutMs ?? 10 * 60_000);
  let cursor = from;
  while (Date.now() < deadline) {
    const head = await dst.head();
    if (head >= cursor) {
      const events = await dst.initiated(cursor, head, { redeemer: me });
      const ok = events.find((e) => acceptableFill(e, req, me, secretHash));
      if (ok) return ok;
      cursor = head + 1n;
    }
    await sleep(req.pollMs ?? 2_000);
  }
  return undefined;
}
