import { encodeAbiParameters, keccak256, type Hex } from "viem";

/**
 * Merkle tree over indexed secret hashes. Mirrors `MerkleSecrets.sol`:
 * leaf = keccak256(keccak256(abi.encode(index, secretHash))), sorted-pair
 * hashing, and the last node is paired with itself on odd levels.
 */
export function leaf(index: number, secretHash: Hex): Hex {
  return keccak256(
    keccak256(encodeAbiParameters([{ type: "uint256" }, { type: "bytes32" }], [BigInt(index), secretHash])),
  );
}

function hashPair(a: Hex, b: Hex): Hex {
  const [lo, hi] = BigInt(a) < BigInt(b) ? [a, b] : [b, a];
  return keccak256(encodeAbiParameters([{ type: "bytes32" }, { type: "bytes32" }], [lo, hi]));
}

export class SecretTree {
  private readonly levels: Hex[][];

  constructor(readonly hashes: Hex[]) {
    if (hashes.length === 0) throw new Error("empty tree");
    let level = hashes.map((h, i) => leaf(i, h));
    this.levels = [level];
    while (level.length > 1) {
      const next: Hex[] = [];
      for (let i = 0; i < level.length; i += 2) {
        const l = level[i]!;
        next.push(hashPair(l, level[i + 1] ?? l));
      }
      this.levels.push(next);
      level = next;
    }
  }

  get root(): Hex {
    return this.levels[this.levels.length - 1]![0]!;
  }

  proof(index: number): Hex[] {
    if (index < 0 || index >= this.hashes.length) throw new Error(`no leaf ${index}`);
    const out: Hex[] = [];
    let idx = index;
    for (const level of this.levels.slice(0, -1)) {
      out.push(level[idx ^ 1] ?? level[idx]!);
      idx >>= 1;
    }
    return out;
  }

  static verify(root: Hex, index: number, secretHash: Hex, proof: Hex[]): boolean {
    return proof.reduce((node, sib) => hashPair(node, sib), leaf(index, secretHash)) === root;
  }
}
