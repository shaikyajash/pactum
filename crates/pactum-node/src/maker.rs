use std::collections::HashSet;

use alloy::{
    primitives::{Address, B256, U256},
    signers::{SignerSync, local::PrivateKeySigner},
};
use anyhow::ensure;
use pactum_core::{
    Order, OrderSecrets, Secret,
    chains::{evm_to_word, word_to_evm},
    permit::{Permit, SignedPermit, TokenDomain},
    timelock::TimelockPolicy,
};
use pactum_relay::{RelayClient, SecretRelease, SignedOrder};
use tracing::info;

use crate::{Chain, store::JsonStore};

#[derive(Clone, Debug)]
pub struct IntentRequest {
    pub src_token: Address,
    pub src_amount: U256,
    /// Destination asset as a 32-byte word (see `pactum_core::chains`); zero = native.
    pub dst_token: B256,
    /// Opening ask for the whole order; decays linearly to `end_dst_amount`.
    pub start_dst_amount: U256,
    pub end_dst_amount: U256,
    pub auction_duration: u64,
    /// How long the order may be filled.
    pub ttl: u64,
    /// Lock on each source escrow.
    pub src_lock: u64,
    pub parts: u16,
    /// Destination recipient as a 32-byte word; defaults to the maker's EVM address.
    pub receiver: Option<B256>,
    /// Native deposit each fill must post; paid to whoever settles the escrow,
    /// so keepers are paid to refund us if a resolver walks away.
    pub min_deposit: U256,
    /// Sign an EIP-2612 permit with this domain version instead of relying on a
    /// prior `approve`. `None` means the allowance is already in place.
    pub permit_version: Option<String>,
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

/// On-disk form of an [`OpenIntent`]. Holds unrevealed secrets.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedIntent {
    order: Order,
    hash: B256,
    secrets: Vec<Secret>,
    released: Vec<usize>,
    src_from: u64,
    dst_from: u64,
}

impl From<&OpenIntent> for SavedIntent {
    fn from(i: &OpenIntent) -> Self {
        let mut released: Vec<usize> = i.released.iter().copied().collect();
        released.sort_unstable();
        Self {
            order: i.order.clone(),
            hash: i.hash,
            secrets: i.secrets.secrets.clone(),
            released,
            src_from: i.src_from,
            dst_from: i.dst_from,
        }
    }
}

impl From<SavedIntent> for OpenIntent {
    fn from(s: SavedIntent) -> Self {
        Self {
            order: s.order,
            hash: s.hash,
            secrets: OrderSecrets::from_secrets(s.secrets),
            released: s.released.into_iter().collect(),
            src_from: s.src_from,
            dst_from: s.dst_from,
        }
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
    store: Option<JsonStore<SavedIntent>>,
}

impl Maker {
    pub fn new(src: Chain, dst: Chain, signer: PrivateKeySigner, relay: RelayClient) -> Self {
        Self { src, dst, signer, relay, policy: TimelockPolicy::default(), store: None }
    }

    /// Persist the intent (including its secrets) to `path`.
    pub fn with_store(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.store = Some(JsonStore::new(path));
        self
    }

    /// Resume an intent saved by a previous run, if any.
    pub fn resume(&self) -> anyhow::Result<Option<OpenIntent>> {
        let Some(store) = &self.store else { return Ok(None) };
        Ok(store.load()?.map(|saved: SavedIntent| {
            let intent = OpenIntent::from(saved);
            info!(hash = %intent.hash, released = intent.released.len(), "resumed intent");
            intent
        }))
    }

    fn persist(&self, intent: &OpenIntent) -> anyhow::Result<()> {
        match &self.store {
            Some(s) => s.save(&SavedIntent::from(intent)),
            None => Ok(()),
        }
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
            receiver: req.receiver.unwrap_or(evm_to_word(maker)),
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
            minDeposit: req.min_deposit,
        };
        let digest = order.digest(self.src.id, self.src.contract);
        let signature = self.signer.sign_hash_sync(&digest)?;
        let permit = match &req.permit_version {
            Some(version) => Some(self.sign_permit(&order, version).await?),
            None => None,
        };
        let signed = SignedOrder {
            order: order.clone(),
            signature: signature.as_bytes().into(),
            src_chain_id: self.src.id,
            contract: self.src.contract,
            secret_hashes: secrets.hashes(),
            permit,
        };
        let intent = OpenIntent {
            order,
            hash: digest,
            secrets,
            released: HashSet::new(),
            src_from: self.src.head().await?,
            dst_from: self.dst.head().await?,
        };
        // save before posting: once the order is public it can be filled, and
        // losing the secrets then would strand every fill until it times out
        self.persist(&intent)?;

        let hash = self.relay.post(&signed).await?;
        ensure!(hash == digest, "relay returned a different order hash");
        info!(%hash, "intent opened");
        Ok(intent)
    }

    /// Permit the intents contract to pull the whole order, valid until the deadline.
    async fn sign_permit(&self, order: &Order, version: &str) -> anyhow::Result<SignedPermit> {
        let (name, nonce) = self.src.permit_info(order.srcToken, order.maker).await?;
        let permit = Permit {
            owner: order.maker,
            spender: self.src.contract,
            value: order.srcAmount,
            nonce,
            deadline: U256::from(order.deadline),
        };
        let domain = TokenDomain {
            name,
            version: version.to_owned(),
            chain_id: self.src.id,
            token: order.srcToken,
        };
        let sig = self.signer.sign_hash_sync(&permit.digest(&domain))?;
        Ok(SignedPermit {
            value: permit.value,
            deadline: permit.deadline,
            v: 27 + sig.v() as u8,
            r: sig.r().into(),
            s: sig.s().into(),
        })
    }

    /// Release secrets for fills whose destination lock checks out.
    pub async fn tick(&self, intent: &mut OpenIntent) -> anyhow::Result<()> {
        let fills = intent_fills(self, intent).await?;
        let (Some(receiver), Some(dst_token)) =
            (word_to_evm(intent.order.receiver), word_to_evm(intent.order.dstToken))
        else {
            anyhow::bail!("destination is not an EVM chain");
        };
        let locks = self.dst.locks_to(receiver, intent.dst_from, self.dst.head().await?).await?;

        for f in fills {
            if intent.released.contains(&f.secret_index) {
                continue;
            }
            let Some(secret) = intent.secrets.secrets.get(f.secret_index) else { continue };
            let src_escrow = self.src.escrow(f.swap_id).await?;

            let lock = locks.iter().find(|l| {
                l.initiator == f.resolver
                    && l.token == dst_token
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
            self.persist(intent)?;
            info!(order = %intent.hash, index = f.secret_index, lock = %lock.swap_id, "secret released");
        }
        Ok(())
    }
}

impl Maker {
    /// True once the order is fully filled and every fill's secret is released.
    /// Split orders may skip buckets, so this is not "all secrets released".
    pub async fn is_complete(&self, intent: &OpenIntent) -> anyhow::Result<bool> {
        let (filled, _) = self.src.progress(intent.hash).await?;
        if filled != intent.order.srcAmount {
            return Ok(false);
        }
        let fills = intent_fills(self, intent).await?;
        Ok(fills.iter().all(|f| intent.released.contains(&f.secret_index)))
    }
}

async fn intent_fills(m: &Maker, intent: &OpenIntent) -> anyhow::Result<Vec<crate::chain::Filled>> {
    m.src.fills_of(intent.hash, intent.src_from, m.src.head().await?).await
}
