use std::collections::HashMap;

use alloy::primitives::{B256, U256};
use pactum_core::{
    Order, SecretTree,
    auction::{owed, secret_index},
    timelock::TimelockPolicy,
};
use pactum_relay::{OpenOrder, RelayClient};
use tracing::{debug, info, warn};

use crate::Chain;

/// A resolver's pricing strategy.
pub trait Bid: Send + Sync {
    /// Most destination tokens we pay for `amount` of the order's source asset,
    /// or `None` to skip the order.
    fn bid(&self, order: &Order, amount: U256) -> Option<U256>;

    /// Largest slice to take in one fill.
    fn max_fill(&self, order: &Order) -> U256 {
        order.srcAmount
    }
}

/// Fixed exchange rate with an optional per-fill slice and inventory cap.
#[derive(Clone, Debug)]
pub struct FixedRate {
    /// destination units paid per `rate_den` source units
    pub rate_num: U256,
    pub rate_den: U256,
    /// largest fill as basis points of the order
    pub max_fill_bps: u64,
}

impl Bid for FixedRate {
    fn bid(&self, _order: &Order, amount: U256) -> Option<U256> {
        Some(amount * self.rate_num / self.rate_den)
    }

    fn max_fill(&self, order: &Order) -> U256 {
        order.srcAmount * U256::from(self.max_fill_bps) / U256::from(10_000)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Locked,
    Settled,
    Refunded,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub order_hash: B256,
    pub index: usize,
    pub amount: U256,
    pub src_chain: u64,
    pub src_swap: B256,
    pub dst_chain: u64,
    pub dst_swap: B256,
    pub dst_expiry: u64,
    pub status: JobStatus,
}

/// Resolver for gasless intents.
///
/// Waits for the Dutch auction to reach a price it is willing to pay, fills
/// (possibly a slice), mirrors the lock on the destination chain, then
/// completes both legs once the maker releases the secret: redeeming on the
/// destination pays the maker, redeeming on the source pays us.
pub struct IntentFiller<B> {
    chains: HashMap<u64, Chain>,
    relay: RelayClient,
    bid: B,
    policy: TimelockPolicy,
    /// Source units we may have outstanding across all jobs.
    capacity: Option<U256>,
    pub jobs: Vec<Job>,
}

impl<B: Bid> IntentFiller<B> {
    pub fn new(chains: Vec<Chain>, relay: RelayClient, bid: B) -> Self {
        Self {
            chains: chains.into_iter().map(|c| (c.id, c)).collect(),
            relay,
            bid,
            policy: TimelockPolicy::default(),
            capacity: None,
            jobs: Vec::new(),
        }
    }

    pub fn with_capacity(mut self, capacity: U256) -> Self {
        self.capacity = Some(capacity);
        self
    }

    pub fn with_policy(mut self, policy: TimelockPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub async fn tick(&mut self) -> anyhow::Result<()> {
        for o in self.relay.orders().await? {
            if let Err(e) = self.try_fill(&o).await {
                debug!(order = %o.hash, "not filled: {e:#}");
            }
        }
        self.settle().await
    }

    fn committed(&self) -> U256 {
        self.jobs.iter().filter(|j| j.status == JobStatus::Locked).map(|j| j.amount).sum()
    }

    async fn try_fill(&mut self, o: &OpenOrder) -> anyhow::Result<()> {
        let order = &o.signed.order;
        let (Some(src), Some(dst)) = (
            self.chains.get(&o.signed.src_chain_id),
            self.chains.get(&order.dstChainId.to::<u64>()),
        ) else {
            return Ok(());
        };

        let now = src.now().await?;
        if now > order.deadline {
            return Ok(());
        }
        let (filled, next_index) = src.progress(o.hash).await?;
        let remaining = order.srcAmount - filled;
        if remaining.is_zero() {
            return Ok(());
        }

        let mut amount = self.bid.max_fill(order).min(remaining);
        let mut index = secret_index(order.srcAmount, order.parts, filled + amount);
        // a slice inside an already-used bucket would reuse a public secret;
        // the contract rejects it, so take the rest instead
        if index.as_ref().map_or(true, |i| *i < next_index) {
            amount = remaining;
            index = secret_index(order.srcAmount, order.parts, filled + amount);
        }
        let index = index?;

        if let Some(cap) = self.capacity {
            if self.committed() + amount > cap {
                return Ok(());
            }
        }
        let Some(bid) = self.bid.bid(order, amount) else { return Ok(()) };
        // small buffer for the block that lands the fill
        if owed(order, amount, now + 15) > bid {
            return Ok(());
        }

        let hashes = &o.signed.secret_hashes;
        let tree = SecretTree::new(hashes.clone());
        anyhow::ensure!(tree.root() == order.secretsRoot, "secret hashes do not match root");
        let secret_hash = *hashes.get(index).ok_or_else(|| anyhow::anyhow!("no hash {index}"))?;

        let fill = src
            .fill(order, o.signed.signature.clone(), amount, index, secret_hash, tree.proof(index))
            .await?;
        let src_expiry = src.escrow(fill.swap_id).await?.expiry;
        let Some(dst_expiry) = self.policy.follower_expiry(src_expiry, dst.now().await?) else {
            // src escrow simply refunds to the maker later
            warn!(order = %o.hash, "source lock too short for a safe counter-lock");
            return Ok(());
        };

        dst.ensure_allowance(order.dstToken, fill.owed).await?;
        let dst_swap = dst
            .initiate(order.receiver, order.dstToken, fill.owed, secret_hash, dst_expiry, fill.swap_id)
            .await?;

        info!(order = %o.hash, %amount, index, owed = %fill.owed, "filled");
        self.jobs.push(Job {
            order_hash: o.hash,
            index,
            amount,
            src_chain: src.id,
            src_swap: fill.swap_id,
            dst_chain: dst.id,
            dst_swap,
            dst_expiry,
            status: JobStatus::Locked,
        });
        Ok(())
    }

    async fn settle(&mut self) -> anyhow::Result<()> {
        for job in self.jobs.iter_mut().filter(|j| j.status == JobStatus::Locked) {
            let src = &self.chains[&job.src_chain];
            let dst = &self.chains[&job.dst_chain];

            if let Some(secret) = self.relay.secret(job.order_hash, job.index).await? {
                // pay the maker first (gasless for them), then ourselves
                if !dst.escrow(job.dst_swap).await?.settled {
                    dst.redeem(job.dst_swap, secret.clone()).await?;
                }
                if !src.escrow(job.src_swap).await?.settled {
                    src.redeem(job.src_swap, secret).await?;
                }
                job.status = JobStatus::Settled;
                info!(swap = %job.src_swap, "settled");
            } else if dst.now().await? >= job.dst_expiry && !dst.escrow(job.dst_swap).await?.settled
            {
                dst.refund(job.dst_swap).await?;
                job.status = JobStatus::Refunded;
                info!(swap = %job.dst_swap, "refunded");
            }
        }
        Ok(())
    }
}
