use std::collections::HashMap;

use alloy::primitives::{B256, U256};
use tracing::{info, warn};

use crate::Chain;

/// Watches for expired, unsettled escrows that carry a safety deposit and
/// refunds them. The deposit is the keeper's reward; the initiator (usually a
/// maker whose resolver walked away) gets their funds back without paying gas.
pub struct Keeper {
    chains: Vec<Chain>,
    /// Smallest deposit worth spending a refund transaction on.
    min_reward: U256,
    cursors: HashMap<u64, u64>,
    /// chain id -> swap id -> expiry, for escrows we have not seen settle
    watching: HashMap<u64, HashMap<B256, u64>>,
    pub refunded: Vec<(u64, B256)>,
}

impl Keeper {
    pub fn new(chains: Vec<Chain>, min_reward: U256) -> Self {
        Self {
            chains,
            min_reward,
            cursors: HashMap::new(),
            watching: HashMap::new(),
            refunded: Vec::new(),
        }
    }

    /// Start scanning at `block` on every chain instead of the current head.
    pub fn from_block(mut self, block: u64) -> Self {
        for c in &self.chains {
            self.cursors.insert(c.id, block);
        }
        self
    }

    pub async fn tick(&mut self) -> anyhow::Result<()> {
        for chain in &self.chains {
            let head = chain.head().await?;
            let from = match self.cursors.get(&chain.id) {
                Some(c) => *c,
                None => head,
            };
            if head >= from {
                let watch = self.watching.entry(chain.id).or_default();
                for lock in chain.locks_from(from, head).await? {
                    watch.insert(lock.swap_id, lock.expiry);
                }
                self.cursors.insert(chain.id, head + 1);
            }

            let now = chain.now().await?;
            let Some(watch) = self.watching.get_mut(&chain.id) else { continue };
            let due: Vec<B256> =
                watch.iter().filter(|(_, exp)| now >= **exp).map(|(id, _)| *id).collect();

            for id in due {
                if chain.escrow(id).await?.settled {
                    watch.remove(&id);
                    continue;
                }
                let reward = chain.deposit(id).await?;
                if reward < self.min_reward || reward.is_zero() {
                    watch.remove(&id);
                    continue;
                }
                match chain.refund(id).await {
                    Ok(()) => {
                        info!(chain = chain.name, swap = %id, %reward, "refunded abandoned escrow");
                        self.refunded.push((chain.id, id));
                        watch.remove(&id);
                    }
                    // someone else may have beaten us to it; retry next tick
                    Err(e) => warn!(swap = %id, "refund failed: {e:#}"),
                }
            }
        }
        Ok(())
    }
}
