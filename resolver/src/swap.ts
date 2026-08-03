import { encodeAbiParameters, keccak256, zeroAddress, type Address, type Hex } from "viem";

export const NATIVE: Address = zeroAddress;

export interface SwapParams {
  initiator: Address;
  redeemer: Address;
  token: Address;
  amount: bigint;
  secretHash: Hex;
  expiry: bigint;
  salt: Hex;
}

/** Mirror of `HashLock.computeSwapId`. */
export function computeSwapId(chainId: number, contract: Address, p: SwapParams): Hex {
  return keccak256(
    encodeAbiParameters(
      [
        { type: "uint256" },
        { type: "address" },
        { type: "address" },
        { type: "address" },
        { type: "address" },
        { type: "uint256" },
        { type: "bytes32" },
        { type: "uint64" },
        { type: "bytes32" },
      ],
      [
        BigInt(chainId),
        contract,
        p.initiator,
        p.redeemer,
        p.token,
        p.amount,
        p.secretHash,
        p.expiry,
        p.salt,
      ],
    ),
  );
}
