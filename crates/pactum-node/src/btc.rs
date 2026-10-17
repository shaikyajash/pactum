//! Bitcoin destination leg: a resolver funds a Taproot HTLC paying the maker's
//! x-only key; the maker verifies it on-chain and claims it, which reveals the
//! secret in the witness for the resolver to use on the source chain.

use alloy::primitives::{B256, U256};
use anyhow::{Context, bail, ensure};
use pactum_btc::{BitcoinRpc, Htlc, htlc::MTP_LAG_SECS};
use pactum_core::chains::{BitcoinNetwork, ChainKind};
use pactum_relay::LockAnnouncement;
use serde::{Deserialize, Serialize};

pub use pactum_btc::bitcoin;
use pactum_btc::bitcoin::{
    Address, Amount, Network, OutPoint, ScriptBuf, XOnlyPublicKey,
    absolute::LockTime,
    key::{Keypair, Secp256k1},
};

/// Flat fee for HTLC spends. Fine for regtest and quiet mainnet; a production
/// agent would estimate it (and bump with CPFP).
pub const SPEND_FEE: Amount = Amount::from_sat(2_000);

#[derive(Clone)]
pub struct BtcLeg {
    pub network: BitcoinNetwork,
    pub rpc: BitcoinRpc,
    /// wallet-scoped client used to fund HTLCs (resolver) or receive (maker)
    pub wallet: BitcoinRpc,
    pub key: Keypair,
    /// confirmations a funding output needs before the maker reveals a secret
    pub min_confirmations: u64,
}

/// A funded HTLC and enough context to settle it later.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtcLock {
    pub htlc: Htlc,
    pub outpoint: OutPoint,
    pub value: Amount,
    /// block height to start searching from for the spend
    pub funded_at: u64,
}

impl BtcLeg {
    pub fn kind(&self) -> ChainKind {
        ChainKind::Bitcoin(self.network)
    }

    pub fn btc_network(&self) -> Network {
        match self.network {
            BitcoinNetwork::Mainnet => Network::Bitcoin,
            BitcoinNetwork::Testnet4 => Network::Testnet4,
            BitcoinNetwork::Signet => Network::Signet,
            BitcoinNetwork::Regtest => Network::Regtest,
        }
    }

    pub fn x_only(&self) -> XOnlyPublicKey {
        self.key.x_only_public_key().0
    }

    /// This key as an `Order.receiver` word.
    pub fn receiver_word(&self) -> B256 {
        B256::from(self.x_only().serialize())
    }

    pub fn handles(&self, dst_chain_id: U256) -> bool {
        self.kind().chain_id() == dst_chain_id
    }

    async fn own_address(&self) -> anyhow::Result<ScriptBuf> {
        let a: String =
            self.wallet.call("getnewaddress", serde_json::json!(["", "bech32m"])).await?;
        Ok(a.parse::<Address<_>>()?.require_network(self.btc_network())?.script_pubkey())
    }

    // ------------------------------------------------------------------ resolver

    /// Lock `sats` for the maker behind `secret_hash`, refundable by us after
    /// `refund_after` (Unix time).
    pub async fn lock(
        &self,
        maker_key: B256,
        secret_hash: B256,
        sats: u64,
        refund_after: u64,
    ) -> anyhow::Result<BtcLock> {
        let htlc = Htlc {
            secret_hash: secret_hash.0,
            redeemer: XOnlyPublicKey::from_slice(maker_key.as_slice())
                .context("order receiver is not an x-only key")?,
            refunder: self.x_only(),
            refund_after: LockTime::from_time(u32::try_from(refund_after)?)
                .context("refund time out of range")?,
        };
        let address = htlc.address(&Secp256k1::new(), self.btc_network())?;
        let funded_at = self.rpc.block_count().await?;
        let value = Amount::from_sat(sats);
        let outpoint = self.wallet.send_to(&address, value).await?;
        Ok(BtcLock { htlc, outpoint, value, funded_at })
    }

    pub fn announcement(&self, index: usize, lock: &BtcLock) -> LockAnnouncement {
        LockAnnouncement::Bitcoin {
            index,
            txid: lock.outpoint.txid.to_string(),
            vout: lock.outpoint.vout,
            value_sats: lock.value.to_sat(),
            refunder: B256::from(lock.htlc.refunder.serialize()),
            refund_after: lock.htlc.refund_after.to_consensus_u32(),
        }
    }

    /// The secret, once the maker has claimed the HTLC (mined or in the mempool).
    pub async fn revealed(&self, lock: &BtcLock) -> anyhow::Result<Option<[u8; 32]>> {
        if self.rpc.tx_out(lock.outpoint).await?.is_some() {
            return Ok(None); // still unspent
        }
        let spend = self.rpc.find_spend(lock.outpoint, lock.funded_at).await?;
        Ok(spend.and_then(|tx| lock.htlc.extract_secret(&tx, lock.outpoint)))
    }

    /// True once the refund leaf is spendable (median-time-past passed the lock).
    pub async fn refundable(&self, lock: &BtcLock) -> anyhow::Result<bool> {
        let mtp = self.rpc.median_time().await?;
        Ok(mtp > lock.htlc.refund_after.to_consensus_u32() as u64)
    }

    pub async fn refund(&self, lock: &BtcLock) -> anyhow::Result<()> {
        let to = self.own_address().await?;
        let tx = lock.htlc.refund_tx(lock.outpoint, lock.value, to, SPEND_FEE, &self.key)?;
        self.rpc.send_raw(&tx).await?;
        Ok(())
    }

    // --------------------------------------------------------------------- maker

    /// Rebuild an announced lock and check it on-chain: it must pay our key
    /// behind `secret_hash`, hold at least `min_sats`, be confirmed, and leave
    /// us at least `min_window` seconds (beyond the MTP lag) to claim it.
    pub async fn verify(
        &self,
        ann: &LockAnnouncement,
        secret_hash: B256,
        min_sats: u64,
        now: u64,
        min_window: u64,
    ) -> anyhow::Result<BtcLock> {
        let LockAnnouncement::Bitcoin { txid, vout, refunder, refund_after, .. } = ann else {
            bail!("not a Bitcoin lock");
        };
        let htlc = Htlc {
            secret_hash: secret_hash.0,
            redeemer: self.x_only(),
            refunder: XOnlyPublicKey::from_slice(refunder.as_slice())?,
            refund_after: LockTime::from_consensus(*refund_after),
        };
        ensure!(htlc.refund_after.is_block_time(), "refund lock must be a timestamp");
        ensure!(
            (*refund_after as u64) >= now + min_window,
            "refund opens too soon for a safe claim"
        );

        let outpoint = OutPoint { txid: txid.parse()?, vout: *vout };
        let Some((value, confs)) = self.rpc.tx_out(outpoint).await? else {
            bail!("announced output is missing or spent");
        };
        // the output must actually be locked by *this* HTLC
        let tx = self.rpc.raw_tx(outpoint.txid).await?;
        let out = tx.output.get(*vout as usize).context("no such vout")?;
        ensure!(
            out.script_pubkey == htlc.script_pubkey(&Secp256k1::new())?,
            "output is not the expected HTLC"
        );
        ensure!(value.to_sat() >= min_sats, "locked {} < owed {min_sats}", value.to_sat());
        ensure!(confs >= self.min_confirmations, "only {confs} confirmations");
        let funded_at = self.rpc.block_count().await?.saturating_sub(confs);
        Ok(BtcLock { htlc, outpoint, value, funded_at })
    }

    /// Claim a verified lock to our wallet. Fees come out of the HTLC value,
    /// so the maker needs no BTC beforehand.
    pub async fn claim(&self, lock: &BtcLock, secret: &[u8]) -> anyhow::Result<()> {
        let to = self.own_address().await?;
        let tx =
            lock.htlc.redeem_tx(lock.outpoint, lock.value, to, SPEND_FEE, secret, &self.key)?;
        self.rpc.send_raw(&tx).await?;
        Ok(())
    }
}

/// Latest refund time for a BTC counter-lock paired with a source escrow
/// expiring at `src_expiry`: the resolver must be able to learn the secret
/// and redeem the source leg after the BTC refund opens, allowing for the
/// median-time-past lag.
pub fn btc_refund_after(src_expiry: u64, safety_window: u64) -> Option<u64> {
    src_expiry.checked_sub(safety_window)?.checked_sub(MTP_LAG_SECS)
}
