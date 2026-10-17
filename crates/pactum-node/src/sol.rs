//! Solana destination leg. The program's `redeem` is permissionless and pays
//! the escrow's fixed redeemer, so it works like the EVM leg: the maker only
//! verifies the escrow and releases the secret; the resolver submits the
//! redeem that pays the maker, so the maker needs no SOL for fees.

use alloy::primitives::{B256, U256};
use anyhow::{bail, ensure};
use pactum_core::{
    chains::{ChainKind, SolanaCluster},
    timelock::TimelockPolicy,
};
use pactum_relay::LockAnnouncement;
use pactum_sol::{Escrow, EscrowState, HtlcProgram, Keypair, Pubkey};
use serde::{Deserialize, Serialize};

pub use pactum_sol;

#[derive(Clone)]
pub struct SolLeg {
    pub cluster: SolanaCluster,
    pub program: HtlcProgram,
    /// resolver: funds escrows and pays fees; maker: the receiving key
    pub key: Keypair,
}

/// A resolver's escrow, identified by the swap id it was opened under.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolLock {
    pub swap_id: B256,
    pub redeemer: B256,
    pub expiry: i64,
}

impl SolLeg {
    pub fn kind(&self) -> ChainKind {
        ChainKind::Solana(self.cluster)
    }

    pub fn handles(&self, dst_chain_id: U256) -> bool {
        self.kind().chain_id() == dst_chain_id
    }

    /// This key as an `Order.receiver` word.
    pub fn receiver_word(&self) -> B256 {
        B256::from(self.key.pubkey().0)
    }

    pub async fn now(&self) -> anyhow::Result<i64> {
        self.program.rpc.chain_time().await
    }

    // ------------------------------------------------------------------ resolver

    pub async fn lock(
        &self,
        swap_id: B256,
        receiver: B256,
        secret_hash: B256,
        lamports: u64,
        expiry: i64,
    ) -> anyhow::Result<SolLock> {
        self.program
            .initiate(&self.key, swap_id.0, Pubkey(receiver.0), secret_hash.0, lamports, expiry)
            .await?;
        Ok(SolLock { swap_id, redeemer: receiver, expiry })
    }

    /// Pay the maker by revealing the secret (we pay the fee).
    pub async fn redeem_for(&self, lock: &SolLock, secret: [u8; 32]) -> anyhow::Result<()> {
        if self.state(lock).await? == Some(EscrowState::Open) {
            self.program.redeem(&self.key, lock.swap_id.0, Pubkey(lock.redeemer.0), secret).await?;
        }
        Ok(())
    }

    /// The secret if someone already redeemed the escrow.
    pub async fn revealed(&self, lock: &SolLock) -> anyhow::Result<Option<[u8; 32]>> {
        Ok(self.program.escrow(&lock.swap_id.0).await?.and_then(|e| e.secret))
    }

    pub async fn state(&self, lock: &SolLock) -> anyhow::Result<Option<EscrowState>> {
        Ok(self.program.escrow(&lock.swap_id.0).await?.map(|e| e.state))
    }

    pub async fn refund(&self, lock: &SolLock) -> anyhow::Result<()> {
        self.program.refund(&self.key, lock.swap_id.0, self.key.pubkey()).await?;
        Ok(())
    }

    // --------------------------------------------------------------------- maker

    /// Check an announced escrow on chain before releasing a secret: owned by
    /// the program, open, paying our key at least `min_lamports` behind
    /// `secret_hash`, expiring early enough before the source escrow, and late
    /// enough that the released secret can still be used on it.
    pub async fn verify(
        &self,
        ann: &LockAnnouncement,
        secret_hash: B256,
        min_lamports: u64,
        src_expiry: u64,
        now: u64,
        policy: &TimelockPolicy,
    ) -> anyhow::Result<Escrow> {
        let LockAnnouncement::Solana { swap_id, .. } = ann else {
            bail!("not a Solana lock");
        };
        let Some(e) = self.program.escrow(&swap_id.0).await? else {
            bail!("escrow does not exist");
        };
        ensure!(e.state == EscrowState::Open, "escrow already settled");
        ensure!(e.redeemer == self.key.pubkey(), "escrow pays someone else");
        ensure!(e.secret_hash == secret_hash.0, "escrow uses another secret");
        ensure!(e.amount >= min_lamports, "locked {} < owed {min_lamports}", e.amount);
        ensure!(
            policy.is_safe(u64::try_from(e.expiry)?, src_expiry),
            "escrow expires too close to the source escrow"
        );
        // a short escrow could be refunded by the resolver after we reveal
        ensure!(
            u64::try_from(e.expiry)? >= now + policy.min_follower_lock,
            "escrow expires too soon to use the secret safely"
        );
        Ok(e)
    }
}
