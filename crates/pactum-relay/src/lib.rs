//! Off-chain order relay.
//!
//! Makers post signed orders; resolvers poll for open orders and for secrets
//! the maker releases once a fill checks out. The relay is untrusted: it only
//! stores data anyone can verify (signatures, Merkle proofs, preimages), and it
//! never sees a secret before the maker chooses to release it.

mod client;
mod server;

pub use client::RelayClient;
pub use server::{Relay, router, serve};

use alloy::primitives::{Address, B256, Bytes};
use pactum_core::{Order, permit::SignedPermit};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedOrder {
    pub order: Order,
    pub signature: Bytes,
    pub src_chain_id: u64,
    pub contract: Address,
    /// Public leaf hashes so resolvers can build proofs. Secrets stay with the maker.
    pub secret_hashes: Vec<B256>,
    /// EIP-2612 permit for `srcToken`, so the maker never sends an approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit: Option<SignedPermit>,
}

impl SignedOrder {
    pub fn hash(&self) -> B256 {
        self.order.digest(self.src_chain_id, self.contract)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub hash: B256,
    #[serde(flatten)]
    pub signed: SignedOrder,
}

/// A resolver's counter-lock on a destination chain without EVM-style events
/// the maker could scan for. Purely advisory: the maker re-derives the lock
/// from these parameters and verifies it on-chain before releasing anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "chain", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LockAnnouncement {
    /// Taproot HTLC output funded by the resolver.
    Bitcoin {
        index: usize,
        txid: String,
        vout: u32,
        value_sats: u64,
        /// resolver's x-only refund key
        refunder: B256,
        /// absolute nLockTime of the refund leaf
        refund_after: u32,
    },
    /// Escrow of the Pactum Solana program, at the PDA `["escrow", swap_id]`.
    Solana { index: usize, swap_id: B256 },
}

impl LockAnnouncement {
    pub fn index(&self) -> usize {
        match self {
            LockAnnouncement::Bitcoin { index, .. } | LockAnnouncement::Solana { index, .. } => {
                *index
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecretRelease {
    pub index: usize,
    pub secret: Bytes,
    pub proof: Vec<B256>,
}
