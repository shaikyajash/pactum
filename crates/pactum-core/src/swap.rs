use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::sol_types::SolValue;

/// Mirror of `HashLock.computeSwapId`.
#[allow(clippy::too_many_arguments)]
pub fn swap_id(
    chain_id: u64,
    contract: Address,
    initiator: Address,
    redeemer: Address,
    token: Address,
    amount: U256,
    secret_hash: B256,
    expiry: u64,
    salt: B256,
) -> B256 {
    keccak256(
        (
            U256::from(chain_id),
            contract,
            initiator,
            redeemer,
            token,
            amount,
            secret_hash,
            U256::from(expiry),
            salt,
        )
            .abi_encode(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    #[test]
    fn matches_onchain_encoding() {
        // reference from `cast keccak $(cast abi-encode ...)`
        let id = swap_id(
            8453,
            address!("1111111111111111111111111111111111111111"),
            address!("00000000000000000000000000000000000000a1"),
            address!("00000000000000000000000000000000000000b0"),
            Address::ZERO,
            U256::from(10u64).pow(U256::from(18)),
            B256::repeat_byte(0xab),
            1_760_000_000,
            b256!("0000000000000000000000000000000000000000000000000000000000000007"),
        );
        assert_eq!(id, b256!("5ec8470890b17673838ff60c4359da8fbe8a7b1e642524b09ae58a12b38630c2"));
    }
}
