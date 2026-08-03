import { bytesToHex, sha256, type Hex } from "viem";
import { randomBytes } from "node:crypto";

export interface Secret {
  secret: Hex;
  secretHash: Hex;
}

/** Generate a fresh 32-byte secret and the sha256 hash that locks a swap. */
export function newSecret(): Secret {
  const secret = bytesToHex(randomBytes(32));
  return { secret, secretHash: sha256(secret) };
}

export function matchesHash(secret: Hex, secretHash: Hex): boolean {
  return sha256(secret).toLowerCase() === secretHash.toLowerCase();
}
