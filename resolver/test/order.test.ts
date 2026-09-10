import { describe, expect, it } from "vitest";
import { encodeAbiParameters, sha256, zeroAddress, type Hex } from "viem";
import { SecretTree } from "../src/merkle.js";
import {
  auctionPrice,
  OrderSecrets,
  owedDstAmount,
  secretIndexFor,
  type Order,
} from "../src/order.js";

const h = (i: number) => sha256(encodeAbiParameters([{ type: "uint256" }], [BigInt(i)]));

describe("SecretTree", () => {
  it("matches the Solidity tree builder", () => {
    // vector from TreeBuilder.rootAndProof(sha256(abi.encode(i)) for i < 5, target 4)
    const tree = new SecretTree([0, 1, 2, 3, 4].map(h));
    expect(tree.root).toBe("0xb0d3301e731129d289da59aefe64d69ae7770197cb269907bd7df1385a330b71");
    expect(tree.proof(4)).toEqual([
      "0xa0a59bae27a6d04030d96fcef0e9ad7d499d01c0d5b2f367fcd8c10e8c10675a",
      "0x98f593d59d90c9e4f525a1a2ab9ba97038ca6a4a7dccd7ec6edb50e87ee266f8",
      "0x7f5b348b4fc30bb77f54f446bae792a9997cb8a05eddbca1d0acd706cdb0d2c3",
    ]);
  });

  it.each([1, 2, 3, 7, 8, 33])("every leaf of a %i-leaf tree verifies", (n) => {
    const hashes = Array.from({ length: n }, (_, i) => h(i));
    const tree = new SecretTree(hashes);
    hashes.forEach((x, i) => expect(SecretTree.verify(tree.root, i, x, tree.proof(i))).toBe(true));
  });

  it("binds index to hash", () => {
    const tree = new SecretTree([h(0), h(1), h(2)]);
    expect(SecretTree.verify(tree.root, 0, h(1), tree.proof(1))).toBe(false);
  });
});

describe("OrderSecrets", () => {
  it("makes parts+1 secrets for split orders and 1 for whole orders", () => {
    expect(new OrderSecrets(4).secrets).toHaveLength(5);
    expect(new OrderSecrets(1).secrets).toHaveLength(1);
  });
});

describe("secretIndexFor", () => {
  it.each([
    [1n, 0],
    [25n, 0],
    [26n, 1],
    [50n, 1],
    [75n, 2],
    [99n, 3],
    [100n, 4],
  ])("filled %s/100 in 4 parts -> secret %i", (filled, idx) => {
    expect(secretIndexFor(100n, 4, filled)).toBe(idx);
  });

  it("rejects partial fills of single-part orders", () => {
    expect(() => secretIndexFor(100n, 1, 50n)).toThrow();
  });
});

describe("auction", () => {
  const o: Order = {
    maker: zeroAddress,
    receiver: zeroAddress,
    srcToken: zeroAddress,
    srcAmount: 100n,
    dstChainId: 1n,
    dstToken: zeroAddress,
    startDstAmount: 5000n,
    endDstAmount: 3000n,
    auctionStart: 1000n,
    auctionEnd: 1600n,
    deadline: 0n,
    srcLock: 0n,
    parts: 4,
    secretsRoot: `0x${"00".repeat(32)}` as Hex,
    nonce: 0n,
  };

  it("decays linearly and clamps", () => {
    expect(auctionPrice(o, 0n)).toBe(5000n);
    expect(auctionPrice(o, 1300n)).toBe(4000n);
    expect(auctionPrice(o, 9999n)).toBe(3000n);
  });

  it("charges pro-rata, rounding up", () => {
    expect(owedDstAmount(o, 50n, 1300n)).toBe(2000n);
    expect(owedDstAmount({ ...o, startDstAmount: 1n, endDstAmount: 1n }, 1n, 0n)).toBe(1n);
  });
});
