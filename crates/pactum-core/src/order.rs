use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::{Eip712Domain, SolStruct, eip712_domain};

use crate::{SecretTree, secret::Secret};

sol! {
    /// Mirror of `PactumIntents.Order`. Field order defines the EIP-712 type.
    #[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Order {
        address maker;
        address receiver;
        address srcToken;
        uint256 srcAmount;
        uint256 dstChainId;
        address dstToken;
        uint256 startDstAmount;
        uint256 endDstAmount;
        uint64 auctionStart;
        uint64 auctionEnd;
        uint64 deadline;
        uint64 srcLock;
        uint16 parts;
        bytes32 secretsRoot;
        uint256 nonce;
        uint256 minDeposit;
    }
}

/// EIP-712 domain of a `PactumIntents` (inherited from `HashLock`) deployment.
pub fn domain(chain_id: u64, contract: Address) -> Eip712Domain {
    eip712_domain! {
        name: "HashLock",
        version: "1",
        chain_id: chain_id,
        verifying_contract: contract,
    }
}

impl Order {
    /// The digest the maker signs and the contract recovers (`hashOrder`).
    pub fn digest(&self, chain_id: u64, contract: Address) -> B256 {
        self.eip712_signing_hash(&domain(chain_id, contract))
    }
}

/// The maker's secrets for an order: one per fill bucket plus one for the final
/// fill, or a single secret when partial fills are disabled.
#[derive(Clone, Debug)]
pub struct OrderSecrets {
    pub secrets: Vec<Secret>,
    pub tree: SecretTree,
}

impl OrderSecrets {
    pub fn count_for(parts: u16) -> usize {
        if parts == 1 { 1 } else { parts as usize + 1 }
    }

    pub fn random(parts: u16) -> Self {
        Self::from_secrets((0..Self::count_for(parts)).map(|_| Secret::random()).collect())
    }

    pub fn from_secrets(secrets: Vec<Secret>) -> Self {
        let tree = SecretTree::new(secrets.iter().map(|s| s.hash).collect());
        Self { secrets, tree }
    }

    pub fn root(&self) -> B256 {
        self.tree.root()
    }

    pub fn hashes(&self) -> Vec<B256> {
        self.tree.hashes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typehash_matches_contract() {
        // keccak256 of the type string in PactumIntents.ORDER_TYPEHASH
        let expected = alloy::primitives::keccak256(
            "Order(address maker,address receiver,address srcToken,uint256 srcAmount,\
             uint256 dstChainId,address dstToken,uint256 startDstAmount,uint256 endDstAmount,\
             uint64 auctionStart,uint64 auctionEnd,uint64 deadline,uint64 srcLock,uint16 parts,\
             bytes32 secretsRoot,uint256 nonce,uint256 minDeposit)",
        );
        assert_eq!(Order::default().eip712_type_hash(), expected);
    }

    #[test]
    fn secret_counts() {
        assert_eq!(OrderSecrets::random(4).secrets.len(), 5);
        assert_eq!(OrderSecrets::random(1).secrets.len(), 1);
    }

    #[test]
    fn digest_is_domain_bound() {
        let o = Order::default();
        assert_ne!(o.digest(1, Address::ZERO), o.digest(10, Address::ZERO));
    }
}
