//! EIP-2612 permits, so a maker's token approval is a signature rather than a
//! transaction. Paired with `PactumIntents.fillWithPermit`.

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::{SolStruct, eip712_domain};
use serde::{Deserialize, Serialize};

sol! {
    /// EIP-2612 `Permit` typed-data struct.
    #[derive(Debug, Default)]
    struct Permit {
        address owner;
        address spender;
        uint256 value;
        uint256 nonce;
        uint256 deadline;
    }
}

/// Token-specific EIP-712 domain. Most tokens use version "1"; Circle's USDC uses "2".
#[derive(Clone, Debug)]
pub struct TokenDomain {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    pub token: Address,
}

impl Permit {
    pub fn digest(&self, d: &TokenDomain) -> B256 {
        let domain = eip712_domain! {
            name: d.name.clone(),
            version: d.version.clone(),
            chain_id: d.chain_id,
            verifying_contract: d.token,
        };
        self.eip712_signing_hash(&domain)
    }
}

/// A signed permit as carried in a posted order and passed to `fillWithPermit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPermit {
    pub value: U256,
    pub deadline: U256,
    pub v: u8,
    pub r: B256,
    pub s: B256,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typehash_matches_eip2612() {
        let expected = alloy::primitives::keccak256(
            "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
        );
        assert_eq!(Permit::default().eip712_type_hash(), expected);
    }
}
