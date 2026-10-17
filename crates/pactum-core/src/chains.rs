//! Chain identities for `Order.dstChainId`, plus the 32-byte address encoding
//! used by `Order.receiver` and `Order.dstToken`.
//!
//! EVM chains use their EIP-155 chain id. Non-EVM chains use
//! `uint256(keccak256(CAIP-2 id))`, which can never collide with a real EVM id
//! (those fit in 64 bits; a keccak output does so with probability 2^-192).

use alloy::primitives::{Address, B256, U256, keccak256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet4,
    Signet,
    Regtest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolanaCluster {
    Mainnet,
    Devnet,
    Localnet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainKind {
    Evm(u64),
    Bitcoin(BitcoinNetwork),
    Solana(SolanaCluster),
}

impl ChainKind {
    /// CAIP-2 identifier (chain-agnostic `namespace:reference`).
    pub fn caip2(&self) -> String {
        match self {
            ChainKind::Evm(id) => format!("eip155:{id}"),
            // bip122 references are the first 32 hex chars of the genesis block hash
            ChainKind::Bitcoin(n) => format!(
                "bip122:{}",
                match n {
                    BitcoinNetwork::Mainnet => "000000000019d6689c085ae165831e93",
                    BitcoinNetwork::Testnet4 => "00000000da84f2bafbbc53dee25a72ae",
                    BitcoinNetwork::Signet => "00000008819873e925422c1ff0f99f7c",
                    BitcoinNetwork::Regtest => "0f9188f13cb7b2c71f2a335e3a4fc328",
                }
            ),
            // solana references are the first 32 chars of the base58 genesis hash
            ChainKind::Solana(c) => format!(
                "solana:{}",
                match c {
                    SolanaCluster::Mainnet => "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                    SolanaCluster::Devnet => "EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
                    SolanaCluster::Localnet => "localnet",
                }
            ),
        }
    }

    /// Value to put in `Order.dstChainId`.
    pub fn chain_id(&self) -> U256 {
        match self {
            ChainKind::Evm(id) => U256::from(*id),
            other => U256::from_be_bytes(keccak256(other.caip2()).0),
        }
    }

    /// Inverse of [`chain_id`](Self::chain_id) for the chains we know about.
    pub fn from_chain_id(id: U256) -> Option<Self> {
        if id < U256::from(u64::MAX) {
            return Some(ChainKind::Evm(id.to::<u64>()));
        }
        use BitcoinNetwork as B;
        use SolanaCluster as S;
        [
            ChainKind::Bitcoin(B::Mainnet),
            ChainKind::Bitcoin(B::Testnet4),
            ChainKind::Bitcoin(B::Signet),
            ChainKind::Bitcoin(B::Regtest),
            ChainKind::Solana(S::Mainnet),
            ChainKind::Solana(S::Devnet),
            ChainKind::Solana(S::Localnet),
        ]
        .into_iter()
        .find(|k| k.chain_id() == id)
    }
}

/// EVM address as a 32-byte `receiver` / `dstToken` (left-padded).
pub fn evm_to_word(a: Address) -> B256 {
    a.into_word()
}

/// The EVM address in a 32-byte word, if its top 12 bytes are zero.
pub fn word_to_evm(w: B256) -> Option<Address> {
    w[..12].iter().all(|b| *b == 0).then(|| Address::from_word(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_do_not_collide_with_evm() {
        let kinds = [
            ChainKind::Evm(1),
            ChainKind::Evm(8453),
            ChainKind::Bitcoin(BitcoinNetwork::Mainnet),
            ChainKind::Bitcoin(BitcoinNetwork::Regtest),
            ChainKind::Solana(SolanaCluster::Mainnet),
            ChainKind::Solana(SolanaCluster::Localnet),
        ];
        for k in kinds {
            assert_eq!(ChainKind::from_chain_id(k.chain_id()), Some(k), "{}", k.caip2());
        }
        assert!(ChainKind::Bitcoin(BitcoinNetwork::Mainnet).chain_id() > U256::from(u64::MAX));
    }

    #[test]
    fn caip2_strings() {
        assert_eq!(ChainKind::Evm(1).caip2(), "eip155:1");
        assert_eq!(
            ChainKind::Bitcoin(BitcoinNetwork::Mainnet).caip2(),
            "bip122:000000000019d6689c085ae165831e93"
        );
    }

    #[test]
    fn evm_words() {
        let a = Address::repeat_byte(0xab);
        assert_eq!(word_to_evm(evm_to_word(a)), Some(a));
        assert_eq!(word_to_evm(B256::repeat_byte(1)), None, "a Solana key is not an EVM address");
    }
}
