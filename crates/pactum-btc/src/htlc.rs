//! Taproot HTLC.
//!
//! The output key tweaks the BIP-341 "nothing up my sleeve" point, which has
//! no known private key, so the output can only be spent through one of two
//! script leaves:
//!
//! ```text
//! redeem:  OP_SIZE 32 OP_EQUALVERIFY OP_SHA256 <secret_hash> OP_EQUALVERIFY
//!          <redeemer> OP_CHECKSIG
//! refund:  <locktime> OP_CHECKLOCKTIMEVERIFY OP_DROP <refunder> OP_CHECKSIG
//! ```
//!
//! The refund lock is *absolute* and normally a Unix timestamp, so the Bitcoin
//! leg expires on the same clock as the EVM leg it pairs with. A block-count
//! lock would drift with block times: slow blocks could keep the BTC
//! unrefundable after the EVM escrow expired, letting the redeemer take both
//! legs. Bitcoin checks timestamps against median-time-past, which trails wall
//! time by about an hour; see [`MTP_LAG_SECS`].
//!
//! The redeem leaf requires the redeemer's signature as well as the secret.
//! Without it, anyone who saw the secret in the mempool could race the
//! redeemer with a transaction paying themselves. `OP_SIZE 32` pins the
//! preimage length, matching the 32-byte rule enforced on every other chain.

use bitcoin::{
    Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
    XOnlyPublicKey,
    absolute::LockTime,
    hashes::{Hash, sha256},
    key::{Keypair, Secp256k1},
    opcodes::all::{OP_CHECKSIG, OP_CLTV, OP_DROP, OP_EQUALVERIFY, OP_SHA256, OP_SIZE},
    script::Builder,
    secp256k1::{Message, Verification},
    sighash::{Prevouts, SighashCache, TapSighashType},
    taproot::{self, LeafVersion, TapLeafHash, TaprootBuilder, TaprootSpendInfo},
    transaction::Version,
};
use thiserror::Error;

/// Conservative bound on how far median-time-past trails wall-clock time.
/// A timestamp refund lock `T` becomes spendable at roughly `T + MTP_LAG_SECS`.
pub const MTP_LAG_SECS: u64 = 2 * 3600;

/// BIP-341 NUMS point `H`: lift_x(sha256(G)). Nobody knows its discrete log.
const NUMS: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

#[derive(Debug, Error)]
pub enum HtlcError {
    #[error("secret must be 32 bytes")]
    SecretLength,
    #[error("secret does not match the HTLC hash")]
    WrongSecret,
    #[error("fee {fee} leaves nothing to spend from {value}")]
    FeeTooHigh { fee: Amount, value: Amount },
    #[error("refund lock must be non-zero")]
    BadDelay,
    #[error("taproot: {0}")]
    Taproot(String),
}

/// Parameters fully determining an HTLC output.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Htlc {
    pub secret_hash: [u8; 32],
    /// Can spend with the secret.
    pub redeemer: XOnlyPublicKey,
    /// Can spend once `refund_after` has passed.
    pub refunder: XOnlyPublicKey,
    /// Absolute lock on the refund leaf: a Unix timestamp in production, a
    /// block height in tests.
    pub refund_after: LockTime,
}

impl Htlc {
    pub fn redeem_script(&self) -> ScriptBuf {
        Builder::new()
            .push_opcode(OP_SIZE)
            .push_int(32)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_SHA256)
            .push_slice(self.secret_hash)
            .push_opcode(OP_EQUALVERIFY)
            .push_x_only_key(&self.redeemer)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    pub fn refund_script(&self) -> ScriptBuf {
        Builder::new()
            .push_int(self.refund_after.to_consensus_u32() as i64)
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.refunder)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    pub fn spend_info<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
    ) -> Result<TaprootSpendInfo, HtlcError> {
        if self.refund_after == LockTime::ZERO {
            return Err(HtlcError::BadDelay);
        }
        let nums = XOnlyPublicKey::from_slice(&NUMS).expect("valid NUMS point");
        TaprootBuilder::new()
            .add_leaf(1, self.redeem_script())
            .and_then(|b| b.add_leaf(1, self.refund_script()))
            .map_err(|e| HtlcError::Taproot(e.to_string()))?
            .finalize(secp, nums)
            .map_err(|_| HtlcError::Taproot("unfinalizable tree".into()))
    }

    pub fn script_pubkey<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
    ) -> Result<ScriptBuf, HtlcError> {
        Ok(ScriptBuf::new_p2tr_tweaked(self.spend_info(secp)?.output_key()))
    }

    pub fn address<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
        network: Network,
    ) -> Result<Address, HtlcError> {
        Ok(Address::p2tr_tweaked(self.spend_info(secp)?.output_key(), network))
    }

    /// Spend the HTLC to `to` by revealing `secret`, signed by the redeemer.
    pub fn redeem_tx(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        fee: Amount,
        secret: &[u8],
        redeemer: &Keypair,
    ) -> Result<Transaction, HtlcError> {
        if secret.len() != 32 {
            return Err(HtlcError::SecretLength);
        }
        if sha256::Hash::hash(secret).to_byte_array() != self.secret_hash {
            return Err(HtlcError::WrongSecret);
        }
        let script = self.redeem_script();
        self.spend(funding, value, to, fee, LockTime::ZERO, script, redeemer, |sig| {
            vec![sig, secret.to_vec()]
        })
    }

    /// Spend the HTLC back to `to` once `refund_after` has passed, signed by the refunder.
    pub fn refund_tx(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        fee: Amount,
        refunder: &Keypair,
    ) -> Result<Transaction, HtlcError> {
        let script = self.refund_script();
        self.spend(funding, value, to, fee, self.refund_after, script, refunder, |sig| vec![sig])
    }

    #[allow(clippy::too_many_arguments)]
    fn spend(
        &self,
        funding: OutPoint,
        value: Amount,
        to: ScriptBuf,
        fee: Amount,
        lock_time: LockTime,
        leaf: ScriptBuf,
        key: &Keypair,
        stack: impl FnOnce(Vec<u8>) -> Vec<Vec<u8>>,
    ) -> Result<Transaction, HtlcError> {
        let out = value.checked_sub(fee).filter(|v| *v > Amount::ZERO);
        let out = out.ok_or(HtlcError::FeeTooHigh { fee, value })?;
        let secp = Secp256k1::new();
        let info = self.spend_info(&secp)?;
        let prevout =
            TxOut { value, script_pubkey: ScriptBuf::new_p2tr_tweaked(info.output_key()) };

        let mut tx = Transaction {
            version: Version::TWO,
            lock_time,
            // a non-final sequence makes nLockTime (and so OP_CLTV) enforceable
            input: vec![TxIn {
                previous_output: funding,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                ..Default::default()
            }],
            output: vec![TxOut { value: out, script_pubkey: to }],
        };

        let leaf_hash = TapLeafHash::from_script(&leaf, LeafVersion::TapScript);
        let sighash = SighashCache::new(&tx)
            .taproot_script_spend_signature_hash(
                0,
                &Prevouts::All(&[prevout]),
                leaf_hash,
                TapSighashType::Default,
            )
            .map_err(|e| HtlcError::Taproot(e.to_string()))?;
        let msg = Message::from_digest(sighash.to_byte_array());
        let sig = taproot::Signature {
            signature: secp.sign_schnorr(&msg, key),
            sighash_type: TapSighashType::Default,
        };

        let control = info
            .control_block(&(leaf.clone(), LeafVersion::TapScript))
            .ok_or_else(|| HtlcError::Taproot("leaf not in tree".into()))?;
        let mut witness = Witness::new();
        for item in stack(sig.to_vec()) {
            witness.push(item);
        }
        witness.push(leaf.as_bytes());
        witness.push(control.serialize());
        tx.input[0].witness = witness;
        Ok(tx)
    }

    /// The secret, if `tx` spends `funding` through the redeem leaf.
    ///
    /// This is how the resolver learns the secret once the maker has claimed
    /// the BTC: it is sitting in the witness, on-chain or in the mempool.
    pub fn extract_secret(&self, tx: &Transaction, funding: OutPoint) -> Option<[u8; 32]> {
        let input = tx.input.iter().find(|i| i.previous_output == funding)?;
        // redeem witness: [signature, secret, script, control block]
        if input.witness.len() != 4 || input.witness.nth(2)? != self.redeem_script().as_bytes() {
            return None;
        }
        let secret: [u8; 32] = input.witness.nth(1)?.try_into().ok()?;
        (sha256::Hash::hash(&secret).to_byte_array() == self.secret_hash).then_some(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Txid, secp256k1::rand::thread_rng};

    fn fixture() -> (Htlc, Keypair, Keypair, [u8; 32]) {
        let secp = Secp256k1::new();
        let redeemer = Keypair::new(&secp, &mut thread_rng());
        let refunder = Keypair::new(&secp, &mut thread_rng());
        let secret = [7u8; 32];
        let htlc = Htlc {
            secret_hash: sha256::Hash::hash(&secret).to_byte_array(),
            redeemer: redeemer.x_only_public_key().0,
            refunder: refunder.x_only_public_key().0,
            refund_after: LockTime::from_time(1_900_000_000).unwrap(),
        };
        (htlc, redeemer, refunder, secret)
    }

    fn funding() -> OutPoint {
        OutPoint { txid: Txid::all_zeros(), vout: 0 }
    }

    #[test]
    fn nums_point_is_bip341_h() {
        // H = lift_x(sha256(uncompressed G)), as specified in BIP-341
        let g = bitcoin::secp256k1::constants::GENERATOR_X;
        let gy = bitcoin::secp256k1::constants::GENERATOR_Y;
        let mut uncompressed = vec![0x04];
        uncompressed.extend_from_slice(&g);
        uncompressed.extend_from_slice(&gy);
        assert_eq!(sha256::Hash::hash(&uncompressed).to_byte_array(), NUMS);
        assert!(XOnlyPublicKey::from_slice(&NUMS).is_ok());
    }

    #[test]
    fn redeem_leaf_layout() {
        let (h, ..) = fixture();
        let asm = h.redeem_script().to_asm_string();
        assert!(asm.starts_with("OP_SIZE OP_PUSHBYTES_1 20 OP_EQUALVERIFY OP_SHA256"), "{asm}");
        assert!(asm.ends_with("OP_CHECKSIG"));
        let asm = h.refund_script().to_asm_string();
        assert!(asm.contains("OP_CLTV OP_DROP"), "{asm}");
    }

    #[test]
    fn address_is_deterministic_and_taproot() {
        let (h, ..) = fixture();
        let secp = Secp256k1::new();
        let a = h.address(&secp, Network::Regtest).unwrap();
        assert_eq!(a, h.address(&secp, Network::Regtest).unwrap());
        assert!(a.to_string().starts_with("bcrt1p"));
        let other = Htlc { refund_after: LockTime::from_time(1_900_000_001).unwrap(), ..h };
        assert_ne!(a, other.address(&secp, Network::Regtest).unwrap());
    }

    #[test]
    fn redeem_rejects_bad_secrets() {
        let (h, redeemer, ..) = fixture();
        let to = ScriptBuf::new();
        let v = Amount::from_sat(100_000);
        let fee = Amount::from_sat(500);
        assert!(matches!(
            h.redeem_tx(funding(), v, to.clone(), fee, &[7u8; 31], &redeemer),
            Err(HtlcError::SecretLength)
        ));
        assert!(matches!(
            h.redeem_tx(funding(), v, to, fee, &[8u8; 32], &redeemer),
            Err(HtlcError::WrongSecret)
        ));
    }

    #[test]
    fn secret_round_trips_through_witness() {
        let (h, redeemer, refunder, secret) = fixture();
        let to = ScriptBuf::new();
        let v = Amount::from_sat(100_000);
        let fee = Amount::from_sat(500);
        let tx = h.redeem_tx(funding(), v, to.clone(), fee, &secret, &redeemer).unwrap();
        assert_eq!(tx.output[0].value, Amount::from_sat(99_500));
        assert_eq!(h.extract_secret(&tx, funding()), Some(secret));

        // a refund reveals nothing
        let refund = h.refund_tx(funding(), v, to, fee, &refunder).unwrap();
        assert_eq!(refund.lock_time, LockTime::from_time(1_900_000_000).unwrap());
        assert!(refund.input[0].sequence.enables_absolute_lock_time());
        assert_eq!(h.extract_secret(&refund, funding()), None);
    }

    #[test]
    fn fee_cannot_consume_the_output() {
        let (h, redeemer, _, secret) = fixture();
        let v = Amount::from_sat(1_000);
        assert!(matches!(
            h.redeem_tx(funding(), v, ScriptBuf::new(), v, &secret, &redeemer),
            Err(HtlcError::FeeTooHigh { .. })
        ));
    }
}
