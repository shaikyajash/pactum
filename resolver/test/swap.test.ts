import { describe, expect, it } from "vitest";
import { computeSwapId, NATIVE } from "../src/swap.js";
import { matchesHash, newSecret } from "../src/secret.js";

describe("computeSwapId", () => {
  it("matches the on-chain encoding", () => {
    // reference value from:
    //   cast keccak $(cast abi-encode "f(uint256,address,address,address,address,uint256,bytes32,uint64,bytes32)" ...)
    const id = computeSwapId(8453, "0x1111111111111111111111111111111111111111", {
      initiator: "0x00000000000000000000000000000000000000a1",
      redeemer: "0x00000000000000000000000000000000000000b0",
      token: NATIVE,
      amount: 10n ** 18n,
      secretHash: `0x${"ab".repeat(32)}`,
      expiry: 1_760_000_000n,
      salt: `0x${"00".repeat(31)}07`,
    });
    expect(id).toBe("0x5ec8470890b17673838ff60c4359da8fbe8a7b1e642524b09ae58a12b38630c2");
  });

  it("differs across chains", () => {
    const p = {
      initiator: NATIVE,
      redeemer: NATIVE,
      token: NATIVE,
      amount: 1n,
      secretHash: `0x${"00".repeat(32)}`,
      expiry: 1n,
      salt: `0x${"00".repeat(32)}`,
    } as const;
    expect(computeSwapId(1, NATIVE, p)).not.toBe(computeSwapId(10, NATIVE, p));
  });
});

describe("secret", () => {
  it("generates a 32-byte secret whose hash verifies", () => {
    const { secret, secretHash } = newSecret();
    expect(secret).toMatch(/^0x[0-9a-f]{64}$/);
    expect(matchesHash(secret, secretHash)).toBe(true);
    expect(matchesHash(newSecret().secret, secretHash)).toBe(false);
  });

  it("uses sha256 like the contract", () => {
    // sha256 of 32 zero bytes
    expect(
      matchesHash(
        `0x${"00".repeat(32)}`,
        "0x66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925",
      ),
    ).toBe(true);
  });
});

import { acceptableFill } from "../src/client.js";

describe("acceptableFill", () => {
  const me = "0x00000000000000000000000000000000000000a1" as const;
  const resolver = "0x00000000000000000000000000000000000000b0" as const;
  const token = "0x00000000000000000000000000000000000000c0" as const;
  const secretHash = `0x${"11".repeat(32)}` as const;
  const ev = {
    swapId: `0x${"22".repeat(32)}`,
    initiator: resolver,
    redeemer: me,
    token,
    amount: 100n,
    secretHash,
    expiry: 0n,
    blockNumber: 0n,
  } as const;
  const req = { resolver, dstToken: token, minDstAmount: 100n };

  it("accepts a matching fill", () => {
    expect(acceptableFill(ev, req, me, secretHash)).toBe(true);
  });

  it.each([
    ["short amount", { amount: 99n }],
    ["wrong token", { token: resolver }],
    ["wrong hash", { secretHash: `0x${"33".repeat(32)}` }],
    ["not from resolver", { initiator: me }],
  ])("rejects %s", (_, patch) => {
    expect(acceptableFill({ ...ev, ...patch } as typeof ev, req, me, secretHash)).toBe(false);
  });
});
