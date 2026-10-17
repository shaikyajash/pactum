//! Keys, program-derived addresses and legacy transaction encoding, written
//! out from the Solana wire format so the workspace doesn't need the SDK.

use std::{fmt, str::FromStr};

use anyhow::{Context, bail};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Pubkey(pub [u8; 32]);

impl fmt::Display for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(self.0).into_string())
    }
}

impl fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pubkey({self})")
    }
}

impl FromStr for Pubkey {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let v = bs58::decode(s).into_vec().context("base58")?;
        Ok(Pubkey(v.try_into().map_err(|_| anyhow::anyhow!("pubkey must be 32 bytes"))?))
    }
}

/// The system program (all zero bytes).
pub const SYSTEM_PROGRAM: Pubkey = Pubkey([0; 32]);

pub struct Keypair(SigningKey);

impl Keypair {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("os randomness");
        Keypair(SigningKey::from_bytes(&seed))
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Keypair(SigningKey::from_bytes(&seed))
    }

    pub fn pubkey(&self) -> Pubkey {
        Pubkey(self.0.verifying_key().to_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.0.sign(msg).to_bytes()
    }
}

impl Clone for Keypair {
    fn clone(&self) -> Self {
        Keypair(SigningKey::from_bytes(&self.0.to_bytes()))
    }
}

fn on_curve(bytes: &[u8; 32]) -> bool {
    CompressedEdwardsY(*bytes).decompress().is_some()
}

/// `Pubkey::find_program_address`: the first bump (from 255 down) whose
/// address is *off* the ed25519 curve, so no private key can sign for it.
pub fn find_program_address(seeds: &[&[u8]], program: &Pubkey) -> (Pubkey, u8) {
    for bump in (0..=255u8).rev() {
        let mut h = Sha256::new();
        for s in seeds {
            h.update(s);
        }
        h.update([bump]);
        h.update(program.0);
        h.update(b"ProgramDerivedAddress");
        let addr: [u8; 32] = h.finalize().into();
        if !on_curve(&addr) {
            return (Pubkey(addr), bump);
        }
    }
    unreachable!("no viable bump")
}

#[derive(Clone, Debug)]
pub struct AccountMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl AccountMeta {
    pub fn new(pubkey: Pubkey, is_signer: bool, is_writable: bool) -> Self {
        Self { pubkey, is_signer, is_writable }
    }
}

#[derive(Clone, Debug)]
pub struct Instruction {
    pub program_id: Pubkey,
    pub accounts: Vec<AccountMeta>,
    pub data: Vec<u8>,
}

/// Solana's compact-u16 length prefix.
fn short_vec(len: usize, out: &mut Vec<u8>) {
    let mut v = len as u16;
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        b |= 0x80;
        out.push(b);
    }
}

/// Build and sign a legacy transaction. `payer` must be the first signer.
pub fn build_transaction(
    instructions: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
    recent_blockhash: [u8; 32],
) -> anyhow::Result<Vec<u8>> {
    // collect accounts: payer first, then merge flags per key
    let mut keys: Vec<(Pubkey, bool, bool)> = vec![(payer.pubkey(), true, true)];
    let mut upsert = |k: Pubkey, s: bool, w: bool| match keys.iter_mut().find(|e| e.0 == k) {
        Some(e) => {
            e.1 |= s;
            e.2 |= w;
        }
        None => keys.push((k, s, w)),
    };
    for ix in instructions {
        for a in &ix.accounts {
            upsert(a.pubkey, a.is_signer, a.is_writable);
        }
        upsert(ix.program_id, false, false);
    }
    // order: writable signers, readonly signers, writable, readonly (payer stays first)
    let payer_key = payer.pubkey();
    keys[1..].sort_by_key(|(_, s, w)| (!*s, !*w));
    debug_assert_eq!(keys[0].0, payer_key);

    let num_signers = keys.iter().filter(|k| k.1).count();
    let ro_signed = keys.iter().filter(|k| k.1 && !k.2).count();
    let ro_unsigned = keys.iter().filter(|k| !k.1 && !k.2).count();
    let index = |k: &Pubkey| keys.iter().position(|e| e.0 == *k).expect("collected") as u8;

    let mut msg = vec![num_signers as u8, ro_signed as u8, ro_unsigned as u8];
    short_vec(keys.len(), &mut msg);
    for (k, ..) in &keys {
        msg.extend_from_slice(&k.0);
    }
    msg.extend_from_slice(&recent_blockhash);
    short_vec(instructions.len(), &mut msg);
    for ix in instructions {
        msg.push(index(&ix.program_id));
        short_vec(ix.accounts.len(), &mut msg);
        for a in &ix.accounts {
            msg.push(index(&a.pubkey));
        }
        short_vec(ix.data.len(), &mut msg);
        msg.extend_from_slice(&ix.data);
    }

    let mut all: Vec<&Keypair> = vec![payer];
    all.extend(signers.iter().copied().filter(|s| s.pubkey() != payer_key));
    let mut tx = Vec::new();
    short_vec(num_signers, &mut tx);
    for (k, ..) in keys.iter().take(num_signers) {
        let Some(signer) = all.iter().find(|s| s.pubkey() == *k) else {
            bail!("missing signer {k}");
        };
        tx.extend_from_slice(&signer.sign(&msg));
    }
    tx.extend_from_slice(&msg);
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_vec_encoding() {
        let enc = |n| {
            let mut v = vec![];
            short_vec(n, &mut v);
            v
        };
        assert_eq!(enc(0), vec![0]);
        assert_eq!(enc(127), vec![0x7f]);
        assert_eq!(enc(128), vec![0x80, 0x01]);
        assert_eq!(enc(16384), vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn pda_is_off_curve_and_deterministic() {
        let program = Pubkey([7; 32]);
        let (a, bump) = find_program_address(&[b"escrow", &[1; 32]], &program);
        assert!(!on_curve(&a.0));
        assert_eq!(find_program_address(&[b"escrow", &[1; 32]], &program), (a, bump));
        assert_ne!(find_program_address(&[b"escrow", &[2; 32]], &program).0, a);
    }

    #[test]
    fn keypair_pubkeys_are_on_curve() {
        assert!(on_curve(&Keypair::generate().pubkey().0));
    }

    #[test]
    fn base58_round_trip() {
        let k = Keypair::generate().pubkey();
        assert_eq!(k.to_string().parse::<Pubkey>().unwrap(), k);
        assert_eq!(SYSTEM_PROGRAM.to_string(), "11111111111111111111111111111111");
    }

    #[test]
    fn transaction_layout() {
        let payer = Keypair::generate();
        let other = Keypair::generate();
        let prog = Pubkey([9; 32]);
        let ix = Instruction {
            program_id: prog,
            accounts: vec![
                AccountMeta::new(Pubkey([1; 32]), false, true),
                AccountMeta::new(other.pubkey(), true, false),
            ],
            data: vec![1, 2, 3],
        };
        let tx = build_transaction(&[ix], &payer, &[&other], [5; 32]).unwrap();
        assert_eq!(tx[0], 2, "two signatures");
        let msg = &tx[1 + 128..];
        assert_eq!(&msg[..3], &[2, 1, 1], "2 signers, 1 readonly signer, 1 readonly unsigned");
        // the payer's signature verifies over the message
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&payer.pubkey().0).unwrap();
        let sig = ed25519_dalek::Signature::from_bytes(&tx[1..65].try_into().unwrap());
        vk.verify_strict(msg, &sig).unwrap();
    }
}
