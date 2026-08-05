import { describe, expect, it } from "vitest";
import { quote, type Route } from "../src/quote.js";
import { NATIVE } from "../src/swap.js";

const USDC = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const routes: Route[] = [
  {
    srcChainId: 1,
    srcToken: NATIVE,
    dstChainId: 8453,
    dstToken: USDC,
    rateNum: 3000n,
    rateDen: 1n,
    minSrc: 10n ** 16n,
    maxSrc: 10n ** 19n,
  },
];

describe("quote", () => {
  it("prices a supported route", () => {
    const q = quote(routes, 1, NATIVE, 10n ** 18n);
    expect(q.ok && q.dstAmount).toBe(3000n * 10n ** 18n);
  });

  it("is case-insensitive on token address", () => {
    const r = [{ ...routes[0]!, srcChainId: 8453, srcToken: USDC as `0x${string}` }];
    expect(quote(r, 8453, USDC.toLowerCase() as `0x${string}`, 10n ** 18n).ok).toBe(true);
  });

  it.each([
    [1, 1n, "amount below minimum"],
    [1, 10n ** 20n, "amount above maximum"],
    [10, 10n ** 18n, "no route for source asset"],
  ])("rejects chain %i amount %s", (chain, amount, reason) => {
    expect(quote(routes, chain, NATIVE, amount)).toEqual({ ok: false, reason });
  });
});
