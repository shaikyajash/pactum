use std::collections::HashSet;

use alloy::{
    primitives::{Address, B256, U256},
    signers::{SignerSync, local::PrivateKeySigner},
};
use anyhow::ensure;
use pactum_core::{Order, OrderSecrets, timelock::TimelockPolicy};
use pactum_relay::{RelayClient, SecretRelease, SignedOrder};
use tracing::info;

use crate::Chain;

#[derive(Clone, Debug)]
pub struct IntentRequest {
    pub src_token: Address,
    pub src_amount: U256,
    pub dst_token: Address,
    /// Opening ask for the whole order; decays linearly to `end_dst_amount`.
    pub start_dst_amount: U256,
    pub end_dst_amount: U256,
    pub auction_duration: u64,
    /// How long the order may be filled.
    pub ttl: u64,
    /// Lock on each source escrow.
    pub src_lock: u64,
    pub parts: u16,
    pub receiver: Option<Address>,
}

pub struct OpenIntent {
    pub order: Order,
    pub hash: B256,
    pub secrets: OrderSecrets,
    released: HashSet<usize>,
    src_from: u64,
    dst_from: u64,
}

impl OpenIntent {
    pub fn released(&self) -> &HashSet<usize> {
        &self.released
    }
}

/// Maker side of an intent. Signs once, then for every fill checks that the
/// resolver locked enough on the destination chain before releasing that
/// fill's secret. The maker never sends a transaction.
pub struct Maker {
    pub src: Chain,
    pub dst: Chain,
    signer: PrivateKeySigner,
    relay: RelayClient,
    policy: TimelockPolicy,
}

impl Maker {
    pub fn new(src: Chain, dst: Chain, signer: PrivateKeySigner, relay: RelayClient) -> Self {
        Self { src, dst, signer, relay, policy: TimelockPolicy::default() }
    }

    pub fn with_policy(mut self, policy: TimelockPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub async fn open(&self, req: IntentRequest, nonce: U256) -> anyhow::Result<OpenIntent> {
        let now = self.src.now().await?;
        let secrets = OrderSecrets::random(req.parts);
        let maker = self.signer.address();
        let order = Order {
            maker,
            receiver: req.receiver.unwrap_or(maker),
            srcToken: req.src_token,
            srcAmount: req.src_amount,
            dstChainId: U256::from(self.dst.id),
            dstToken: req.dst_token,
            startDstAmount: req.start_dst_amount,
            endDstAmount: req.end_dst_amount,
            auctionStart: now,
            auctionEnd: now + req.auction_duration,
            deadline: now + req.ttl,
            srcLock: req.src_lock,
            parts: req.parts,
            secretsRoot: secrets.root(),
            nonce,
        };
        let digest = order.digest(self.src.id, self.src.contract);
        let signature = self.signer.sign_hash_sync(&digest)?;
        let signed = SignedOrder {
            order: order.clone(),
            signature: signature.as_bytes().into(),
            src_chain_id: self.src.id,
            contract: self.src.contract,
            secret_hashes: secrets.hashes(),
        };
        let hash = self.relay.post(&signed).await?;
        ensure!(hash == digest, "relay returned a different order hash");
        info!(%hash, "intent opened");

        Ok(OpenIntent {
            order,
            hash,
            secrets,
            released: HashSet::new(),
            src_from: self.src.head().await?,
            dst_from: self.dst.head().await?,
        })
    }

    /// Release secrets for fills whose destination lock checks out.
    pub async fn tick(&self, intent: &mut OpenIntent) -> anyhow::Result<()> {
        let fills = intent_fills(self, intent).await?;
        let locks =
            self.dst.locks_to(intent.order.receiver, intent.dst_from, self.dst.head().await?).await?;

        for f in fills {
            if intent.released.contains(&f.secret_index) {
                continue;
            }
            let Some(secret) = intent.secrets.secrets.get(f.secret_index) else { continue };
            let src_escrow = self.src.escrow(f.swap_id).await?;

            let lock = locks.iter().find(|l| {
                l.initiator == f.resolver
                    && l.token == intent.order.dstToken
                    && l.secret_hash == secret.hash
                    && l.amount >= f.owed
                    // we must be able to redeem on dst before either side can refund
                    && self.policy.is_safe(l.expiry, src_escrow.expiry)
            });
            // no valid counter-lock yet: keep the secret
            let Some(lock) = lock else { continue };
            if self.dst.escrow(lock.swap_id).await?.settled {
                continue;
            }

            let release = SecretRelease {
                index: f.secret_index,
                secret: secret.secret.clone(),
                proof: intent.secrets.tree.proof(f.secret_index),
            };
            self.relay.release(intent.hash, &release).await?;
            intent.released.insert(f.secret_index);
            info!(order = %intent.hash, index = f.secret_index, lock = %lock.swap_id, "secret released");
        }
        Ok(())
    }
}

async fn intent_fills(m: &Maker, intent: &OpenIntent) -> anyhow::Result<Vec<crate::chain::Filled>> {
    m.src.fills_of(intent.hash, intent.src_from, m.src.head().await?).await
}
