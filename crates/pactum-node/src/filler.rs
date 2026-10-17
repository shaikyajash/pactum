use std::collections::HashMap;

use alloy::primitives::{B256, U256};
use pactum_core::{
    Order, SecretTree,
    auction::{owed, secret_index},
    chains::word_to_evm,
    timelock::TimelockPolicy,
};
use pactum_relay::{OpenOrder, RelayClient};
use tracing::{debug, info, warn};

use crate::{
    Chain,
    btc::{BtcLeg, BtcLock, btc_refund_after},
    sol::{SolLeg, SolLock},
    store::JsonStore,
};

/// Where a fill's counter-lock goes.
enum Dst<'a> {
    Evm {
        chain: &'a Chain,
        receiver: alloy::primitives::Address,
        token: alloy::primitives::Address,
    },
    Bitcoin(&'a BtcLeg),
    Solana(&'a SolLeg),
}

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

/// Fixed exchange rate with a per-fill slice limit.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JobStatus {
    Locked,
    Settled,
    Refunded,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    /// Bitcoin counter-lock, when the destination is Bitcoin (then
    /// `dst_chain`/`dst_swap` are unused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btc: Option<BtcLock>,
    /// Solana escrow, when the destination is Solana.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sol: Option<SolLock>,
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
    /// Total source units this filler will buy; refunded fills give budget back.
    budget: Option<U256>,
    store: Option<JsonStore<Vec<Job>>>,
    btc: Option<BtcLeg>,
    sol: Option<SolLeg>,
    pub jobs: Vec<Job>,
}

impl<B: Bid> IntentFiller<B> {
    pub fn new(chains: Vec<Chain>, relay: RelayClient, bid: B) -> Self {
        Self {
            chains: chains.into_iter().map(|c| (c.id, c)).collect(),
            relay,
            bid,
            policy: TimelockPolicy::default(),
            budget: None,
            store: None,
            btc: None,
            sol: None,
            jobs: Vec::new(),
        }
    }

    pub fn with_budget(mut self, budget: U256) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Also fill orders whose destination is this Bitcoin network.
    pub fn with_bitcoin(mut self, leg: BtcLeg) -> Self {
        self.btc = Some(leg);
        self
    }

    /// Also fill orders whose destination is this Solana cluster.
    pub fn with_solana(mut self, leg: SolLeg) -> Self {
        self.sol = Some(leg);
        self
    }

    pub fn with_policy(mut self, policy: TimelockPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Persist jobs to `path`, resuming any saved there by a previous run so
    /// open escrows are still settled or refunded after a restart.
    pub fn with_store(mut self, path: impl Into<std::path::PathBuf>) -> anyhow::Result<Self> {
        let store = JsonStore::<Vec<Job>>::new(path);
        if let Some(jobs) = store.load()? {
            info!(count = jobs.len(), "resumed jobs");
            self.jobs = jobs;
        }
        self.store = Some(store);
        Ok(self)
    }

    fn persist(&self) -> anyhow::Result<()> {
        match &self.store {
            Some(s) => s.save(&self.jobs),
            None => Ok(()),
        }
    }

    pub async fn tick(&mut self) -> anyhow::Result<()> {
        for o in self.relay.orders().await? {
            if let Err(e) = self.try_fill(&o).await {
                debug!(order = %o.hash, "not filled: {e:#}");
            }
        }
        self.settle().await?;
        self.collect_deposits().await
    }

    /// Settling credits our deposits to HashLock's pull ledger; sweep them back.
    async fn collect_deposits(&self) -> anyhow::Result<()> {
        for chain in self.chains.values() {
            let got = chain.withdraw_deposits().await?;
            if !got.is_zero() {
                info!(chain = chain.name, amount = %got, "withdrew deposits");
            }
        }
        Ok(())
    }

    fn spent(&self) -> U256 {
        self.jobs.iter().filter(|j| j.status != JobStatus::Refunded).map(|j| j.amount).sum()
    }

    async fn try_fill(&mut self, o: &OpenOrder) -> anyhow::Result<()> {
        let order = &o.signed.order;
        let Some(src) = self.chains.get(&o.signed.src_chain_id) else { return Ok(()) };
        let Some(dst) = self.destination(order) else { return Ok(()) };

        let now = src.now().await?;
        if now > order.deadline {
            return Ok(());
        }
        // a Bitcoin counter-lock must still leave the maker a safe claim window
        // after the source escrow's timing is accounted for
        if matches!(dst, Dst::Bitcoin(_)) {
            let refund_after = btc_refund_after(now + order.srcLock, self.policy.safety_window);
            if refund_after.is_none_or(|t| t < now + self.policy.min_follower_lock) {
                return Ok(());
            }
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

        if self.budget.is_some_and(|budget| self.spent() + amount > budget) {
            return Ok(());
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
            .fill(
                order,
                o.signed.signature.clone(),
                amount,
                index,
                secret_hash,
                tree.proof(index),
                o.signed.permit.as_ref(),
            )
            .await?;
        let src_expiry = src.escrow(fill.swap_id).await?.expiry;
        let mut job = Job {
            order_hash: o.hash,
            index,
            amount,
            src_chain: src.id,
            src_swap: fill.swap_id,
            dst_chain: 0,
            dst_swap: B256::ZERO,
            dst_expiry: 0,
            status: JobStatus::Locked,
            btc: None,
            sol: None,
        };

        match dst {
            Dst::Evm { chain, receiver, token } => {
                let Some(dst_expiry) = self.policy.follower_expiry(src_expiry, chain.now().await?)
                else {
                    // src escrow simply refunds to the maker later
                    warn!(order = %o.hash, "source lock too short for a safe counter-lock");
                    return Ok(());
                };
                chain.ensure_allowance(token, fill.owed).await?;
                job.dst_swap = chain
                    .initiate(receiver, token, fill.owed, secret_hash, dst_expiry, fill.swap_id)
                    .await?;
                job.dst_chain = chain.id;
                job.dst_expiry = dst_expiry;
            }
            Dst::Bitcoin(btc) => {
                let refund_after = btc_refund_after(src_expiry, self.policy.safety_window)
                    .ok_or_else(|| anyhow::anyhow!("source lock too short"))?;
                let sats = u64::try_from(fill.owed)?;
                let lock = btc.lock(order.receiver, secret_hash, sats, refund_after).await?;
                self.relay.announce(o.hash, &btc.announcement(index, &lock)).await?;
                job.dst_expiry = refund_after;
                job.btc = Some(lock);
            }
            Dst::Solana(sol) => {
                let now = u64::try_from(sol.now().await?)?;
                let Some(expiry) = self.policy.follower_expiry(src_expiry, now) else {
                    warn!(order = %o.hash, "source lock too short for a safe counter-lock");
                    return Ok(());
                };
                let lamports = u64::try_from(fill.owed)?;
                // the source swap id is unique per fill, so it names the escrow
                let lock = sol
                    .lock(
                        fill.swap_id,
                        order.receiver,
                        secret_hash,
                        lamports,
                        i64::try_from(expiry)?,
                    )
                    .await?;
                let ann = pactum_relay::LockAnnouncement::Solana { index, swap_id: fill.swap_id };
                self.relay.announce(o.hash, &ann).await?;
                job.dst_expiry = expiry;
                job.sol = Some(lock);
            }
        }

        info!(order = %o.hash, %amount, index, owed = %fill.owed, "filled");
        self.jobs.push(job);
        self.persist()
    }

    fn destination(&self, order: &Order) -> Option<Dst<'_>> {
        if let Some(btc) = self.btc.as_ref().filter(|b| b.handles(order.dstChainId)) {
            // only native BTC; token words are meaningless on Bitcoin
            return order.dstToken.is_zero().then_some(Dst::Bitcoin(btc));
        }
        if let Some(sol) = self.sol.as_ref().filter(|s| s.handles(order.dstChainId)) {
            // native SOL only for now
            return order.dstToken.is_zero().then_some(Dst::Solana(sol));
        }
        // EVM ids fit in u64; non-EVM ids are 256-bit hashes
        let chain = self.chains.get(&u64::try_from(order.dstChainId).ok()?)?;
        Some(Dst::Evm {
            chain,
            receiver: word_to_evm(order.receiver)?,
            token: word_to_evm(order.dstToken)?,
        })
    }

    async fn settle(&mut self) -> anyhow::Result<()> {
        let before: Vec<JobStatus> = self.jobs.iter().map(|j| j.status).collect();
        let result = self.settle_jobs().await;
        if self.jobs.iter().map(|j| j.status).ne(before) {
            self.persist()?;
        }
        result
    }

    async fn settle_jobs(&mut self) -> anyhow::Result<()> {
        for job in self.jobs.iter_mut().filter(|j| j.status == JobStatus::Locked) {
            let src = &self.chains[&job.src_chain];

            if let Some(lock) = &job.btc {
                let btc = self.btc.as_ref().ok_or_else(|| anyhow::anyhow!("no bitcoin leg"))?;
                // the maker claims the BTC itself, revealing the secret on Bitcoin
                let secret = match self.relay.secret(job.order_hash, job.index).await? {
                    Some(s) => Some(s),
                    None => btc.revealed(lock).await?.map(|s| s.to_vec().into()),
                };
                if let Some(secret) = secret {
                    if !src.escrow(job.src_swap).await?.settled {
                        src.redeem(job.src_swap, secret).await?;
                    }
                    job.status = JobStatus::Settled;
                    info!(swap = %job.src_swap, "settled against bitcoin");
                } else if btc.refundable(lock).await?
                    && btc.rpc.tx_out(lock.outpoint).await?.is_some()
                {
                    btc.refund(lock).await?;
                    job.status = JobStatus::Refunded;
                    info!(outpoint = %lock.outpoint, "refunded bitcoin lock");
                }
                continue;
            }

            if let Some(lock) = &job.sol {
                let sol = self.sol.as_ref().ok_or_else(|| anyhow::anyhow!("no solana leg"))?;
                let secret = match self.relay.secret(job.order_hash, job.index).await? {
                    Some(s) => Some(<[u8; 32]>::try_from(s.as_ref())?),
                    None => sol.revealed(lock).await?,
                };
                if let Some(secret) = secret {
                    // pay the maker first (we pay the Solana fee), then ourselves
                    sol.redeem_for(lock, secret).await?;
                    if !src.escrow(job.src_swap).await?.settled {
                        src.redeem(job.src_swap, secret.to_vec().into()).await?;
                    }
                    job.status = JobStatus::Settled;
                    info!(swap = %job.src_swap, "settled against solana");
                } else if u64::try_from(sol.now().await?)? >= job.dst_expiry
                    && sol.state(lock).await? == Some(pactum_sol::EscrowState::Open)
                {
                    sol.refund(lock).await?;
                    job.status = JobStatus::Refunded;
                    info!(swap = %lock.swap_id, "refunded solana escrow");
                }
                continue;
            }

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
